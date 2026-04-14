# Scraper

A production-grade web scraper built in **Rust** that extracts structured course data from [coventry.ac.uk](https://www.coventry.ac.uk). Designed for robustness, correctness, and long-term maintainability.

---

## Why Rust?

Most scrapers are written in Python with `requests` + `BeautifulSoup`. This implementation uses Rust for three reasons:

1. **Performance** — async I/O with `tokio` handles network latency without blocking threads
2. **Reliability** — Rust's type system eliminates null-pointer errors and data races at compile time
3. **Production-readiness** — retry logic, structured logging, and schema validation match real data-engineering pipelines

---

## Architecture

The scraper runs a 4-stage pipeline:

```
sitemap.rs → fetcher.rs → extractor.rs (4 layers) → output/courses.json
```

### Stage 1 — URL Discovery (`sitemap.rs`)

Four strategies are tried in order; the first that returns results wins:

| # | Strategy | Description |
|---|----------|-------------|
| 1 | **Sitemap XML** | Parses `sitemap_index.xml`, walks child sitemaps for `/course-structure/` URLs |
| 2 | **Search HTML** | Scrapes the search listing page for course links |
| 3 | **Homepage crawl** | Follows `/course-structure/` links found on the Coventry homepage |
| 4 | **Verified fallback** | Hardcoded set of confirmed-working URLs, validated with HEAD requests |

**Deduplication:** Courses are deduplicated by their URL slug, so the same course does not appear twice under multiple intake years.

### Stage 2 — Async HTTP (`fetcher.rs`)

- Realistic browser headers (User-Agent, Accept-Language, etc.)
- `robots.txt` compliance check on startup
- Exponential backoff retry on 429 / 5xx responses (up to 4 attempts, capped at 16s delay)
- Respects the `Retry-After` header when provided
- 1-second polite delay between course page requests

### Stage 3 — 4-Layer Extraction (`extractor.rs`)

| Layer | Method | Why |
|-------|--------|-----|
| 0 | **Full-text Regex** | Extracts from entire page text using patterns — most reliable for unstructured content |
| 1 | **JSON-LD** | Universities embed Schema.org structured data for SEO — zero fragile selectors |
| 2 | **HTML selectors** | Fills gaps not covered by other layers; uses Coventry-specific CSS classes |
| 3 | **Headless Chrome** | Renders JS-loaded content as a last resort (optional feature) |

**Key improvements in final version:**
- **PTE score validation** — Filters invalid scores (only accepts 36-90 range)
- **Intake year filtering** — Only includes years >= 2025
- **Field-specific keyword validation** — Prevents pollution (e.g., GRE field only populated when actually mentioned in test context)
- **Document vs. requirement separation** — Distinguishes mandatory documents from academic requirements
- **IELTS leakage prevention** — English scores only appear in English fields

Any field not found at any layer is set to `"NA"` per specification.

### Stage 4 — Output (`schema.rs` + `main.rs`)

`CourseData` is a typed Rust struct with `serde::Serialize`. It serialises directly to clean, validated JSON.

---

## Project Structure

```
coventry_scraper/
├── Cargo.toml           # Dependencies
├── src/
│   ├── main.rs          # Orchestrator — runs the full pipeline
│   ├── sitemap.rs       # URL discovery (4-strategy cascade)
│   ├── fetcher.rs       # HTTP client with headers, robots check & retry
│   ├── extractor.rs     # 3-layer field extraction
│   └── schema.rs        # CourseData struct + helpers
├── output/
│   └── courses.json     # Final output (generated on run)
└── README.md
```

---

## Output Schema

Each course record in `courses.json` follows this structure (fields not found on the page are `"NA"`):

```json
{
  "program_course_name":                     "International Business Management MSc",
  "university_name":                         "Coventry University",
  "course_website_url":                      "https://www.coventry.ac.uk/course-structure/pg/bl/international-business-management-msc/?term=2025-26",
  "campus":                                  "Coventry",
  "country":                                 "United Kingdom",
  "address":                                 "Priory Street, Coventry, CV1 5FB, United Kingdom",
  "study_level":                             "Postgraduate",
  "course_duration":                         "1 year",
  "all_intakes_available":                   "September 2025",
  "mandatory_documents_required":            "Transcripts, personal statement ...",
  "yearly_tuition_fee":                      "£18,900",
  "scholarship_availability":                "Scholarships available (see course page)",
  "gre_gmat_mandatory_min_score":            "NA",
  "indian_regional_institution_restrictions":"NA",
  "class_12_boards_accepted":                "NA",
  "gap_year_max_accepted":                   "NA",
  "mandatory_work_exp":                      "NA",
  "max_backlogs":                            "NA",
  "ug_academic_min_gpa":                     "2:2 Honours degree or equivalent",
  "twelfth_pass_min_cgpa":                   "NA",
  "min_ielts":                               "6.5",
  "min_pte":                                 "58",
  "min_toefl":                               "79",
  "min_duolingo":                            "NA",
  "kaplan_test_of_english":                  "NA",
  "english_waiver_class12":                  "NA",
  "english_waiver_moi":                      "NA"
}
```

Fields that cannot be extracted are set to `"NA"`.
Raw text is preserved where Coventry lists entry requirements as paragraphs rather than structured data.

---

## Setup & Usage

### Prerequisites

- [Rust](https://rustup.rs) (stable, 1.70+)
- Internet access to `coventry.ac.uk`
- *(Optional)* [Google Chrome](https://www.google.com/chrome/) for Layer 3 JS rendering

### Install & Run

```bash
# Navigate to the project directory
cd Scraper_rust

# Build (first build downloads ~30 crates, ~1 min)
cargo build --release

# Run (default — no Chrome required)
cargo run --release

# Run with verbose logging
RUST_LOG=debug cargo run --release

# Run with Layer 3 headless Chrome (install Chrome first)
cargo run --release --features js_render
```

Output is written to `output/courses.json`.

### Running on Windows

```cmd
set RUST_LOG=debug
cargo run --release
```

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1 | Async runtime |
| `reqwest` | 0.12 | HTTP client with gzip/deflate |
| `scraper` | 0.20 | HTML parsing with CSS selectors |
| `serde` + `serde_json` | 1 | JSON serialisation |
| `quick-xml` | 0.36 | Sitemap XML parsing |
| `regex` | 1 | Robust test-score extraction |
| `chromiumoxide` | 0.7 | Headless Chrome — optional feature `js_render` |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 | Structured logging |
| `anyhow` | 1 | Ergonomic error handling |
| `futures` | 0.3 | Async stream utilities (used by chromiumoxide) |

---

## Design Decisions

**Why sitemap-first over listing-page scraping?**
Sitemaps are the canonical machine-readable index of a website — the same source search engines use. This approach is more robust to UI changes on the listing page.

**Why include `?term=` in fallback URLs?**
Coventry's course pages are parameterised by intake year. Without `?term=`, some fields (tuition fees, start dates) may not render or may show stale data. Fallback URLs explicitly include `?term=2025-26` to ensure dated information is retrieved.

**Why regex for score extraction?**
Simple `str.find()` followed by a numeric scan can pick up adjacent year numbers (e.g. "2025" following a test acronym). Regex-based extraction with a plausible-score guard (`≤3 digit` filter) prevents this class of false positives.

**Why return `"NA"` instead of throwing an error?**
Partial data is more useful than a crash. The scraper always produces a complete record; missing fields are explicitly marked so downstream consumers know the difference between "not found" and "not checked".

**Why deduplicate by slug?**
Coventry lists the same course under multiple `?term=` URLs. Without deduplication, running the scraper with `TARGET_COURSES = 5` could return five records for only two or three actual courses.
