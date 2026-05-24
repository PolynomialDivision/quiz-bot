use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet, VecDeque}, path::Path};

// ── Persistent state ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    /// Pre-fetched questions from OpenTDB waiting to be used.
    #[serde(default)]
    pub cached_questions: VecDeque<FetchedQuestion>,
    /// OpenTDB session token — tracks which questions have been served so we
    /// don't repeat until all are exhausted.
    #[serde(default)]
    pub opentdb_token: Option<String>,
    /// Full history of completed quiz rounds.
    pub results: Vec<QuizResult>,
    /// Set on first boot; used as the statistics baseline.
    pub created_at: Option<DateTime<Utc>>,
    /// Per-slot last-fired date.  Key = "HH:MM" time string from config.
    /// Written at quiz-start so a crash during the reminder doesn't re-fire.
    #[serde(default)]
    pub last_quiz_dates: HashMap<String, NaiveDate>,
}

/// A question as returned by the OpenTDB API (HTML-decoded).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FetchedQuestion {
    pub category: String,
    pub difficulty: String,
    pub question: String,
    pub correct_answer: String,
    pub incorrect_answers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QuizResult {
    /// Identifies which round this question belongs to.
    /// Generated once per `start_quiz` call as a Unix-ms timestamp.
    pub round_id: u64,
    pub question_text: String,
    pub category: String,
    pub difficulty: String,
    pub correct_answer: String,
    pub correct_index: u8,
    pub asked_at: DateTime<Utc>,
    /// user_id → choice_index.
    pub answers: HashMap<String, u8>,
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

// ── Statistics ────────────────────────────────────────────────────────────────

impl State {
    /// Returns a map of user_id → (correct_count, total_questions).
    ///
    /// If a user answered at least one question in a round, every unanswered
    /// question in that round counts against them as wrong.
    pub fn user_stats(&self) -> HashMap<String, (u32, u32)> {
        let mut map: HashMap<String, (u32, u32)> = HashMap::new();

        // Group questions by round.
        let mut rounds: HashMap<u64, Vec<&QuizResult>> = HashMap::new();
        for result in &self.results {
            rounds.entry(result.round_id).or_default().push(result);
        }

        for questions in rounds.values() {
            // Every user who answered at least one question in this round.
            let participants: HashSet<&str> = questions
                .iter()
                .flat_map(|r| r.answers.keys().map(String::as_str))
                .collect();

            let n = questions.len() as u32;
            for user in participants {
                let e = map.entry(user.to_owned()).or_default();
                e.1 += n;
                for result in questions {
                    if result.answers.get(user) == Some(&result.correct_index) {
                        e.0 += 1;
                    }
                }
            }
        }

        map
    }

    /// Sorted leaderboard: (user_id, correct, total), best first.
    ///
    /// Sort order:
    ///   1. accuracy (correct / total) — descending
    ///   2. tie: more correct answers — descending
    ///   3. tie: more total answers played — descending
    ///   4. tie: username — ascending (alphabetical)
    pub fn leaderboard(&self) -> Vec<(String, u32, u32)> {
        let mut board: Vec<(String, u32, u32)> = self
            .user_stats()
            .into_iter()
            .map(|(u, (c, t))| (u, c, t))
            .collect();
        board.sort_by(|a, b| {
            let pct_a = if a.2 > 0 { a.1 as f64 / a.2 as f64 } else { 0.0 };
            let pct_b = if b.2 > 0 { b.1 as f64 / b.2 as f64 } else { 0.0 };
            pct_b.partial_cmp(&pct_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.cmp(&a.1))   // more correct answers
                .then(b.2.cmp(&a.2))   // more total answers
                .then(a.0.cmp(&b.0))   // alphabetical
        });
        board
    }
}
