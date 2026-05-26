//! SQLite persistence layer via rusqlite + spawn_blocking.
//!
//! The database file lives at `{STORE_PATH}/quiz.db` — same directory as
//! state.json.  No separate process or configuration required.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::{Arc, Mutex}};
use tracing::info;

// ── Pool wrapper ──────────────────────────────────────────────────────────────

pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

// ── Result types ──────────────────────────────────────────────────────────────

pub struct LeaderboardEntry {
    pub user_id:         String,
    pub total_correct:   i64,
    pub total_questions: i64,
    pub rounds_played:   i64,
    /// Wilson score lower bound (95 % CI) — the sort key used by the leaderboard.
    /// Ranges 0..1; penalises small sample sizes so occasional lucky players
    /// don't outrank consistent regulars.
    pub wilson_score:    f64,
}

// ── Wilson score ──────────────────────────────────────────────────────────────

/// Lower bound of the Wilson score confidence interval for a binomial proportion.
///
/// Uses z = 1.96 (95 % confidence).  Returns 0 when `total == 0`.
///
/// Properties that make it fair for a game leaderboard:
/// * 5/5  correct (100 %) → ~0.57   (small sample, low confidence)
/// * 47/50 correct (94 %) → ~0.83   (large sample, high confidence)
/// * 0/0  answered        →  0.00   (no data)
pub fn wilson_lower_bound(correct: i64, total: i64) -> f64 {
    if total == 0 { return 0.0; }
    let n  = total   as f64;
    let p  = correct as f64 / n;
    let z  = 1.96_f64;
    let z2 = z * z;
    let centre_adj   = z2 / (2.0 * n);
    let margin       = z * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt());
    let denominator  = 1.0 + z2 / n;
    (p + centre_adj - margin) / denominator
}

pub struct UserStatsRow {
    pub total_correct:   i64,
    pub total_questions: i64,
    pub rounds_played:   i64,
}

pub struct CategoryStat {
    pub category:        String,
    pub questions_asked: i64,
    pub total_answers:   i64,
    pub correct_answers: i64,
}

pub struct SpeedEntry {
    pub user_id:      String,
    pub avg_secs:     f64,
    pub sample_count: i64,
}

/// Users who have 1–2 correct answers (below the 3-sample speed threshold).
pub struct SpeedNearEntry {
    pub user_id:      String,
    pub correct_count: i64,
}

pub struct UserCategoryStat {
    pub category: String,
    pub answered: i64,
    pub correct:  i64,
}

/// A single user's final answer including timing metadata.
#[derive(Clone, Debug)]
pub struct AnswerRecord {
    pub choice:         u8,
    pub source:         &'static str,   // 'reaction' | 'text' | 'reconciled'
    pub submitted_at:   DateTime<Utc>,
    pub changed_answer: bool,
}

// ── Spawn-blocking helper ─────────────────────────────────────────────────────

impl Db {
    /// Run a synchronous closure on the connection inside spawn_blocking.
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| anyhow::anyhow!("DB mutex: {e}"))?;
            f(&mut conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))?
    }
}

// ── Open & migrate ────────────────────────────────────────────────────────────

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path)
                .with_context(|| format!("Opening SQLite at {}", path.display()))?;
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA synchronous=NORMAL;",
            )?;
            Ok(conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub async fn migrate(&self) -> Result<()> {
        self.run(|conn| {
            conn.execute_batch(include_str!("../migrations/001_initial.sql"))
                .context("Applying DB schema")?;
            conn.execute_batch(include_str!("../migrations/002_fix_round_totals.sql"))
                .context("Backfilling round_scores.total_count")?;
            Ok(())
        })
        .await?;
        info!("DB schema applied");
        Ok(())
    }
}

// ── Key-value helpers ─────────────────────────────────────────────────────────

impl Db {
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        self.run(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT value FROM bot_kv WHERE key = ?1",
            )?;
            let mut rows = stmt.query(params![key])?;
            Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
        })
        .await
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO bot_kv (key, value, updated_at)
                 VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 ON CONFLICT (key) DO UPDATE
                   SET value = excluded.value,
                       updated_at = excluded.updated_at",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }
}

// ── Player registry ───────────────────────────────────────────────────────────

impl Db {
    pub async fn upsert_player(&self, user_id: &str, display_name: Option<&str>) -> Result<()> {
        let user_id      = user_id.to_owned();
        let display_name = display_name.map(str::to_owned);
        self.run(move |conn| {
            match &display_name {
                Some(name) => conn.execute(
                    "INSERT INTO players (user_id, display_name)
                     VALUES (?1, ?2)
                     ON CONFLICT (user_id) DO UPDATE
                       SET display_name = excluded.display_name,
                           last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                    params![user_id, name],
                )?,
                None => conn.execute(
                    "INSERT INTO players (user_id)
                     VALUES (?1)
                     ON CONFLICT (user_id) DO UPDATE
                       SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                    params![user_id],
                )?,
            };
            Ok(())
        })
        .await
    }

    pub async fn update_display_names(&self, names: &HashMap<String, String>) -> Result<()> {
        for (user_id, display_name) in names {
            self.upsert_player(user_id, Some(display_name)).await?;
        }
        Ok(())
    }
}

// ── Rounds ────────────────────────────────────────────────────────────────────

pub struct RoundParams<'a> {
    pub room_id:                    &'a str,
    pub n_questions_planned:        i32,
    pub triggered_by:               &'a str,
    pub config_answer_timeout:      i32,
    pub config_questions_per_round: i32,
    pub config_timezone:            &'a str,
    pub config_category_id:         Option<i32>,
    pub config_difficulty:          Option<&'a str>,
}

impl Db {
    pub async fn create_round(&self, p: &RoundParams<'_>) -> Result<i64> {
        let room_id         = p.room_id.to_owned();
        let n_planned       = p.n_questions_planned;
        let triggered_by    = p.triggered_by.to_owned();
        let timeout         = p.config_answer_timeout;
        let n_per_round     = p.config_questions_per_round;
        let timezone        = p.config_timezone.to_owned();
        let category_id     = p.config_category_id;
        let difficulty      = p.config_difficulty.map(str::to_owned);

        self.run(move |conn| {
            conn.execute(
                "INSERT INTO rounds
                   (room_id, n_questions_planned, triggered_by,
                    config_answer_timeout, config_questions_per_round,
                    config_timezone, config_category_id, config_difficulty)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![room_id, n_planned, triggered_by,
                         timeout, n_per_round, timezone, category_id, difficulty],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn finish_round(&self, round_id: i64, n_actual: i32) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE rounds
                 SET ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                     n_questions_actual = ?1
                 WHERE id = ?2",
                params![n_actual, round_id],
            )?;
            Ok(())
        })
        .await
    }
}

// ── Questions ─────────────────────────────────────────────────────────────────

pub struct QuestionParams<'a> {
    pub round_id:            i64,
    pub question_num:        i32,
    pub matrix_event_id:     Option<&'a str>,
    pub category:            &'a str,
    pub difficulty:          &'a str,
    pub question_text:       &'a str,
    pub choices:             &'a [String],
    pub correct_index:       i16,
    pub correct_answer_text: &'a str,
    pub answer_timeout_secs: i32,
}

impl Db {
    pub async fn insert_question(&self, p: &QuestionParams<'_>) -> Result<i64> {
        let round_id        = p.round_id;
        let question_num    = p.question_num;
        let matrix_event_id = p.matrix_event_id.map(str::to_owned);
        let category        = p.category.to_owned();
        let difficulty      = p.difficulty.to_owned();
        let question_text   = p.question_text.to_owned();
        let choices_json    = serde_json::to_string(p.choices)?;
        let correct_index   = p.correct_index as i32;
        let correct_answer  = p.correct_answer_text.to_owned();
        let timeout         = p.answer_timeout_secs;

        self.run(move |conn| {
            conn.execute(
                "INSERT INTO questions
                   (round_id, question_num, matrix_event_id,
                    category, difficulty, question_text,
                    choices, correct_index, correct_answer_text,
                    answer_timeout_secs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![round_id, question_num, matrix_event_id,
                         category, difficulty, question_text,
                         choices_json, correct_index, correct_answer, timeout],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn update_question_stats(
        &self,
        question_id: i64,
        n_answers:   i32,
        n_correct:   i32,
        n_wrong:     i32,
    ) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE questions
                 SET n_answers_received = ?1, n_correct = ?2, n_wrong = ?3
                 WHERE id = ?4",
                params![n_answers, n_correct, n_wrong, question_id],
            )?;
            Ok(())
        })
        .await
    }
}

// ── Answers ───────────────────────────────────────────────────────────────────

impl Db {
    pub async fn insert_answers(
        &self,
        question_id:   i64,
        round_id:      i64,
        answers:       &HashMap<String, AnswerRecord>,
        correct_index: u8,
    ) -> Result<()> {
        // Upsert players first (best-effort).
        for user_id in answers.keys() {
            self.upsert_player(user_id, None).await.ok();
        }

        let answers = answers.clone();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, rec) in &answers {
                let is_correct     = (rec.choice == correct_index) as i32;
                let choice_i       = rec.choice as i32;
                let changed_answer = rec.changed_answer as i32;
                let submitted_at   = rec.submitted_at.to_rfc3339();
                tx.execute(
                    "INSERT INTO answers
                       (question_id, round_id, user_id,
                        choice_index, is_correct, source,
                        submitted_at, changed_answer)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (question_id, user_id) DO UPDATE
                       SET choice_index   = excluded.choice_index,
                           is_correct     = excluded.is_correct,
                           source         = excluded.source,
                           submitted_at   = excluded.submitted_at,
                           changed_answer = excluded.changed_answer",
                    params![question_id, round_id, user_id,
                             choice_i, is_correct, rec.source,
                             submitted_at, changed_answer],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

// ── Round scores ──────────────────────────────────────────────────────────────

impl Db {
    pub async fn write_round_scores(
        &self,
        round_id: i64,
        scores:   &HashMap<String, (u32, u32)>,
    ) -> Result<()> {
        let scores = scores.clone();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, &(correct, total)) in &scores {
                tx.execute(
                    "INSERT INTO round_scores (round_id, user_id, correct_count, total_count)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (round_id, user_id) DO UPDATE
                       SET correct_count = excluded.correct_count,
                           total_count   = excluded.total_count",
                    params![round_id, user_id, correct as i64, total as i64],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

// ── Stats queries ─────────────────────────────────────────────────────────────

impl Db {
    pub async fn leaderboard(&self) -> Result<Vec<LeaderboardEntry>> {
        let mut entries = self.run(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     user_id,
                     SUM(correct_count) AS total_correct,
                     SUM(total_count)   AS total_questions,
                     COUNT(*)           AS rounds_played
                 FROM round_scores
                 GROUP BY user_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await?;

        // Compute Wilson score and sort in Rust — SQLite lacks sqrt() by default.
        let mut result: Vec<LeaderboardEntry> = entries
            .drain(..)
            .map(|(user_id, correct, total, rounds)| LeaderboardEntry {
                wilson_score:    wilson_lower_bound(correct, total),
                user_id,
                total_correct:   correct,
                total_questions: total,
                rounds_played:   rounds,
            })
            .collect();

        result.sort_by(|a, b| {
            b.wilson_score
                .partial_cmp(&a.wilson_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Tie-break: more questions answered first, then alphabetical.
                .then(b.total_questions.cmp(&a.total_questions))
                .then(a.user_id.cmp(&b.user_id))
        });

        Ok(result)
    }

    pub async fn user_stats(&self, user_id: &str) -> Result<Option<UserStatsRow>> {
        let user_id = user_id.to_owned();
        self.run(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     COALESCE(SUM(correct_count), 0) AS total_correct,
                     COALESCE(SUM(total_count), 0)   AS total_questions,
                     COUNT(*)                        AS rounds_played
                 FROM round_scores
                 WHERE user_id = ?1",
            )?;
            let row = stmt.query_row(params![user_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            if row.1 == 0 {
                return Ok(None);
            }
            Ok(Some(UserStatsRow {
                total_correct:   row.0,
                total_questions: row.1,
                rounds_played:   row.2,
            }))
        })
        .await
    }

    pub async fn round_count(&self) -> Result<i64> {
        self.run(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM rounds WHERE ended_at IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn question_count(&self) -> Result<i64> {
        self.run(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM questions", [], |r| r.get(0))?)
        })
        .await
    }

    pub async fn question_exists(&self, question_text: &str) -> Result<bool> {
        let question_text = question_text.to_owned();
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM questions WHERE question_text = ?1)",
                params![question_text],
                |r| r.get::<_, bool>(0),
            )?)
        })
        .await
    }
}

// ── Extended stats queries ────────────────────────────────────────────────────

impl Db {
    /// Per-category question counts and answer accuracy.
    pub async fn category_stats(&self) -> Result<Vec<CategoryStat>> {
        self.run(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     category,
                     COUNT(*)                            AS questions_asked,
                     COALESCE(SUM(n_answers_received),0) AS total_answers,
                     COALESCE(SUM(n_correct),0)          AS correct_answers
                 FROM questions
                 GROUP BY category
                 ORDER BY questions_asked DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(CategoryStat {
                    category:        r.get(0)?,
                    questions_asked: r.get(1)?,
                    total_answers:   r.get(2)?,
                    correct_answers: r.get(3)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }

    /// Average seconds from question post to correct answer, per user.
    /// Only includes users with at least 3 correct answers.
    pub async fn speed_leaderboard(&self) -> Result<Vec<SpeedEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     a.user_id,
                     AVG((julianday(a.submitted_at) - julianday(q.asked_at)) * 86400.0) AS avg_secs,
                     COUNT(*) AS sample_count
                 FROM answers a
                 JOIN questions q ON a.question_id = q.id
                 WHERE a.is_correct = 1
                   AND a.submitted_at > q.asked_at
                 GROUP BY a.user_id
                 HAVING COUNT(*) >= 3
                 ORDER BY avg_secs ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SpeedEntry {
                    user_id:      r.get(0)?,
                    avg_secs:     r.get(1)?,
                    sample_count: r.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }

    /// Users with 1 or 2 correct answers — close to the 3-sample speed threshold.
    pub async fn speed_near_threshold(&self) -> Result<Vec<SpeedNearEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT a.user_id, COUNT(*) AS correct_count
                 FROM answers a
                 JOIN questions q ON a.question_id = q.id
                 WHERE a.is_correct = 1
                   AND a.submitted_at > q.asked_at
                 GROUP BY a.user_id
                 HAVING COUNT(*) < 3
                 ORDER BY correct_count DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SpeedNearEntry {
                    user_id:       r.get(0)?,
                    correct_count: r.get(1)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }

    /// Per-category accuracy for a single user (categories with ≥ 2 answers).
    pub async fn user_category_stats(&self, user_id: &str) -> Result<Vec<UserCategoryStat>> {
        let user_id = user_id.to_owned();
        self.run(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT
                     q.category,
                     COUNT(*)         AS answered,
                     SUM(a.is_correct) AS correct
                 FROM answers a
                 JOIN questions q ON a.question_id = q.id
                 WHERE a.user_id = ?1
                 GROUP BY q.category
                 HAVING COUNT(*) >= 2
                 ORDER BY CAST(SUM(a.is_correct) AS REAL) / COUNT(*) DESC",
            )?;
            let rows = stmt.query_map(params![user_id], |r| {
                Ok(UserCategoryStat {
                    category: r.get(0)?,
                    answered: r.get(1)?,
                    correct:  r.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!(e))
        })
        .await
    }
}

// ── Reset ─────────────────────────────────────────────────────────────────────

impl Db {
    pub async fn reset_stats(&self) -> Result<()> {
        self.run(|conn| {
            conn.execute_batch(
                "DELETE FROM answers;
                 DELETE FROM round_scores;
                 DELETE FROM questions;
                 DELETE FROM rounds;
                 DELETE FROM players;",
            )?;
            Ok(())
        })
        .await
    }
}
