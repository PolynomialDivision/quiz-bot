use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, VecDeque}, path::Path};

/// A one-time quiz scheduled via `!schedulequiz`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScheduledOnce {
    /// Quiz *start* time as "HH:MM" in the configured timezone.
    pub quiz_time: String,
    /// The calendar date on which this quiz should fire.
    pub date: NaiveDate,
}

// ── Persistent state (operational, not analytics) ─────────────────────────────
//
// Analytics data lives in SQLite (quiz.db).  This file holds only the ephemeral
// operational state that the bot needs across restarts.

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    /// Pre-fetched questions from OpenTDB waiting to be used.
    #[serde(default)]
    pub cached_questions: VecDeque<FetchedQuestion>,
    /// OpenTDB session token — tracks which questions have been served so we
    /// don't repeat until all are exhausted.
    #[serde(default)]
    pub opentdb_token: Option<String>,
    /// Set on first boot.
    pub created_at: Option<DateTime<Utc>>,
    /// Per-slot last-fired date.  Key = "HH:MM" time string from config.
    #[serde(default)]
    pub last_quiz_dates: HashMap<String, NaiveDate>,
    /// One-time quizzes added via `!schedulequiz`.
    /// Entries are removed by the scheduler the moment they fire.
    #[serde(default)]
    pub scheduled_once: Vec<ScheduledOnce>,
}

/// A question as returned by the OpenTDB API (decoded).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FetchedQuestion {
    pub category:          String,
    pub difficulty:        String,
    pub question:          String,
    pub correct_answer:    String,
    pub incorrect_answers: Vec<String>,
}

// ── I/O ───────────────────────────────────────────────────────────────────────

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        if tokio::fs::metadata(path).await.is_ok() {
            let s = tokio::fs::read_to_string(path).await?;
            Ok(serde_json::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_string_pretty(self)?).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "quiz-bot-state-test-{name}-{}-{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[tokio::test]
    async fn opentdb_token_survives_a_save_and_load_round_trip() {
        // The OpenTDB session token must be requested at most once per
        // bot lifetime, not once per question — this is what lets a fresh
        // `State::load` after a restart pick the persisted token back up
        // instead of `ensure_token` requesting a new one.
        let path = temp_path("token-roundtrip");
        let state = State {
            opentdb_token: Some("persisted-token".to_owned()),
            ..State::default()
        };
        state.save(&path).await.unwrap();

        let loaded = State::load(&path).await.unwrap();
        assert_eq!(loaded.opentdb_token.as_deref(), Some("persisted-token"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn missing_state_file_defaults_to_no_cached_token() {
        let path = temp_path("missing");
        let loaded = State::load(&path).await.unwrap();
        assert_eq!(loaded.opentdb_token, None);
    }
}
