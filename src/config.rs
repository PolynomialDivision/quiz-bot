use serde::Deserialize;
pub use mxbot_common::config::{EncryptionStrategy, MatrixConfig};

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub trivia: TriviaConfig,
    #[serde(default)]
    pub explainer: ExplainerConfig,
}

#[derive(Deserialize, Default)]
pub struct SecurityConfig {
    #[serde(default)]
    pub allowed_inviters: Vec<String>,
    #[serde(default)]
    pub admin_users: Vec<String>,
    #[serde(default)]
    pub encryption_strategy: EncryptionStrategy,
}

#[derive(Deserialize)]
pub struct ScheduleConfig {
    pub room_id: String,
    /// One or more "HH:MM" times (in the configured timezone) to run the quiz.
    /// e.g. `quiz_times = ["18:00", "21:30"]`
    pub quiz_times: Vec<String>,
    /// Seconds to collect reactions before revealing the answer.
    #[serde(default = "default_answer_timeout")]
    pub answer_timeout_secs: u64,
    /// Number of questions per quiz round.
    #[serde(default = "default_questions_per_round")]
    pub questions_per_round: u32,
    /// Pause in seconds between questions.
    #[serde(default = "default_inter_question_secs")]
    pub inter_question_secs: u64,
    /// Seconds before the quiz to post a "starting soon" reminder.
    /// Set to 0 to disable. The scheduler fires this many seconds early.
    #[serde(default = "default_reminder_before_secs")]
    pub reminder_before_secs: u64,
    /// IANA timezone used for quiz scheduling (e.g. "Europe/Berlin").
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

impl ScheduleConfig {
    /// Parse `"HH:MM"` into `(hour, minute)`.  Returns `None` on invalid input.
    pub fn parse_quiz_time(s: &str) -> Option<(u32, u32)> {
        let (h, m) = s.split_once(':')?;
        let hour: u32   = h.trim().parse().ok()?;
        let minute: u32 = m.trim().parse().ok()?;
        if hour < 24 && minute < 60 { Some((hour, minute)) } else { None }
    }
}

fn default_answer_timeout() -> u64 { 60 }
fn default_questions_per_round() -> u32 { 5 }
fn default_inter_question_secs() -> u64 { 10 }
fn default_reminder_before_secs() -> u64 { 300 }   // 5 minutes
fn default_timezone() -> String { "UTC".to_owned() }

#[derive(Deserialize, Default)]
pub struct TriviaConfig {
    /// OpenTDB category ID (see https://opentdb.com/api_category.php).
    /// Omit for random category.
    pub category: Option<u32>,
    /// Difficulty filter: "easy", "medium", or "hard".
    /// Omit for any difficulty.
    pub difficulty: Option<String>,
    /// How many questions to pre-fetch per API call (1–50, default 10).
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    /// Category groups to skip entirely.
    /// Available names: "General Knowledge", "Entertainment", "Science & Technology",
    /// "Mythology", "Sports", "Geography", "History", "Politics", "Art",
    /// "Celebrities", "Animals", "Vehicles".
    /// Matching is case-insensitive and ignores "&" vs "and".
    #[serde(default)]
    pub excluded_categories: Vec<String>,
}

fn default_batch_size() -> u32 { 10 }

#[derive(Deserialize, Default)]
pub struct ExplainerConfig {
    /// Groq API key — leave empty to disable post-question explanations.
    pub api_key: Option<String>,
    /// Groq model to use for explanations (default: llama-3.3-70b-versatile).
    #[serde(default = "default_explainer_model")]
    pub model: String,
}

fn default_explainer_model() -> String { "llama-3.3-70b-versatile".to_owned() }
