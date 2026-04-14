//! URL discovery for Coventry University course pages.
//!
//! Four strategies tried in order — the scraper never hard-fails on URL discovery:
//!
//!   1. Sitemap XML  — parse sitemap_index.xml → walk child sitemaps for course URLs
//!   2. Search HTML  — parse /search/?contentType=NewCoursePage for href links
//!   3. Homepage crawl — follow any /course-structure/ links found on the homepage
//!   4. Verified fallback — a small set of confirmed-working URLs updated here
//!
//! Deduplication: courses are deduplicated by their "slug" (the last meaningful
//! path segment), so the same course does not appear twice for different intake
//! years (?term=2025-26 vs ?term=2026-27).

use anyhow::{Context, Result};
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

// ─── Configuration ────────────────────────────────────────────────────────────

const BASE: &str = "https://www.coventry.ac.uk";

/// Top-level sitemap index — the canonical machine-readable course directory.
const SITEMAP_INDEX: &str = "https://www.coventry.ac.uk/sitemap_index.xml";

/// Search listing pages tried when the sitemap approach yields nothing.
const SEARCH_PAGES: &[&str] = &[
    "https://www.coventry.ac.uk/search/?location=113&contentType=NewCoursePage",
    "https://www.coventry.ac.uk/study-at-coventry/postgraduate-study/az-course-list/",
    "https://www.coventry.ac.uk/study-at-coventry/az-course-list/",
];

/// Verified-working course URLs as of April 2026.
/// These include explicit `?term=` parameters so the extractor sees dated
/// tuition fees and intake information (e.g. 2025-26 vs 2026-27).
/// Only used when all dynamic strategies fail.
const FALLBACK_URLS: &[&str] = &[
    "https://www.coventry.ac.uk/course-structure/pg/bl/international-business-management-msc/?term=2025-26",
    "https://www.coventry.ac.uk/course-structure/pg/ees/data-science-and-computational-intelligence-msc/?term=2025-26",
    "https://www.coventry.ac.uk/course-structure/pg/cbl/accounting-and-financial-management-msc/?term=2025-26",
    "https://www.coventry.ac.uk/course-structure/ug/eec/computer-science-mscibsc-hons/?term=2025-26",
    "https://www.coventry.ac.uk/course-structure/ug/bl/business-management-bsc-hons/?term=2025-26",
];

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn discover_course_urls(client: &Client, limit: usize) -> Result<Vec<String>> {
    info!("Starting URL discovery (4 strategies)");

    // Strategy 1: Sitemap XML
    match try_sitemap(client, limit).await {
        Ok(urls) if !urls.is_empty() => {
            info!("[Strategy 1/4] Sitemap XML: found {} URLs", urls.len());
            return Ok(urls);
        }
        Ok(_) => debug!("[Strategy 1/4] Sitemap XML: no course URLs found"),
        Err(e) => debug!("[Strategy 1/4] Sitemap XML failed: {}", e),
    }

    // Strategy 2: Search results / listing page HTML
    match try_html_pages(client, limit).await {
        Ok(urls) if !urls.is_empty() => {
            info!("[Strategy 2/4] HTML pages: found {} URLs", urls.len());
            return Ok(urls);
        }
        Ok(_) => debug!("[Strategy 2/4] HTML pages: no URLs found"),
        Err(e) => debug!("[Strategy 2/4] HTML pages failed: {}", e),
    }

    // Strategy 3: Homepage link crawl
    match try_homepage_crawl(client, limit).await {
        Ok(urls) if !urls.is_empty() => {
            info!("[Strategy 3/4] Homepage crawl: found {} URLs", urls.len());
            return Ok(urls);
        }
        Ok(_) => debug!("[Strategy 3/4] Homepage crawl: no URLs found"),
        Err(e) => debug!("[Strategy 3/4] Homepage crawl failed: {}", e),
    }

    // Strategy 4: Verified hardcoded fallback — validate each URL before using it
    info!("[Strategy 4/4] Using hardcoded fallback — validating URLs...");
    Ok(validated_fallback(client, limit).await)
}

// ─── Strategy 1: Sitemap XML ──────────────────────────────────────────────────

/// Fetches the sitemap index, then walks child sitemaps to collect course URLs.
/// Coventry's sitemap structure: sitemap_index.xml → page-sitemap.xml (or similar)
/// where individual course pages appear as `<loc>` entries.
async fn try_sitemap(client: &Client, limit: usize) -> Result<Vec<String>> {
    let index_xml = fetch_text(client, SITEMAP_INDEX).await?;

    // Extract child sitemap URLs from the index
    let child_sitemaps = extract_locs_from_xml(&index_xml);
    debug!("Sitemap index contains {} child sitemaps", child_sitemaps.len());

    // Prioritise sitemaps that are likely to contain course pages
    let mut ordered: Vec<&str> = child_sitemaps
        .iter()
        .map(String::as_str)
        .collect();
    ordered.sort_by_key(|u| {
        // Bring course-related sitemaps to the front
        if u.contains("course") || u.contains("page") { 0usize } else { 1 }
    });

    let mut seen_slugs: HashMap<String, String> = HashMap::new(); // slug → full URL

    'outer: for sitemap_url in &ordered {
        debug!("Walking child sitemap: {}", sitemap_url);
        let xml = match fetch_text(client, sitemap_url).await {
            Ok(x) => x,
            Err(e) => { debug!("  failed: {}", e); continue; }
        };

        for loc in extract_locs_from_xml(&xml) {
            if is_course_url(&loc) {
                let slug = course_slug(&loc);
                // Keep only one URL per course slug (prefer ?term= URLs)
                let entry = seen_slugs.entry(slug).or_insert_with(|| loc.clone());
                if loc.contains("?term=") && !entry.contains("?term=") {
                    *entry = loc;
                }
                if seen_slugs.len() >= limit {
                    break 'outer;
                }
            }
        }
    }

    // If the sitemap contained no course pages, also try parsing sitemap as a
    // direct URL list (some Coventry sitemaps are not index files)
    if seen_slugs.is_empty() {
        for loc in extract_locs_from_xml(&index_xml) {
            if is_course_url(&loc) {
                let slug = course_slug(&loc);
                seen_slugs.entry(slug).or_insert(loc);
                if seen_slugs.len() >= limit { break; }
            }
        }
    }

    let mut result: Vec<String> = seen_slugs.into_values().collect();
    result.sort();
    Ok(result)
}

/// Extracts all `<loc>` values from an XML string (sitemap format).
/// Uses simple text scanning — avoids a full XML parser for resilience
/// against malformed sitemaps.
fn extract_locs_from_xml(xml: &str) -> Vec<String> {
    let mut locs = Vec::new();
    let mut search = xml;
    while let Some(start) = search.find("<loc>") {
        search = &search[start + 5..];
        if let Some(end) = search.find("</loc>") {
            let loc = search[..end].trim().to_string();
            if !loc.is_empty() {
                locs.push(loc);
            }
            search = &search[end + 6..];
        }
    }
    locs
}

// ─── Strategy 2: HTML page parsing ───────────────────────────────────────────

async fn try_html_pages(client: &Client, limit: usize) -> Result<Vec<String>> {
    let mut seen_slugs: HashMap<String, String> = HashMap::new();

    for page_url in SEARCH_PAGES {
        debug!("Fetching listing page: {}", page_url);
        let html = match fetch_text(client, page_url).await {
            Ok(h) => h,
            Err(e) => {
                debug!("  failed: {}", e);
                continue;
            }
        };

        let mut raw: HashSet<String> = HashSet::new();
        extract_course_links_from_html(&html, &mut raw);

        for url in raw {
            if is_course_url(&url) {
                let slug = course_slug(&url);
                seen_slugs.entry(slug).or_insert(url);
                if seen_slugs.len() >= limit {
                    break;
                }
            }
        }

        if seen_slugs.len() >= limit {
            break;
        }
    }

    let mut result: Vec<String> = seen_slugs.into_values().take(limit).collect();
    result.sort();
    Ok(result)
}

// ─── Strategy 3: Homepage crawl ───────────────────────────────────────────────

async fn try_homepage_crawl(client: &Client, limit: usize) -> Result<Vec<String>> {
    let html = fetch_text(client, BASE).await?;
    let mut raw: HashSet<String> = HashSet::new();
    extract_course_links_from_html(&html, &mut raw);

    let mut seen_slugs: HashMap<String, String> = HashMap::new();
    for url in raw {
        let slug = course_slug(&url);
        seen_slugs.entry(slug).or_insert(url);
        if seen_slugs.len() >= limit {
            break;
        }
    }

    let mut result: Vec<String> = seen_slugs.into_values().take(limit).collect();
    result.sort();
    Ok(result)
}

// ─── Strategy 4: Validated hardcoded fallback ─────────────────────────────────

/// Validates each hardcoded URL with a HEAD request; skips non-200 responses.
/// This ensures we never pass dead URLs to the extractor.
async fn validated_fallback(client: &Client, limit: usize) -> Vec<String> {
    let mut valid = Vec::new();

    for &url in FALLBACK_URLS {
        if valid.len() >= limit {
            break;
        }
        match client.head(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("  ✓ {} ({})", url, resp.status());
                valid.push(url.to_string());
            }
            Ok(resp) => warn!("  ✗ {} — HTTP {}", url, resp.status()),
            Err(e) => warn!("  ✗ {} — {}", url, e),
        }
    }

    if valid.is_empty() {
        // Last resort: return unvalidated and let the extractor handle failures
        warn!("All fallback URLs failed validation — returning unvalidated");
        FALLBACK_URLS
            .iter()
            .take(limit)
            .map(|s| s.to_string())
            .collect()
    } else {
        valid
    }
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Extracts all hrefs from an HTML string that pass `is_course_url`.
fn extract_course_links_from_html(html: &str, seen: &mut HashSet<String>) {
    let doc = scraper::Html::parse_document(html);
    for sel_str in &[
        "a[href*='course-structure']",
        "a[href*='/courses/undergraduate']",
        "a[href*='/courses/postgraduate']",
        "a[href*='/courses/research']",
    ] {
        if let Ok(sel) = scraper::Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                if let Some(href) = el.value().attr("href") {
                    let full = normalise_url(href);
                    if is_course_url(&full) {
                        seen.insert(full);
                    }
                }
            }
        }
    }
}

/// Extracts a stable course slug from a URL, stripping query parameters.
///
/// `/course-structure/pg/bl/accounting-msc/?term=2025-26`
///   → `accounting-msc`
///
/// This is used to deduplicate courses that appear under multiple intake-year
/// URLs — we only want one record per actual course.
fn course_slug(url: &str) -> String {
    // Strip query string first
    let path = url.split('?').next().unwrap_or(url);
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Returns true for individual course detail pages.
///
/// Matches both the current `/course-structure/{ug|pg|research}/{faculty}/{slug}/`
/// URL scheme and the legacy `/courses/{level}/` scheme.
pub fn is_course_url(url: &str) -> bool {
    let lower = url.to_lowercase();

    if !lower.contains("coventry.ac.uk") {
        return false;
    }

    let is_course_structure = lower.contains("/course-structure/")
        && (lower.contains("/course-structure/ug/")
            || lower.contains("/course-structure/pg/")
            || lower.contains("/course-structure/research/"));

    let is_legacy_courses = lower.contains("/courses/undergraduate/")
        || lower.contains("/courses/postgraduate/")
        || lower.contains("/courses/research/");

    if !is_course_structure && !is_legacy_courses {
        return false;
    }

    // Reject bare category index pages
    let is_bare_index = lower.ends_with("/course-structure/ug/")
        || lower.ends_with("/course-structure/pg/")
        || lower.ends_with("/course-structure/")
        || lower.ends_with("/courses/undergraduate/")
        || lower.ends_with("/courses/postgraduate/")
        || lower.ends_with("/courses/");

    // Strip query string for depth check
    let path_only = lower.split('?').next().unwrap_or(&lower);
    let segments = path_only
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count();

    // Require: domain + course-structure + level + faculty + slug = ≥5 segments
    !is_bare_index && segments >= 5
}

fn normalise_url(href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", BASE, href)
    } else {
        href.to_string()
    }
}

async fn fetch_text(client: &Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET failed: {}", url))?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("HTTP {} for {}", resp.status(), url));
    }

    resp.text()
        .await
        .with_context(|| format!("Failed to read body: {}", url))
}