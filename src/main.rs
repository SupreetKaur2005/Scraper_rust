mod extractor;
mod fetcher;
mod schema;
mod sitemap;

use anyhow::Result;
use std::fs;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

const TARGET_COURSES: usize = 5;
const OUTPUT_PATH: &str = "output/courses.json";
const POLITE_DELAY_MS: u64 = 1_000; // 1 s between requests — be a good citizen
const PER_COURSE_TIMEOUT_SECS: u64 = 45; // abort a single hung page after 45 s

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("coventry_scraper=info".parse().unwrap()),
        )
        .init();

    info!("=== Coventry University Course Scraper ===");
    info!("Target: {} courses → {}", TARGET_COURSES, OUTPUT_PATH);

    let client = fetcher::build_client()?;
    fetcher::check_robots(&client).await;

    // ── Step 1: URL discovery ─────────────────────────────────────────────────
    info!("--- Step 1: Discovering course URLs ---");
    let course_urls = sitemap::discover_course_urls(&client, TARGET_COURSES).await?;

    if course_urls.is_empty() {
        anyhow::bail!("No course URLs found — check network connection or site structure");
    }

    info!("Discovered {} course URLs:", course_urls.len());
    for (i, url) in course_urls.iter().enumerate() {
        info!("  [{}] {}", i + 1, url);
    }

    // ── Step 2: Extraction ────────────────────────────────────────────────────
    info!("--- Step 2: Extracting course data ---");
    let mut courses: Vec<schema::CourseData> = Vec::with_capacity(course_urls.len());

    for (i, url) in course_urls.iter().enumerate() {
        if i > 0 {
            sleep(Duration::from_millis(POLITE_DELAY_MS)).await;
        }

        // Per-course timeout — a single hung page will not stall the whole run
        let data = match timeout(
            Duration::from_secs(PER_COURSE_TIMEOUT_SECS),
            extractor::extract_course(&client, url),
        )
        .await
        {
            Ok(d) => d,
            Err(_) => {
                warn!(
                    "[{}/{}] Timeout after {}s scraping {} — recording as empty",
                    i + 1,
                    course_urls.len(),
                    PER_COURSE_TIMEOUT_SECS,
                    url
                );
                schema::CourseData::new_empty(url)
            }
        };

        if data.is_valid() {
            info!(
                "[{}/{}] ✓ {} — {}/{} fields filled",
                i + 1,
                course_urls.len(),
                data.program_course_name,
                data.fill_rate(),
                schema::CourseData::FIELD_COUNT,
            );
            courses.push(data);
        } else {
            warn!(
                "[{}/{}] Skipping {} — extraction yielded no course name",
                i + 1,
                course_urls.len(),
                url
            );
        }
    }

    // ── Step 3: Output ────────────────────────────────────────────────────────
    info!("--- Step 3: Writing output ---");
    fs::create_dir_all("output")?;
    let json = serde_json::to_string_pretty(&courses)?;
    fs::write(OUTPUT_PATH, &json)?;

    let total_fields = courses.len() * schema::CourseData::FIELD_COUNT;
    let filled: usize = courses.iter().map(|c| c.fill_rate()).sum();
    let na_total: usize = courses.iter().map(|c| c.na_count()).sum();

    info!("✓ Saved {} courses to {}", courses.len(), OUTPUT_PATH);
    info!(
        "  Fill rate : {}/{} fields ({:.0}%)",
        filled,
        total_fields,
        if total_fields > 0 {
            filled as f64 / total_fields as f64 * 100.0
        } else {
            0.0
        }
    );
    info!("  NA fields : {}", na_total);
    info!(
        "  Per-course: {}",
        courses
            .iter()
            .map(|c| format!("{} ({}/27)", c.program_course_name, c.fill_rate()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(())
}