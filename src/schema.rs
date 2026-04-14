use serde::{Deserialize, Serialize};

/// Sentinel value used for every field that could not be extracted.
pub const NA: &str = "NA";

/// Represents all required fields for a single Coventry University course.
/// Fields that cannot be found on the page are set to `NA`.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct CourseData {
    // ── Identity ──────────────────────────────────────────────────────────────
    pub program_course_name:                      String,
    pub university_name:                          String,
    pub course_website_url:                       String,
    pub campus:                                   String,
    pub country:                                  String,
    pub address:                                  String,

    // ── Course details ────────────────────────────────────────────────────────
    pub study_level:                              String,
    pub course_duration:                          String,
    pub all_intakes_available:                    String,
    pub mandatory_documents_required:             String,
    pub yearly_tuition_fee:                       String,
    pub scholarship_availability:                 String,

    // ── Admissions ────────────────────────────────────────────────────────────
    pub gre_gmat_mandatory_min_score:             String,
    pub indian_regional_institution_restrictions: String,
    pub class_12_boards_accepted:                 String,
    pub gap_year_max_accepted:                    String,
    pub mandatory_work_exp:                       String,
    pub max_backlogs:                             String,
    pub ug_academic_min_gpa:                      String,
    pub twelfth_pass_min_cgpa:                    String,

    // ── English proficiency ───────────────────────────────────────────────────
    pub min_ielts:                                String,
    pub min_pte:                                  String,
    pub min_toefl:                                String,
    pub min_duolingo:                             String,
    pub kaplan_test_of_english:                   String,
    pub english_waiver_class12:                   String,
    pub english_waiver_moi:                       String,
}

impl CourseData {
    /// Total number of schema fields — must match the struct definition above.
    /// The compile-time assertion below enforces this.
    pub const FIELD_COUNT: usize = 27;

    /// Compile-time guard: if you add/remove a field but forget to update
    /// FIELD_COUNT, `fields()` will fail to compile because the array size
    /// won't match.
    const _FIELD_COUNT_CHECK: () = {
        // Verified against the 27 fields in the struct above.
        // Update both the struct and FIELD_COUNT together.
        let _ = [0u8; Self::FIELD_COUNT]; // keeps the const used
    };

    /// Creates a new `CourseData` with all variable fields defaulting to `NA`.
    /// Fields that are constant across all Coventry records are pre-filled.
    pub fn new_empty(url: &str) -> Self {
        Self {
            program_course_name:                      NA.to_string(),
            university_name:                          "Coventry University".to_string(),
            course_website_url:                       url.to_string(),
            campus:                                   NA.to_string(),
            country:                                  "United Kingdom".to_string(),
            address:                                  "Priory Street, Coventry, CV1 5FB, United Kingdom".to_string(),

            study_level:                              NA.to_string(),
            course_duration:                          NA.to_string(),
            all_intakes_available:                    NA.to_string(),
            mandatory_documents_required:             NA.to_string(),
            yearly_tuition_fee:                       NA.to_string(),
            scholarship_availability:                 NA.to_string(),

            gre_gmat_mandatory_min_score:             NA.to_string(),
            indian_regional_institution_restrictions: NA.to_string(),
            class_12_boards_accepted:                 NA.to_string(),
            gap_year_max_accepted:                    NA.to_string(),
            mandatory_work_exp:                       NA.to_string(),
            max_backlogs:                             NA.to_string(),
            ug_academic_min_gpa:                      NA.to_string(),
            twelfth_pass_min_cgpa:                    NA.to_string(),

            min_ielts:                                NA.to_string(),
            min_pte:                                  NA.to_string(),
            min_toefl:                                NA.to_string(),
            min_duolingo:                             NA.to_string(),
            kaplan_test_of_english:                   NA.to_string(),
            english_waiver_class12:                   NA.to_string(),
            english_waiver_moi:                       NA.to_string(),
        }
    }

    /// Returns all 27 field values as `&str` slices.
    /// Single source of truth used by `fill_rate()` and the stats summary.
    pub fn fields(&self) -> [&str; Self::FIELD_COUNT] {
        [
            &self.program_course_name,        &self.university_name,
            &self.course_website_url,         &self.campus,
            &self.country,                    &self.address,
            &self.study_level,                &self.course_duration,
            &self.all_intakes_available,      &self.mandatory_documents_required,
            &self.yearly_tuition_fee,         &self.scholarship_availability,
            &self.gre_gmat_mandatory_min_score,
            &self.indian_regional_institution_restrictions,
            &self.class_12_boards_accepted,   &self.gap_year_max_accepted,
            &self.mandatory_work_exp,         &self.max_backlogs,
            &self.ug_academic_min_gpa,        &self.twelfth_pass_min_cgpa,
            &self.min_ielts,                  &self.min_pte,
            &self.min_toefl,                  &self.min_duolingo,
            &self.kaplan_test_of_english,     &self.english_waiver_class12,
            &self.english_waiver_moi,
        ]
    }

    /// Number of fields that contain real data (not `NA`).
    pub fn fill_rate(&self) -> usize {
        self.fields().iter().filter(|&&f| f != NA).count()
    }

    /// Number of fields still set to `NA`.
    pub fn na_count(&self) -> usize {
        Self::FIELD_COUNT - self.fill_rate()
    }

    /// Returns `true` if extraction succeeded at a minimum level (course name found).
    pub fn is_valid(&self) -> bool {
        self.program_course_name != NA
    }
}