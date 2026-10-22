pub use mxbot_common::config::{MatrixConfig, SecurityConfig, TimeOfDay};
use serde::{Deserialize, Deserializer, Serialize};

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

#[derive(Deserialize)]
pub struct ScheduleConfig {
    pub room_id: String,
    /// One or more "HH:MM" times (in the configured timezone) to run the quiz.
    /// e.g. `quiz_times = ["18:00", "21:30"]`. A malformed time is refused
    /// at startup.
    pub quiz_times: Vec<TimeOfDay>,
    /// Seconds to collect reactions before revealing the answer.
    #[serde(default = "default_answer_timeout")]
    pub answer_timeout_secs: u64,
    /// Number of questions per quiz round.
    #[serde(default = "default_questions_per_round")]
    pub questions_per_round: u32,
    /// Pause in seconds between questions.
    #[serde(default = "default_inter_question_secs")]
    pub inter_question_secs: u64,
    /// Seconds before the quiz to post "starting soon" reminders.
    /// Accepts a single integer (`300`) or a list (`[300, 60]`).
    /// The list is sorted and each entry fires at that many seconds before
    /// the quiz, with the largest value determining when the scheduler wakes.
    /// Set to `0` or `[]` to disable.  Default: `[300]` (one reminder, 5 min).
    #[serde(
        default = "default_reminder_before_secs",
        deserialize_with = "deserialize_reminders"
    )]
    pub reminder_before_secs: Vec<u64>,
    /// IANA timezone used for quiz scheduling (e.g. "Europe/Berlin").
    /// Default: UTC. An unknown name is refused at startup.
    #[serde(
        default = "default_timezone",
        deserialize_with = "deserialize_timezone"
    )]
    pub timezone: chrono_tz::Tz,
}

impl ScheduleConfig {
    /// Parse `"HH:MM"` (typed with `!schedulequiz`, or stored with a
    /// one-time quiz) into `(hour, minute)`.  Returns `None` on invalid input.
    pub fn parse_quiz_time(s: &str) -> Option<(u32, u32)> {
        let time: TimeOfDay = s.parse().ok()?;
        Some((time.hour, time.minute))
    }
}

fn default_answer_timeout() -> u64 {
    60
}
fn default_questions_per_round() -> u32 {
    5
}
fn default_inter_question_secs() -> u64 {
    10
}
fn default_reminder_before_secs() -> Vec<u64> {
    vec![300]
}

/// Accepts either a bare integer (`300`) or an array (`[300, 60]`).
fn deserialize_reminders<'de, D>(d: D) -> Result<Vec<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a number or array of numbers")
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Vec<u64>, E> {
            Ok(if v == 0 { vec![] } else { vec![v] })
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Vec<u64>, E> {
            Ok(if v <= 0 { vec![] } else { vec![v as u64] })
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u64>, A::Error> {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<u64>()? {
                if v > 0 {
                    out.push(v);
                }
            }
            Ok(out)
        }
    }
    d.deserialize_any(V)
}
fn default_timezone() -> chrono_tz::Tz {
    chrono_tz::UTC
}

/// An IANA timezone name such as "Europe/Berlin".
fn deserialize_timezone<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<chrono_tz::Tz, D::Error> {
    let name = String::deserialize(deserializer)?;
    name.trim().parse().map_err(|_| {
        serde::de::Error::custom(format!(
            "unknown timezone {name:?} (expected an IANA name such as \"Europe/Berlin\")"
        ))
    })
}

#[derive(Deserialize)]
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
    /// Number of recently asked category groups to avoid. This is relaxed
    /// automatically when the active category pool is too small.
    #[serde(default = "default_recent_category_window")]
    pub recent_category_window: usize,
    /// Optional secondary question source: TriviaQA questions with
    /// LLM-generated wrong answers. Disabled by default.
    #[serde(default)]
    pub triviaqa: TriviaQaConfig,
}

fn default_batch_size() -> u32 {
    10
}
fn default_recent_category_window() -> usize {
    5
}

impl Default for TriviaConfig {
    fn default() -> Self {
        Self {
            category: None,
            difficulty: None,
            batch_size: default_batch_size(),
            excluded_categories: Vec::new(),
            recent_category_window: default_recent_category_window(),
            triviaqa: TriviaQaConfig::default(),
        }
    }
}

#[derive(Deserialize)]
pub struct TriviaQaConfig {
    /// Off by default: TriviaQA questions need an LLM call (via
    /// `[explainer].api_key`) to generate wrong answers, and the dataset
    /// must be downloaded once before it can supply any questions.
    #[serde(default)]
    pub enabled: bool,
    /// Where to download the dataset from on first use (once: see
    /// `crate::triviaqa`). Point this straight at the official release
    /// (e.g. https://nlp.cs.washington.edu/triviaqa/data/triviaqa-unfiltered.tar.gz):
    /// the bot reads `.tar.gz` archives natively, picking the
    /// `*-dev.json` split out of the bundle (see
    /// `triviaqa::PREFERRED_ARCHIVE_ENTRIES`), so no local extraction or
    /// re-hosting is needed. Plain JSON or gzip-compressed JSON matching
    /// TriviaQA's schema
    /// (`{"Data": [{"Question": ..., "Answer": {"Value": ..., "Aliases": [...]}}]}`)
    /// also works, e.g. to point at your own mirror/subset.
    pub dataset_url: Option<String>,
    /// Cap on how many QA pairs to ingest from the dataset (the full
    /// unfiltered set is very large). Default 20,000.
    #[serde(default = "default_triviaqa_max_pool_size")]
    pub max_pool_size: usize,
    /// Fraction of quiz questions drawn from TriviaQA when enabled and the
    /// pool has usable questions available (0.0–1.0). The rest (and any
    /// slot TriviaQA can't fill: pool exhausted, LLM distractor generation
    /// failed) falls back to OpenTDB. Default 0.3.
    #[serde(default = "default_triviaqa_mix_ratio")]
    pub mix_ratio: f64,
}

fn default_triviaqa_max_pool_size() -> usize {
    20_000
}
fn default_triviaqa_mix_ratio() -> f64 {
    0.3
}

impl Default for TriviaQaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dataset_url: None,
            max_pool_size: default_triviaqa_max_pool_size(),
            mix_ratio: default_triviaqa_mix_ratio(),
        }
    }
}

// ── Runtime settings (`!admin set …`) ─────────────────────────────────────────

/// Where quiz questions come from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuestionSource {
    /// OpenTDB, with a share of TriviaQA (`triviaqa_share`).
    Mixed,
    /// Only OpenTDB.
    Opentdb,
    /// Only TriviaQA (falls back to OpenTDB for a slot it can't fill).
    Triviaqa,
}

impl QuestionSource {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mixed" | "mix" => Some(Self::Mixed),
            "opentdb" => Some(Self::Opentdb),
            "triviaqa" | "ai" => Some(Self::Triviaqa),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Opentdb => "opentdb",
            Self::Triviaqa => "triviaqa",
        }
    }
}

/// Knobs admins can switch in chat (`!admin set question_source triviaqa`)
/// to test the question sources in a targeted way. Defaults come from
/// `[trivia.triviaqa]`.
#[derive(Serialize, Deserialize, Clone)]
pub struct QuizSettings {
    /// `mixed`, `opentdb` or `triviaqa`.
    pub question_source: QuestionSource,
    /// Share of TriviaQA questions in `mixed` mode, 0.0–1.0.
    #[serde(deserialize_with = "share")]
    pub triviaqa_share: f64,
}

fn share<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = f64::deserialize(d)?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(serde::de::Error::custom("must be between 0.0 and 1.0"))
    }
}

impl QuizSettings {
    pub fn from_config(config: &Config) -> Self {
        let tq = &config.trivia.triviaqa;
        QuizSettings {
            question_source: if tq.enabled {
                QuestionSource::Mixed
            } else {
                QuestionSource::Opentdb
            },
            triviaqa_share: tq.mix_ratio.clamp(0.0, 1.0),
        }
    }
}

#[derive(Deserialize, Default)]
pub struct ExplainerConfig {
    /// Groq API key: leave empty to disable post-question explanations.
    pub api_key: Option<String>,
    /// Groq model to use for explanations (default: llama-3.3-70b-versatile).
    #[serde(default = "default_explainer_model")]
    pub model: String,
}

fn default_explainer_model() -> String {
    "llama-3.3-70b-versatile".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiz_settings_validate_the_share_and_parse_sources() {
        let ok: QuizSettings =
            serde_json::from_str(r#"{"question_source": "triviaqa", "triviaqa_share": 0.5}"#)
                .unwrap();
        assert_eq!(ok.question_source, QuestionSource::Triviaqa);
        assert!(serde_json::from_str::<QuizSettings>(
            r#"{"question_source": "mixed", "triviaqa_share": 1.5}"#
        )
        .is_err());
        assert!(serde_json::from_str::<QuizSettings>(
            r#"{"question_source": "wikipedia", "triviaqa_share": 0.5}"#
        )
        .is_err());
        assert_eq!(QuestionSource::parse("AI"), Some(QuestionSource::Triviaqa));
        assert_eq!(QuestionSource::parse("x"), None);
    }

    /// The config house-infra renders today (stacks/matrix-bots/secrets/
    /// quiz-bot.config.toml.j2, with placeholder secrets) still loads.
    #[test]
    fn the_deployed_config_parses() {
        let config: Config = toml::from_str(include_str!("../tests/deployed.config.toml")).unwrap();
        let s = &config.schedule;
        assert_eq!(s.quiz_times, [TimeOfDay::new(12, 0), TimeOfDay::new(20, 0)]);
        assert_eq!(s.timezone, chrono_tz::Europe::Berlin);
        assert_eq!(s.reminder_before_secs, [300, 60]);
        assert!(config.trivia.triviaqa.enabled);
        config.security.validate().unwrap();
    }

    fn schedule(extra: &str) -> Result<ScheduleConfig, toml::de::Error> {
        toml::from_str(&format!("room_id = \"!room:example.org\"\n{extra}"))
    }

    #[test]
    fn quiz_times_and_timezone_are_checked_at_load() {
        let s = schedule("quiz_times = [\"12:00\", \"8:30\"]").unwrap();
        assert_eq!(s.quiz_times, [TimeOfDay::new(12, 0), TimeOfDay::new(8, 30)]);
        assert_eq!(s.timezone, chrono_tz::UTC);
        for bad in ["24:00", "12:60", "noon", "12", ""] {
            let Err(e) = schedule(&format!("quiz_times = [\"12:00\", \"{bad}\"]")) else {
                panic!("{bad} was accepted");
            };
            assert!(e.to_string().contains("time"), "{bad}: {e}");
        }
        let Err(e) = schedule("quiz_times = []\ntimezone = \"Europe/Berln\"") else {
            panic!("a misspelt timezone was accepted");
        };
        assert!(e.to_string().contains("unknown timezone"), "{e}");
        assert_eq!(ScheduleConfig::parse_quiz_time(" 9:05 "), Some((9, 5)));
        assert_eq!(ScheduleConfig::parse_quiz_time("9"), None);
    }

    #[test]
    fn trivia_defaults_enable_recent_category_history() {
        let config = TriviaConfig::default();
        assert_eq!(config.batch_size, 10);
        assert_eq!(config.recent_category_window, 5);
    }
}
