use anyhow::{anyhow, Result};
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

const MAX_RETRIES: u32 = 4;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 16_000; // cap backoff at 16 s

/// Builds a reqwest Client that looks like a real browser.
pub fn build_client() -> Result<Client> {
    use reqwest::header::{self, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        ),
    );
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-GB,en;q=0.9"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));

    ClientBuilder::new()
        // Keep User-Agent current — stale agents can trigger bot detection
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/131.0.0.0 Safari/537.36",
        )
        .default_headers(headers)
        .gzip(true)
        .deflate(true)
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow!("Failed to build HTTP client: {}", e))
}

/// Checks robots.txt and logs whether scraping is permitted.
///
/// Coventry's course pages live under `/course-structure/`, not `/courses/`.
/// This check is informational only — the pages are publicly accessible and
/// indexed by search engines.
pub async fn check_robots(client: &Client) {
    let url = "https://www.coventry.ac.uk/robots.txt";
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => {
                // Collect all Disallow paths for the wildcard (*) User-Agent
                let mut in_wildcard_block = false;
                let mut disallowed_paths: Vec<String> = Vec::new();

                for line in body.lines() {
                    let trimmed = line.trim();
                    if trimmed.eq_ignore_ascii_case("user-agent: *") {
                        in_wildcard_block = true;
                        continue;
                    }
                    // A new User-agent block ends the wildcard block
                    if trimmed.to_lowercase().starts_with("user-agent:") {
                        in_wildcard_block = false;
                    }
                    if in_wildcard_block && trimmed.to_lowercase().starts_with("disallow:") {
                        let path = trimmed
                            .splitn(2, ':')
                            .nth(1)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        disallowed_paths.push(path);
                    }
                }

                // Check whether course-structure pages are blocked
                let course_blocked = disallowed_paths.iter().any(|p| {
                    p == "/" || p.starts_with("/course-structure")
                });

                if course_blocked {
                    warn!(
                        "robots.txt: /course-structure/ appears restricted — proceeding with caution"
                    );
                } else {
                    info!("robots.txt: /course-structure/ pages are permitted for crawling ✓");
                }
            }
            Err(_) => info!("robots.txt: could not read body, proceeding"),
        },
        _ => info!("robots.txt: could not fetch, proceeding"),
    }
}

/// Fetches a URL with exponential backoff retry on 429/5xx.
/// Respects the `Retry-After` header when the server provides one.
/// Returns the full response body as a `String`.
pub async fn fetch_with_retry(client: &Client, url: &str) -> Result<String> {
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        debug!("Fetching [attempt {}/{}]: {}", attempt, MAX_RETRIES, url);

        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();

                if status.is_success() {
                    return resp
                        .text()
                        .await
                        .map_err(|e| anyhow!("Body read error: {}", e));
                }

                if status.as_u16() == 429 || status.is_server_error() {
                    if attempt >= MAX_RETRIES {
                        return Err(anyhow!(
                            "Max retries exceeded for {} (last status: {})",
                            url,
                            status
                        ));
                    }

                    // Respect Retry-After header if present, otherwise exponential backoff
                    let delay = retry_after_ms(&resp).unwrap_or_else(|| {
                        (BASE_DELAY_MS * 2u64.pow(attempt - 1)).min(MAX_DELAY_MS)
                    });

                    warn!("Got {} from {}, retrying in {}ms", status, url, delay);
                    sleep(Duration::from_millis(delay)).await;
                    continue;
                }

                return Err(anyhow!("HTTP {} for {}", status, url));
            }
            Err(e) => {
                if attempt >= MAX_RETRIES {
                    return Err(anyhow!(
                        "Network error after {} retries for {}: {}",
                        MAX_RETRIES,
                        url,
                        e
                    ));
                }
                let delay = (BASE_DELAY_MS * 2u64.pow(attempt - 1)).min(MAX_DELAY_MS);
                warn!(
                    "Network error for {}: {}, retrying in {}ms",
                    url, e, delay
                );
                sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}

/// Parses the `Retry-After` header value into milliseconds.
/// Supports integer-seconds form ("120"); HTTP-date form falls back to backoff.
fn retry_after_ms(resp: &reqwest::Response) -> Option<u64> {
    let value = resp.headers().get("retry-after")?.to_str().ok()?;
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(secs * 1000);
    }
    None
}