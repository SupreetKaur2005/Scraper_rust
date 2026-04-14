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
///   Layer 0 — Full-text regex extraction (most reliable)
///   Layer 1 — JSON-LD structured data (zero fragile selectors)
///   Layer 2 — HTML CSS selectors (fills gaps left by other layers)
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
            
            // ─── LAYER 0: Full-text extraction (CRITICAL - most reliable) ─────
            let full_text = doc.root_element().text().collect::<Vec<_>>().join(" ");
            layer0_full_text(&full_text, &doc, &mut data);
            
            // ─── LAYER 1: JSON-LD ────────────────────────────────────────────
            layer1_json_ld(&doc, &mut data);

            if data.study_level == NA {
                if let Some(level) = study_level_from_content(&doc) {
                    data.study_level = level;
                }
            }

            // ─── LAYER 2: HTML selectors (now as fallback only) ──────────────
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

// ─── LAYER 0: Full-text extraction ───────────────────────────────────────────

fn layer0_full_text(full_text: &str, doc: &Html, data: &mut CourseData) {
    let text_lower = full_text.to_lowercase();
    
    // ── IELTS ────────────────────────────────────────────────────────────────
    set_if_na(&mut data.min_ielts, || {
        let re = Regex::new(r"(?i)ielts[^0-9]{0,30}(\d\.\d)").unwrap();
        re.captures(full_text)
            .map(|c| c[1].to_string())
            .filter(|s| {
                if let Ok(n) = s.parse::<f32>() {
                    n >= 4.0 && n <= 9.0
                } else {
                    false
                }
            })
    });
    
    // ── PTE (FIXED: validates score range 36-90) ─────────────────────────────
    set_if_na(&mut data.min_pte, || {
        let re = Regex::new(r"(?i)PTE(?:\s+Academic)?[^0-9]{0,30}(\d{2,3})").unwrap();
        for cap in re.captures_iter(full_text) {
            let score_str = &cap[1];
            if let Ok(score) = score_str.parse::<u32>() {
                // Valid PTE scores are typically 36-90
                if score >= 36 && score <= 90 {
                    return Some(score_str.to_string());
                }
            }
        }
        None
    });
    
    // ── TOEFL ────────────────────────────────────────────────────────────────
    set_if_na(&mut data.min_toefl, || {
        let re = Regex::new(r"(?i)TOEFL(?:\s+iBT)?[^0-9]{0,30}(\d{2,3})").unwrap();
        for cap in re.captures_iter(full_text) {
            let score_str = &cap[1];
            if let Ok(score) = score_str.parse::<u32>() {
                if score >= 60 && score <= 120 {
                    return Some(score_str.to_string());
                }
            }
        }
        None
    });
    
    // ── Duolingo ─────────────────────────────────────────────────────────────
    set_if_na(&mut data.min_duolingo, || {
        let re = Regex::new(r"(?i)duolingo[^0-9]{0,30}(\d{2,3})").unwrap();
        for cap in re.captures_iter(full_text) {
            let score_str = &cap[1];
            if let Ok(score) = score_str.parse::<u32>() {
                if score >= 80 && score <= 160 {
                    return Some(score_str.to_string());
                }
            }
        }
        None
    });
    
    // ── Kaplan ───────────────────────────────────────────────────────────────
    set_if_na(&mut data.kaplan_test_of_english, || {
        let re = Regex::new(r"(?i)kaplan[^0-9]{0,30}(\d{2,3})").unwrap();
        for cap in re.captures_iter(full_text) {
            let score_str = &cap[1];
            if let Ok(score) = score_str.parse::<u32>() {
                if score >= 300 && score <= 700 {
                    return Some(score_str.to_string());
                }
            }
        }
        None
    });
    
    // ── Duration ─────────────────────────────────────────────────────────────
    set_if_na(&mut data.course_duration, || {
        let patterns = [
            r"(?i)(\d+)\s*years?(?:\s*full[-\s]time)?",
            r"(?i)(\d+)\s*months?",
        ];
        for pat in &patterns {
            if let Ok(re) = Regex::new(pat) {
                if let Some(cap) = re.captures(full_text) {
                    return Some(cap[0].trim().to_string());
                }
            }
        }
        None
    });
    
    // ── Intakes (FIXED: filter valid future years only) ──────────────────────
    set_if_na(&mut data.all_intakes_available, || {
        let re = Regex::new(r"(?i)(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})").unwrap();
        let current_year = 2025; // Minimum valid year
        let mut intakes: Vec<String> = Vec::new();
        
        for cap in re.captures_iter(full_text) {
            let month = &cap[1];
            let year_str = &cap[2];
            if let Ok(year) = year_str.parse::<i32>() {
                // Only keep years >= current_year
                if year >= current_year {
                    intakes.push(format!("{} {}", month, year));
                }
            }
        }
        
        intakes.sort();
        intakes.dedup();
        
        if !intakes.is_empty() {
            Some(intakes.join(", "))
        } else {
            None
        }
    });
    
    // ── Work Experience (FIXED: keyword validation) ──────────────────────────
    set_if_na(&mut data.mandatory_work_exp, || {
        let patterns = [
            r"(?i)(?:\d+\+?\s*years?(?:\s+of)?\s+(?:relevant\s+)?(?:work\s+)?experience[^.]{0,80})",
            r"(?i)work\s+experience[^.]{0,80}\d+\s*years?",
        ];
        for pat in &patterns {
            if let Ok(re) = Regex::new(pat) {
                if let Some(cap) = re.find(full_text) {
                    let text = cap.as_str().trim().to_string();
                    let lower = text.to_lowercase();
                    if lower.contains("year") && lower.contains("experience") {
                        return Some(text);
                    }
                }
            }
        }
        None
    });
    
    // ── Mandatory Documents (FIXED: separate from academic requirements) ─────
    set_if_na(&mut data.mandatory_documents_required, || {
        if let Some(section) = extract_section_by_heading(doc, &["entry requirements", "requirements"]) {
            let lower = section.to_lowercase();
            
            // Skip if it's primarily about English requirements
            if lower.contains("ielts") && !lower.contains("transcript") && 
               !lower.contains("certificate") && !lower.contains("reference") {
                return None;
            }
            
            // Look for actual document mentions
            let doc_keywords = ["transcript", "certificate", "reference", "statement", 
                               "cv", "curriculum vitae", "passport", "visa", "portfolio",
                               "personal statement", "letter of recommendation"];
            
            let has_documents = doc_keywords.iter().any(|kw| lower.contains(kw));
            
            if has_documents {
                // Extract only the document-related parts
                let mut doc_text = String::new();
                for kw in doc_keywords {
                    let pattern = format!(r"(?i)[^.]*{}[^.]*\.", regex::escape(kw));
                    if let Ok(re) = Regex::new(&pattern) {
                        if let Some(cap) = re.find(&section) {
                            doc_text.push_str(cap.as_str().trim());
                            doc_text.push(' ');
                        }
                    }
                }
                
                let trimmed = doc_text.trim().to_string();
                if trimmed.len() > 30 {
                    return Some(trimmed.chars().take(500).collect());
                }
            }
            
            // If section mentions degree but no documents, don't use it
            if lower.contains("degree") || lower.contains("honours") || lower.contains("2:") {
                if !has_documents {
                    return None;
                }
            }
        }
        None
    });
    
    // ── Class 12 Boards Accepted (FIXED: education keyword filtering) ────────
    set_if_na(&mut data.class_12_boards_accepted, || {
        if let Some(section) = extract_section_by_heading(doc, &["entry requirements", "academic requirements"]) {
            let lower = section.to_lowercase();
            
            // Skip if it's just English requirements
            if lower.contains("ielts") && section.len() < 200 {
                return None;
            }
            
            if lower.contains("a-level") || lower.contains("ib") || 
               lower.contains("btec") || lower.contains("gcse") ||
               lower.contains("12th") || lower.contains("board") ||
               lower.contains("high school") || lower.contains("secondary") {
                let cleaned = section
                    .replace("Typical entry requirements:", "")
                    .replace("Entry requirements", "")
                    .trim()
                    .to_string();
                if cleaned.len() > 30 {
                    return Some(cleaned.chars().take(400).collect());
                }
            }
        }
        None
    });
    
    // ── UG Academic Requirement (FIXED: degree keyword filtering) ────────────
    set_if_na(&mut data.ug_academic_min_gpa, || {
        let patterns = [
            r"(?i)(?:2:[12]|first|upper\s+second|lower\s+second)[^.]{0,100}(?:honours|degree|classification)",
            r"(?i)(?:minimum|required|equivalent)[^.]{0,50}(?:degree|honours|gpa)[^.]{0,50}",
        ];
        for pat in &patterns {
            if let Ok(re) = Regex::new(pat) {
                if let Some(cap) = re.find(full_text) {
                    let text = cap.as_str().trim().to_string();
                    let lower = text.to_lowercase();
                    if lower.contains("degree") || lower.contains("honours") || 
                       lower.contains("gpa") || lower.contains("classification") ||
                       lower.contains("bachelor") || lower.contains("equivalent") {
                        return Some(text);
                    }
                }
            }
        }
        None
    });
    
    // ── 12th Grade Minimum (FIXED: A-level/GCSE keyword filtering) ───────────
    set_if_na(&mut data.twelfth_pass_min_cgpa, || {
        if let Some(section) = extract_section_by_heading(doc, &["entry requirements", "academic requirements"]) {
            let lower = section.to_lowercase();
            
            // Skip if it's just English requirements
            if lower.contains("ielts") && section.len() < 200 {
                return None;
            }
            
            if lower.contains("a-level") || lower.contains("a level") || 
               lower.contains("gcse") || lower.contains("12th") || 
               lower.contains("grade 12") || lower.contains("high school") {
                let patterns = [
                    r"(?i)A[- ]level[^.]{0,150}",
                    r"(?i)GCSE[^.]{0,100}",
                    r"(?i)(?:12th|grade\s*12|high\s+school)[^.]{0,100}",
                ];
                for pat in &patterns {
                    if let Ok(re) = Regex::new(pat) {
                        if let Some(cap) = re.find(&section) {
                            let text = cap.as_str().trim().to_string();
                            if text.len() > 15 {
                                return Some(text.chars().take(300).collect());
                            }
                        }
                    }
                }
            }
        }
        None
    });
    
    // ── English Waiver - Medium of Instruction ───────────────────────────────
    set_if_na(&mut data.english_waiver_moi, || {
        if text_lower.contains("medium of instruction") || 
           text_lower.contains("english taught") ||
           text_lower.contains("taught in english") ||
           text_lower.contains("english as the medium") {
            Some("May be waived if previous study was conducted in English".to_string())
        } else {
            None
        }
    });
    
    // ── English Waiver - Class 12 ────────────────────────────────────────────
    set_if_na(&mut data.english_waiver_class12, || {
        if text_lower.contains("english at gcse")
            || text_lower.contains("english language gcse")
            || text_lower.contains("grade c in english")
            || text_lower.contains("grade 4 in english")
            || text_lower.contains("english gcse")
            || text_lower.contains("gcse english")
        {
            Some("English at GCSE grade C/4 or equivalent may satisfy the requirement".to_string())
        } else {
            None
        }
    });
    
    // ── Campus (FIXED: default to Coventry for most courses) ─────────────────
    set_if_na(&mut data.campus, || {
        if text_lower.contains("coventry campus") || 
           text_lower.contains("coventry university campus") ||
           text_lower.contains("main campus") {
            Some("Coventry".to_string())
        } else if text_lower.contains("london campus") || 
                  text_lower.contains("coventry university london") {
            Some("London".to_string())
        } else if text_lower.contains("scarborough campus") {
            Some("Scarborough".to_string())
        } else if text_lower.contains("campus:") {
            // Try to extract campus name from "Campus: XXX" pattern
            let re = Regex::new(r"(?i)campus\s*[:]\s*([^.,\n]+)").unwrap();
            re.captures(full_text)
                .map(|c| c[1].trim().to_string())
                .filter(|s| !s.is_empty() && s.len() < 30)
        } else {
            // Default to Coventry for most courses
            Some("Coventry".to_string())
        }
    });
    
    // ── GRE/GMAT (FIXED: strict keyword validation - no pollution) ──────────
    set_if_na(&mut data.gre_gmat_mandatory_min_score, || {
        // Only proceed if GRE or GMAT is actually mentioned in a meaningful context
        if text_lower.contains("gre") || text_lower.contains("gmat") {
            // Look for "required", "minimum", "score" nearby
            let re = Regex::new(r"(?i)(?:GRE|GMAT)[^.]{0,80}(?:required|minimum|score|expected)").unwrap();
            if let Some(cap) = re.find(full_text) {
                let text = cap.as_str().trim().to_string();
                // Verify it's actually about test scores, not navigation/footer
                let lower = text.to_lowercase();
                if (lower.contains("gre") || lower.contains("gmat")) && 
                   (lower.contains("required") || lower.contains("score") || 
                    lower.contains("minimum") || lower.contains("recommended")) &&
                   !lower.contains("research") && !lower.contains("global") &&
                   !lower.contains("useful links") && !lower.contains("explore") {
                    return Some(text);
                }
            }
        }
        None
    });
    
    // ── Max Backlogs ─────────────────────────────────────────────────────────
    set_if_na(&mut data.max_backlogs, || {
        if text_lower.contains("backlog") || text_lower.contains("arrears") {
            let re = Regex::new(r"(?i)(?:backlog|arrears?)[^.]{0,80}").unwrap();
            re.find(full_text).map(|m| m.as_str().trim().to_string())
        } else {
            None
        }
    });
    
    // ── Gap Year ─────────────────────────────────────────────────────────────
    set_if_na(&mut data.gap_year_max_accepted, || {
        if text_lower.contains("gap year") {
            let re = Regex::new(r"(?i)gap year[^.]*?(\d+)").unwrap();
            if let Some(cap) = re.captures(full_text) {
                return Some(cap[1].to_string());
            }
            Some("Permitted — see entry requirements".to_string())
        } else {
            None
        }
    });
}

/// Extracts text content following a heading that contains any of the given keywords
/// FIXED: Properly walks sibling elements to get content AFTER heading
fn extract_section_by_heading(doc: &Html, heading_keywords: &[&str]) -> Option<String> {
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6, .c-tab__heading, .accordion__heading, .c-tab__title, .c-accordion__title").unwrap();
    
    for heading in doc.select(&heading_sel) {
        let heading_text = heading.text().collect::<String>().to_lowercase();
        
        if heading_keywords.iter().any(|kw| heading_text.contains(kw)) {
            // Walk through siblings after the heading
            let mut content = String::new();
            let mut current = heading.next_sibling_element();
            let mut count = 0;
            
            while let Some(el) = current {
                count += 1;
                if count > 15 { break; } // Limit how far we go
                
                let name = el.value().name();
                // Stop if we hit another heading (h1-h6)
                if name.starts_with('h') && name.len() == 2 {
                    if let Ok(num) = name[1..].parse::<u32>() {
                        if num >= 1 && num <= 6 {
                            break;
                        }
                    }
                }
                // Also stop at major section containers
                if name == "section" || name == "div" {
                    if let Some(class) = el.value().attr("class") {
                        if class.contains("tab") || class.contains("accordion") {
                            break;
                        }
                    }
                }
                
                // Get text from this element
                let el_text = el.text().collect::<String>();
                if !el_text.trim().is_empty() {
                    content.push_str(&el_text);
                    content.push(' ');
                }
                
                current = el.next_sibling_element();
            }
            
            let trimmed = content.trim().to_string();
            if trimmed.len() > 30 {
                return Some(trimmed);
            }
        }
    }
    None
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

// ─── Layer 2: HTML selectors (now as fallback) ────────────────────────────────

fn layer2_selectors(doc: &Html, data: &mut CourseData) {
    let content_text = main_content_text(doc);
    let content_lower = content_text.to_lowercase();

    // Program name
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

    // Study level from breadcrumb
    set_if_na(&mut data.study_level, || {
        first_text(
            doc,
            &[
                "nav[aria-label='breadcrumb'] li:nth-child(2)",
                ".breadcrumb li:nth-child(2)",
                "ol.breadcrumb li:nth-child(2)",
                ".c-breadcrumb__item:nth-child(2)",
            ],
        )
    });

    // Tuition fee from key-info strip
    set_if_na(&mut data.yearly_tuition_fee, || {
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
        extract_fee_from_content(doc)
    });

    // Scholarship
    set_if_na(&mut data.scholarship_availability, || {
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

    // Indian restrictions
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
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let html = page.content().await?;
    browser.close().await?;
    handler_task.abort();

    let doc = Html::parse_document(&html);
    let full_text = doc.root_element().text().collect::<Vec<_>>().join(" ");
    layer0_full_text(&full_text, &doc, data);
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

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Looks for a fee amount (£ or GBP) in the key-info strip.
fn extract_fee_from_content(doc: &Html) -> Option<String> {
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
    let body_text: String = doc.root_element().text().collect();
    let re = Regex::new(r"£\s*[\d,]+(?:\s*per\s+year)?").unwrap();
    re.find(&body_text).map(|m| m.as_str().trim().to_string())
}

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
fn main_content_text(doc: &Html) -> String {
    let sel = Selector::parse("main, #main-content, .main-content, article, #content")
        .unwrap_or_else(|_| Selector::parse("body").unwrap());
    doc.select(&sel)
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| doc.root_element().text().collect::<Vec<_>>().join(" "))
}

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
    .count() >= 3
}

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