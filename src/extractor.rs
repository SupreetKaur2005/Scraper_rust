use crate::fetcher::fetch_with_retry;
use crate::schema::{CourseData, NA};
use regex::Regex;
use reqwest::Client;
use scraper::{Element, Html, Selector};
use serde_json::Value;
use tracing::{debug, info, warn};

// Chromiumoxide is only compiled when the `js_render` feature is enabled.
#[cfg(feature = "js_render")]
use anyhow::Result;
#[cfg(feature = "js_render")]
use chromiumoxide::browser::{Browser, BrowserConfig};
#[cfg(feature = "js_render")]
use futures::StreamExt;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Fetches a course page and extracts all schema fields via a 3-layer pipeline:
///   Layer 1 — JSON-LD structured data (zero fragile selectors)
///   Layer 2 — HTML CSS selectors (fills gaps left by Layer 1)
///   Layer 3 — Headless Chrome (JS-rendered content, optional feature flag)
pub async fn extract_course(client: &Client, url: &str) -> CourseData {
    info!("Extracting: {}", url);
    let mut data = CourseData::new_empty(url);

    // Cheap win: derive study level directly from the URL path before any fetch
    if let Some(level) = study_level_from_url(url) {
        data.study_level = level;
    }

    match fetch_with_retry(client, url).await {
        Ok(html) => {
            let doc = Html::parse_document(&html);
            layer1_json_ld(&doc, &mut data);

            if data.study_level == NA {
                if let Some(level) = study_level_from_content(&doc) {
                    data.study_level = level;
                }
            }

            layer2_selectors(&doc, &mut data);
        }
        Err(e) => warn!("HTTP fetch failed for {}: {}", url, e),
    }

    // Layer 3 only when critical fields are still missing
    if needs_js_render(&data) {
        debug!("Critical fields still missing — trying Layer 3 for {}", url);
        #[cfg(feature = "js_render")]
        match layer3_chromium(url, &mut data).await {
            Ok(_) => {}
            Err(e) => warn!("Chromiumoxide failed for {}: {}", url, e),
        }
        #[cfg(not(feature = "js_render"))]
        debug!("js_render feature not enabled — skipping Layer 3");
    }

    data
}

// ─── Layer 1: JSON-LD ─────────────────────────────────────────────────────────

fn layer1_json_ld(doc: &Html, data: &mut CourseData) {
    let sel = Selector::parse("script[type='application/ld+json']").unwrap();

    for el in doc.select(&sel) {
        let json_text = el.text().collect::<String>();
        match serde_json::from_str::<Value>(&json_text) {
            Ok(v) => {
                // Handle both bare object and @graph array
                let objects: Vec<&Value> = if let Some(graph) = v.get("@graph") {
                    graph.as_array().map(|a| a.iter().collect()).unwrap_or_default()
                } else {
                    vec![&v]
                };
                for obj in objects {
                    let t = obj.get("@type").and_then(|t| t.as_str()).unwrap_or("");
                    if t.contains("Course") || t.contains("EducationalOccupationalProgram") {
                        parse_json_ld_course(obj, data);
                    }
                }
            }
            Err(e) => debug!("JSON-LD parse error: {}", e),
        }
    }
}

fn parse_json_ld_course(obj: &Value, data: &mut CourseData) {
    set_if_na(&mut data.program_course_name, || {
        obj.get("name").and_then(|v| v.as_str()).map(clean)
    });

    set_if_na(&mut data.study_level, || {
        obj.get("educationalLevel")
            .or_else(|| obj.get("educationalCredentialAwarded"))
            .and_then(|v| v.as_str())
            .map(clean)
    });

    set_if_na(&mut data.course_duration, || {
        obj.get("timeToComplete")
            .and_then(|v| v.as_str())
            .map(parse_iso_duration)
    });

    set_if_na(&mut data.yearly_tuition_fee, || {
        let offers = obj.get("offers")?;
        let offer = if offers.is_array() {
            offers.as_array()?.first()?
        } else {
            offers
        };
        let price = offer
            .get("price")
            .or_else(|| offer.get("priceSpecification"))
            .and_then(|v| v.as_str())?;
        let currency = offer
            .get("priceCurrency")
            .and_then(|v| v.as_str())
            .unwrap_or("GBP");
        Some(format!("{} {}", currency, price))
    });

    set_if_na(&mut data.campus, || {
        let loc = obj.get("location").or_else(|| obj.get("locationCreated"))?;
        let name = loc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| loc.as_str().unwrap_or(NA));
        Some(clean(name))
    });
}

// ─── Layer 2: HTML selectors ──────────────────────────────────────────────────

fn layer2_selectors(doc: &Html, data: &mut CourseData) {
    // Scope all text extraction to the main content area to avoid
    // false positives from nav, footer, and cookie banners
    let content_text = main_content_text(doc);
    let content_lower = content_text.to_lowercase();

    // ── Program name ──────────────────────────────────────────────────────────
    set_if_na(&mut data.program_course_name, || {
        first_text(
            doc,
            &[
                "h1.course-title",
                ".course-header h1",
                ".c-course-header__title",
                ".course-hero__title",
                "h1",
            ],
        )
    });

    // ── Study level ───────────────────────────────────────────────────────────
    set_if_na(&mut data.study_level, || {
        first_text(
            doc,
            &[
                // Coventry uses breadcrumb second item for level
                "nav[aria-label='breadcrumb'] li:nth-child(2)",
                ".breadcrumb li:nth-child(2)",
                "ol.breadcrumb li:nth-child(2)",
                ".c-breadcrumb__item:nth-child(2)",
                "[data-study-level]",
                ".course-level",
                ".study-level",
            ],
        )
    });

    // ── Course duration ───────────────────────────────────────────────────────
    set_if_na(&mut data.course_duration, || {
        let v = find_labeled_value(doc, &["Duration", "Course length", "Length", "Study mode"]);
        if v != NA { Some(v) } else { None }
    });

    // ── Tuition fee ───────────────────────────────────────────────────────────
    // Coventry uses a key-info strip with labels like "UK fee" / "International fee"
    set_if_na(&mut data.yearly_tuition_fee, || {
        // Try international fee first (most relevant for overseas applicants)
        for label in &[
            "International fee",
            "International tuition fee",
            "Tuition fees",
            "Annual tuition fee",
            "Fees",
            "Fee",
            "Tuition fee",
        ] {
            let v = find_labeled_value(doc, &[label]);
            if v != NA {
                return Some(v.replace("per year", "").replace("Per year", "").trim().to_string());
            }
        }
        // Fallback: look for a £ or GBP amount in the key info area
        extract_fee_from_content(doc)
    });

    // ── Intakes / start dates ─────────────────────────────────────────────────
    set_if_na(&mut data.all_intakes_available, || {
        let v = find_labeled_value(
            doc,
            &["Start date", "Start dates", "Entry point", "Entry", "Intake", "When can I start"],
        );
        if v != NA { Some(v) } else { None }
    });

    // ── Campus ────────────────────────────────────────────────────────────────
    set_if_na(&mut data.campus, || {
        let v = find_labeled_value(doc, &["Campus", "Location", "Study location", "Where"]);
        if v != NA { Some(v) } else { None }
    });

    // ── English proficiency scores ────────────────────────────────────────────
    // Uses regex patterns to extract numeric scores robustly.
    // Example patterns handled:
    //   "IELTS overall score of 6.5"  → "6.5"
    //   "minimum IELTS 6.0"           → "6.0"
    //   "PTE Academic 54"             → "54"
    //   "TOEFL iBT 79"                → "79"
    set_if_na(&mut data.min_ielts,   || extract_test_score(&content_text, &["IELTS"]));
    set_if_na(&mut data.min_pte,     || extract_test_score(&content_text, &["PTE Academic", "PTE"]));
    set_if_na(&mut data.min_toefl,   || extract_test_score(&content_text, &["TOEFL iBT", "TOEFL"]));
    set_if_na(&mut data.min_duolingo, || extract_test_score(&content_text, &["Duolingo English Test", "Duolingo", "DET"]));
    set_if_na(&mut data.kaplan_test_of_english, || extract_test_score(&content_text, &["Kaplan Test of English", "Kaplan"]));
    set_if_na(&mut data.gre_gmat_mandatory_min_score, || {
        // GRE/GMAT scores are rarely required at Coventry — check for explicit mention
        let has_gre  = content_lower.contains("gre") && content_lower.contains("required");
        let has_gmat = content_lower.contains("gmat") && content_lower.contains("required");
        if has_gre || has_gmat {
            extract_test_score(&content_text, &["GRE", "GMAT"])
        } else {
            None
        }
    });

    // ── Entry requirements / mandatory documents ──────────────────────────────
    // Coventry's entry requirements live in a tab panel — try multiple selectors
    set_if_na(&mut data.mandatory_documents_required, || {
        text_from_selectors(
            doc,
            &[
                // Coventry-specific tab/section IDs
                "#entry-requirements",
                "[data-tab='entry-requirements']",
                "#tab-entry-requirements",
                ".entry-requirements__content",
                // Generic fallbacks
                ".entry-requirements",
                ".requirements-section",
                ".c-entry-requirements",
            ],
            500,
        )
    });

    // ── Class 12 / A-level / IB accepted qualifications ──────────────────────
    set_if_na(&mut data.class_12_boards_accepted, || {
        let block = text_from_selectors(
            doc,
            &[
                "#entry-requirements",
                ".entry-requirements",
                ".entry-requirements__content",
                "#tab-entry-requirements",
            ],
            2000,
        )?;
        // Keep only sentences mentioning recognised pre-university qualifications
        let relevant: String = block
            .split('.')
            .filter(|s| {
                let l = s.to_lowercase();
                l.contains("a-level")
                    || l.contains("a level")
                    || l.contains("ib diploma")
                    || l.contains("international baccalaureate")
                    || l.contains("btec")
                    || l.contains("gcse")
                    || l.contains("12th")
                    || l.contains("high school")
                    || l.contains("secondary school")
                    || l.contains("qualification")
                    || l.contains("grade")
            })
            .collect::<Vec<_>>()
            .join(". ")
            .trim()
            .to_string();
        if relevant.is_empty() { None } else { Some(relevant) }
    });

    // ── UG academic grade requirement ─────────────────────────────────────────
    set_if_na(&mut data.ug_academic_min_gpa, || {
        let v = find_labeled_value(
            doc,
            &[
                "Degree classification",
                "Minimum grade",
                "GPA",
                "Academic requirement",
                "Honours degree",
            ],
        );
        if v != NA { Some(v) } else { None }
    });

    // ── 12th grade minimum ────────────────────────────────────────────────────
    set_if_na(&mut data.twelfth_pass_min_cgpa, || {
        let v = find_labeled_value(
            doc,
            &["A-level", "A level", "GCSE", "12th", "Twelfth", "High school grade"],
        );
        if v != NA { Some(v) } else { None }
    });

    // ── Work experience ───────────────────────────────────────────────────────
    set_if_na(&mut data.mandatory_work_exp, || {
        let v = find_labeled_value(
            doc,
            &[
                "Work experience",
                "Professional experience",
                "Industry experience",
                "Relevant experience",
            ],
        );
        if v != NA {
            return Some(v);
        }
        // Regex: "X year(s)' experience" / "X years of experience"
        let re = Regex::new(r"(?i)\d+\s+years?['\s]+(?:of\s+)?(?:relevant\s+)?(?:professional\s+)?experience")
            .unwrap();
        re.find(&content_text)
            .map(|m| m.as_str().trim().to_string())
    });

    // ── Max backlogs ──────────────────────────────────────────────────────────
    set_if_na(&mut data.max_backlogs, || {
        let v = find_labeled_value(doc, &["Backlog", "Backlogs", "Arrears", "Standing arrears"]);
        if v != NA { Some(v) } else { None }
    });

    // ── Scholarship ───────────────────────────────────────────────────────────
    set_if_na(&mut data.scholarship_availability, || {
        // Check for a dedicated scholarships section
        let from_sel = text_from_selectors(
            doc,
            &[
                ".scholarships",
                "#scholarships",
                "[data-section='scholarships']",
                ".funding-section",
                ".c-scholarships",
                "#funding",
            ],
            300,
        );
        if from_sel.is_some() {
            return from_sel;
        }
        // Check for a scholarships link or mention
        if let Ok(sel) = Selector::parse("a[href*='scholarship'], a[href*='bursary'], a[href*='funding']") {
            if doc.select(&sel).next().is_some() {
                return Some("Scholarships available (see course page)".to_string());
            }
        }
        if content_lower.contains("scholarship") || content_lower.contains("bursary") {
            Some("Scholarships available (see course page)".to_string())
        } else {
            None
        }
    });

    // ── English waivers ───────────────────────────────────────────────────────
    set_if_na(&mut data.english_waiver_moi, || {
        if content_lower.contains("medium of instruction")
            || content_lower.contains("english-taught")
            || content_lower.contains("taught in english")
            || content_lower.contains("english taught")
            || content_lower.contains("exempt if")
        {
            Some(
                "May be waived if previous study was conducted in English — see course page"
                    .to_string(),
            )
        } else {
            None
        }
    });

    set_if_na(&mut data.english_waiver_class12, || {
        if content_lower.contains("english at gcse")
            || content_lower.contains("english language gcse")
            || content_lower.contains("grade c in english")
            || content_lower.contains("grade 4 in english")
            || content_lower.contains("english gcse")
        {
            Some(
                "English at GCSE grade C/4 or equivalent may satisfy the requirement".to_string(),
            )
        } else {
            None
        }
    });

    // ── Gap year ──────────────────────────────────────────────────────────────
    set_if_na(&mut data.gap_year_max_accepted, || {
        if content_lower.contains("gap year") {
            let re = Regex::new(r"(?i)gap year[^.]*?(\d+)").unwrap();
            if let Some(cap) = re.captures(&content_text) {
                return Some(cap[1].to_string());
            }
            Some("Permitted — see entry requirements".to_string())
        } else {
            None
        }
    });

    // ── Indian regional institution restrictions ──────────────────────────────
    set_if_na(&mut data.indian_regional_institution_restrictions, || {
        if content_lower.contains("india") || content_lower.contains("indian") {
            let re = Regex::new(r"(?i)(?:india[^\.\n]*?(?:university|institution|college|board)[^\.\n]*\.)")
                .unwrap();
            re.find(&content_text)
                .map(|m| m.as_str().trim().to_string())
        } else {
            None
        }
    });
}

// ─── Layer 3: Headless Chrome (optional feature) ──────────────────────────────

#[cfg(feature = "js_render")]
async fn layer3_chromium(url: &str, data: &mut CourseData) -> Result<()> {
    info!("Layer 3: launching headless Chrome for {}", url);

    let Some(chrome_path) = find_chrome() else {
        warn!("Chrome not found — skipping Layer 3");
        return Ok(());
    };

    let config = BrowserConfig::builder()
        .chrome_executable(chrome_path)
        .no_sandbox()
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;

    let (mut browser, mut handler) = Browser::launch(config).await?;

    let handler_task = tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    let page = browser.new_page(url).await?;
    // Wait for JS-rendered content to settle
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let html = page.content().await?;
    browser.close().await?;
    handler_task.abort();

    let doc = Html::parse_document(&html);
    layer2_selectors(&doc, data);

    Ok(())
}

#[cfg(feature = "js_render")]
fn find_chrome() -> Option<std::path::PathBuf> {
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    candidates
        .iter()
        .map(std::path::Path::new)
        .find(|p| p.exists())
        .map(|p| p.to_path_buf())
}

// ─── Score extraction (regex-based) ──────────────────────────────────────────

/// Extracts a numeric proficiency score that follows a test keyword.
///
/// Handles patterns such as:
///   "IELTS overall score of 6.5"    → "6.5"
///   "minimum IELTS 6.0"             → "6.0"
///   "PTE Academic: 54"              → "54"
///   "TOEFL iBT score: 79"           → "79"
///
/// The regex requires the score to look like a plausible test score (1-3 digits,
/// optional single decimal place) and NOT be a 4-digit year (≥ 1900), which
/// previously caused `min_pte` to capture page year values like "2025".
fn extract_test_score(text: &str, keywords: &[&str]) -> Option<String> {
    for keyword in keywords {
        let escaped = regex::escape(keyword);
        // Pattern: keyword, optional filler text, then a 1-3 digit number (possibly with one decimal)
        let pattern = format!(r"(?i){}\s*[:]?\s*(?:of\s+)?(?:score\s+)?(\d{{1,3}}(?:\.\d)?)", escaped);
        if let Ok(re) = Regex::new(&pattern) {
            if let Some(cap) = re.captures(text) {
                let score = &cap[1];
                // Sanity-check: reject 4-digit numbers (years) and implausibly large scores
                if score.len() <= 3 {
                    return Some(score.to_string());
                }
            }
        }
    }
    
    // Fallback: simple keyword-find, then take the next short number
    for keyword in keywords {
        if let Some(pos) = text.to_lowercase().find(&keyword.to_lowercase()) {
            let after = &text[pos + keyword.len()..];
            let window: String = after.chars().take(60).collect();
            let re = Regex::new(r"\b(\d{1,3}(?:\.\d)?)\b").unwrap();
            if let Some(cap) = re.captures(&window) {
                let score = &cap[1];
                if score.len() <= 3 {
                    return Some(score.to_string());
                }
            }
        }
    }
    None
}

/// Looks for a fee amount (£ or GBP) in the key-info strip.
/// Returns the first plausible fee string found.
fn extract_fee_from_content(doc: &Html) -> Option<String> {
    // Coventry key-info selectors
    for sel_str in &[
        ".c-key-info__value",
        ".key-info__value",
        ".course-detail__value",
        ".keyinfo__value",
    ] {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                let text = el.text().collect::<String>();
                if text.contains('£') || text.to_uppercase().contains("GBP") {
                    let cleaned = text.trim().to_string();
                    if !cleaned.is_empty() {
                        return Some(cleaned);
                    }
                }
            }
        }
    }
    // Regex fallback: find "£XX,XXX" or "GBP XXXXX" anywhere in the page
    let body_text: String = doc.root_element().text().collect();
    let re = Regex::new(r"£\s*[\d,]+(?:\s*per\s+year)?|GBP\s*[\d,]+").unwrap();
    re.find(&body_text).map(|m| m.as_str().trim().to_string())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Sets a field only if it is currently `NA`, using the provided closure.
#[inline]
fn set_if_na(field: &mut String, f: impl FnOnce() -> Option<String>) {
    if field.as_str() == NA {
        if let Some(v) = f() {
            let v = v.trim().to_string();
            if !v.is_empty() && v != NA {
                *field = v;
            }
        }
    }
}

/// Extracts text from the page's primary content container.
/// Scoping to `<main>` prevents nav/footer/cookie-banner text from creating
/// false positives in score and keyword extraction.
fn main_content_text(doc: &Html) -> String {
    let sel = Selector::parse("main, #main-content, .main-content, article, #content")
        .unwrap_or_else(|_| Selector::parse("body").unwrap());
    doc.select(&sel)
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| doc.root_element().text().collect::<Vec<_>>().join(" "))
}

/// Derives study level from the URL path — free, before any network request.
fn study_level_from_url(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    if lower.contains("/course-structure/ug/") || lower.contains("/courses/undergraduate/") {
        Some("Undergraduate".to_string())
    } else if lower.contains("/course-structure/pg/") || lower.contains("/courses/postgraduate/") {
        Some("Postgraduate".to_string())
    } else if lower.contains("/course-structure/research/") || lower.contains("/courses/research/") {
        Some("Research".to_string())
    } else {
        None
    }
}

/// Derives study level from page content when the URL doesn't make it clear.
fn study_level_from_content(doc: &Html) -> Option<String> {
    for sel_str in &[
        "nav[aria-label='breadcrumb'] li",
        ".breadcrumb li",
        "ol.breadcrumb li",
        ".c-breadcrumb__item",
    ] {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                let t = el.text().collect::<String>().to_lowercase();
                if t.contains("undergraduate") {
                    return Some("Undergraduate".to_string());
                }
                if t.contains("postgraduate") {
                    return Some("Postgraduate".to_string());
                }
                if t.contains("research") {
                    return Some("Research".to_string());
                }
            }
        }
    }
    if let Ok(sel) = Selector::parse("meta[name='keywords'], meta[name='description']") {
        for el in doc.select(&sel) {
            if let Some(content) = el.value().attr("content") {
                let l = content.to_lowercase();
                if l.contains("undergraduate") {
                    return Some("Undergraduate".to_string());
                }
                if l.contains("postgraduate")
                    || l.contains("masters")
                    || l.contains("msc")
                    || l.contains("mba")
                {
                    return Some("Postgraduate".to_string());
                }
                if l.contains("phd") || l.contains("doctorate") {
                    return Some("Research".to_string());
                }
            }
        }
    }
    None
}

/// Returns `true` when enough critical fields are still `NA` to justify a
/// Layer 3 JS render attempt.
fn needs_js_render(data: &CourseData) -> bool {
    [
        &data.program_course_name,
        &data.study_level,
        &data.course_duration,
        &data.yearly_tuition_fee,
        &data.all_intakes_available,
        &data.campus,
    ]
    .iter()
    .filter(|f| f.as_str() == NA)
    .count()
        >= 3
}

/// Tries each selector string in order; returns the trimmed text of the first match.
fn first_text(doc: &Html, selectors: &[&str]) -> Option<String> {
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                let text = el.text().collect::<String>().trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Collects text from the first matching selector, truncated to `max_chars`.
fn text_from_selectors(doc: &Html, selectors: &[&str], max_chars: usize) -> Option<String> {
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            let text: String = doc
                .select(&sel)
                .flat_map(|el| el.text())
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.trim().to_string();
            if text.len() > 20 {
                let snippet: String = text.chars().take(max_chars).collect();
                return Some(snippet.trim().to_string());
            }
        }
    }
    None
}

/// Searches for a label-value pair using three HTML patterns:
///   1. `<dt>Label</dt><dd>Value</dd>`
///   2. `.label-class` + sibling `.value-class`
///   3. Any element whose visible text starts with "Label: value"
fn find_labeled_value(doc: &Html, labels: &[&str]) -> String {
    // Pattern 1: definition list
    if let Ok(dt_sel) = Selector::parse("dt") {
        for dt in doc.select(&dt_sel) {
            let dt_text = dt.text().collect::<String>();
            if labels
                .iter()
                .any(|l| dt_text.trim().to_lowercase().contains(&l.to_lowercase()))
            {
                if let Some(dd) = dt.next_sibling_element() {
                    let val: String = dd.text().collect::<String>().trim().to_string();
                    if !val.is_empty() {
                        return val;
                    }
                }
            }
        }
    }

    // Pattern 2: Coventry-specific label/value class pairs
    for label_sel in &[
        ".c-key-info__label",
        ".course-detail__label",
        ".keyinfo__label",
        ".key-info__label",
        ".info-label",
        "th",
    ] {
        if let Ok(sel) = Selector::parse(label_sel) {
            for el in doc.select(&sel) {
                let text = el.text().collect::<String>();
                if labels
                    .iter()
                    .any(|l| text.trim().to_lowercase().contains(&l.to_lowercase()))
                {
                    // Try next sibling with Coventry-specific value classes
                    if let Some(sibling) = el.next_sibling_element() {
                        let val: String = sibling.text().collect::<String>().trim().to_string();
                        if !val.is_empty() {
                            return val;
                        }
                    }
                }
            }
        }
    }

    // Pattern 3: "Label: value" inline text
    for sel_str in &["li", "p", "div", "span"] {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                let text = el.text().collect::<String>();
                for label in labels {
                    let prefix = format!("{}:", label);
                    if text.trim().to_lowercase().starts_with(&prefix.to_lowercase()) {
                        let val = text[prefix.len()..].trim().to_string();
                        if !val.is_empty() {
                            return val;
                        }
                    }
                }
            }
        }
    }

    NA.to_string()
}

/// Normalises whitespace in a string.
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Converts ISO 8601 duration strings to human-readable form.
/// e.g. "P3Y" → "3 years", "P1Y6M" → "1 year, 6 months"
fn parse_iso_duration(s: &str) -> String {
    if !s.starts_with('P') {
        return s.to_string();
    }
    let inner = &s[1..];
    let parts = [("Y", "year"), ("M", "month"), ("W", "week"), ("D", "day")];
    let result: Vec<String> = parts
        .iter()
        .filter_map(|(suffix, label)| {
            let idx = inner.find(suffix)?;
            let num_str: String = inner[..idx]
                .chars()
                .rev()
                .take_while(|c| c.is_numeric())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let n = num_str.parse::<u32>().ok()?;
            Some(format!("{} {}{}", n, label, if n != 1 { "s" } else { "" }))
        })
        .collect();

    if result.is_empty() {
        s.to_string()
    } else {
        result.join(", ")
    }
}