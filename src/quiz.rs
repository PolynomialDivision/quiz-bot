use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use chrono_tz::Tz;
use matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::{Annotation, Thread},
            room::{
                message::{Relation, ReplacementMetadata, RoomMessageEventContent},
                ImageInfo,
            },
        },
        OwnedEventId, UInt,
    },
    Client, Room,
};
use rand::seq::SliceRandom;
use tracing::{error, info, warn};

use crate::{
    db::{self, AnswerRecord},
    fetcher, state, BotContext,
};

use matrix_sdk::ruma::events::Mentions;

/// Look up the display names of `user_ids` from room state.
async fn fetch_names(room: &Room, user_ids: &[&str]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for &uid_str in user_ids {
        if let Ok(uid) = matrix_sdk::ruma::OwnedUserId::try_from(uid_str) {
            if let Ok(Some(member)) = room.get_member(&uid).await {
                let name = member
                    .display_name()
                    .unwrap_or_else(|| member.user_id().localpart())
                    .to_owned();
                map.insert(uid_str.to_owned(), name);
            }
        }
    }
    map
}

pub const CHOICE_EMOJIS: [&str; 4] = ["🇦", "🇧", "🇨", "🇩"];

// ── Active quiz state (in-memory only) ───────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ActiveQuiz {
    pub event_id: OwnedEventId,
    /// Per-user answer records.  `record_answer` handles change-tracking.
    pub answers: HashMap<String, AnswerRecord>,
    pub correct_index: u8,
}

/// Outcome of `ActiveQuiz::record_answer`, so callers (the reaction/text
/// answer handlers in `main.rs`) can log precisely what happened rather than
/// silently writing into the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// First answer recorded for this user on this question.
    New,
    /// The user had already answered; this changed their recorded choice.
    Replaced { previous: u8 },
    /// The user had already answered this same choice — a no-op duplicate.
    Unchanged,
}

impl ActiveQuiz {
    /// Record or update a user's answer.  Sets `changed_answer = true` when
    /// the user picks a different option than their previous one.
    pub fn record_answer(
        &mut self,
        user_id: String,
        choice: u8,
        source: &'static str,
    ) -> AnswerOutcome {
        let now = chrono::Utc::now();
        let mut outcome = AnswerOutcome::New;
        self.answers
            .entry(user_id)
            .and_modify(|r| {
                outcome = if r.choice == choice {
                    AnswerOutcome::Unchanged
                } else {
                    let previous = r.choice;
                    r.changed_answer = true;
                    AnswerOutcome::Replaced { previous }
                };
                r.choice = choice;
                r.source = source;
                r.submitted_at = now;
            })
            .or_insert(AnswerRecord {
                choice,
                source,
                submitted_at: now,
                changed_answer: false,
            });
        outcome
    }
}

/// What happened to an incoming answer reaction, for the caller to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionResult {
    /// It targeted the active question and was recorded.
    Accepted(AnswerOutcome),
    /// A quiz question is active, but this reaction targets a different
    /// (older) event — e.g. a late reaction to a previous question.
    WrongQuestion,
    /// No quiz question is currently active at all.
    NoActiveQuestion,
}

/// Apply an incoming answer reaction to whatever question is currently
/// active, if any. Pure aside from mutating `active_quiz` in place (no I/O),
/// so the acceptance rules — matching the active question, ignoring a stale
/// one, ignoring reactions with nothing active — are unit-testable without a
/// live Matrix room or event.
pub fn apply_reaction(
    active_quiz: &mut Option<ActiveQuiz>,
    reacted_to: &OwnedEventId,
    sender: String,
    choice: u8,
) -> ReactionResult {
    match active_quiz {
        Some(quiz) if &quiz.event_id == reacted_to => {
            ReactionResult::Accepted(quiz.record_answer(sender, choice, "reaction"))
        }
        Some(_) => ReactionResult::WrongQuestion,
        None => ReactionResult::NoActiveQuestion,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn shuffle_choices(q: &state::FetchedQuestion) -> (Vec<String>, u8) {
    let mut choices: Vec<String> = q
        .incorrect_answers
        .iter()
        .cloned()
        .chain(std::iter::once(q.correct_answer.clone()))
        .collect();
    choices.shuffle(&mut rand::thread_rng());
    let correct_index = choices
        .iter()
        .position(|c| c == &q.correct_answer)
        .unwrap_or(0) as u8;
    (choices, correct_index)
}

/// How many questions this round should actually run for, given how many
/// were successfully preloaded and validated ahead of time. `None` means the
/// round has nothing to work with and should be skipped rather than started
/// with zero questions.
fn resolve_round_length(gathered: usize, configured: u32) -> Option<u32> {
    let len = (gathered as u32).min(configured);
    (len > 0).then_some(len)
}

fn difficulty_icon(d: &str) -> &'static str {
    match d {
        "easy" => "🟢",
        "medium" => "🟡",
        "hard" => "🔴",
        _ => "⚪",
    }
}

fn format_countdown(secs: u64) -> String {
    let mins = secs / 60;
    if mins >= 1 {
        format!("{} minute{}", mins, if mins == 1 { "" } else { "s" })
    } else {
        format!("{} seconds", secs)
    }
}

fn time_bar(remaining: u64, total: u64) -> String {
    const W: usize = 10;
    let filled = if total > 0 {
        (remaining * W as u64 / total) as usize
    } else {
        0
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(W - filled))
}

fn question_text(
    q_num: u32,
    n_questions: u32,
    fetched: &state::FetchedQuestion,
    choices: &[String],
    total_secs: u64,
    remaining_secs: u64,
) -> String {
    let icon = difficulty_icon(&fetched.difficulty);
    let bar = time_bar(remaining_secs, total_secs);
    let mut lines = vec![
        format!(
            "❓ Q{q_num}/{n_questions} | {icon} {} · {} | ⏳ {remaining_secs}s {bar}",
            fetched.difficulty, fetched.category,
        ),
        String::new(),
        fetched.question.clone(),
        String::new(),
    ];
    for (i, choice) in choices.iter().enumerate() {
        lines.push(format!("{}  {}", CHOICE_EMOJIS[i], choice));
    }
    // No "React with …" footer — the bot adds reactions directly so users just tap.
    lines.join("\n")
}

fn make_edit(event_id: OwnedEventId, text: &str) -> RoomMessageEventContent {
    RoomMessageEventContent::text_plain(text)
        .make_replacement(ReplacementMetadata::new(event_id, None))
}

/// Post the round's question message, retrying transient send failures.
///
/// This is the one send in the round loop that the round cannot proceed
/// without. Observed in production: a homeserver-side hiccup (sync/auth
/// 503s) can fail a single request for ~30 s before recovering on its own.
/// Without a retry here, that single failed request used to abort the
/// entire round via `?` — losing every remaining question and leaving the
/// round's DB record permanently unfinished. Mirrors the retry shape already
/// used for OpenTDB requests in fetcher.rs.
async fn send_question_with_retry(
    room: &Room,
    content: &RoomMessageEventContent,
) -> anyhow::Result<OwnedEventId> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(30); // 1 s, 2 s, 4 s, 8 s, 16 s
            warn!("Posting question failed — retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }
        match room.send(content.clone()).await {
            Ok(resp) => return Ok(resp.response.event_id),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.expect("loop runs MAX_ATTEMPTS >= 1 time"))
        .context(format!("failed to post question after {MAX_ATTEMPTS} attempts"))
}

// ── Reaction reconciliation ───────────────────────────────────────────────────

/// Fetch one page of reactions related to `q_event_id`, retrying transient
/// failures. This query is the one mechanism that recovers a reaction the
/// live sync stream missed (e.g. it arrived while `start_quiz` was already
/// draining `active_quiz` for evaluation) — a single failed request here
/// must not silently disable it. Mirrors the retry shape used elsewhere
/// (see `send_question_with_retry`).
///
/// Uses `Room::relations`, which transparently decrypts each returned event,
/// filtering only by relation type (`m.annotation`) rather than by the
/// plaintext `m.reaction` event type. In an encrypted room, reactions travel
/// wrapped as `m.room.encrypted` at the wire level, so a plaintext
/// event-type filter — as this used to do via a raw, non-decrypting API
/// call — matches nothing and silently returns an empty page. Filtering
/// client-side (below) after decryption works regardless of room encryption.
async fn fetch_relations_page(
    room: &Room,
    q_event_id: &OwnedEventId,
    from: Option<String>,
) -> matrix_sdk::Result<matrix_sdk::room::Relations> {
    use matrix_sdk::room::{IncludeRelations, RelationsOptions};
    use matrix_sdk::ruma::events::relation::RelationType;

    const MAX_ATTEMPTS: u32 = 4;
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(30); // 1 s, 2 s, 4 s
            warn!(
                question_event_id = %q_event_id,
                "Reaction reconciliation query failed — retry {attempt}/{MAX_ATTEMPTS} in {delay}s"
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }
        let options = RelationsOptions {
            from: from.clone(),
            include_relations: IncludeRelations::RelationsOfType(RelationType::Annotation),
            ..Default::default()
        };
        match room.relations(q_event_id.clone(), options).await {
            Ok(relations) => return Ok(relations),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.expect("loop runs MAX_ATTEMPTS >= 1 time"))
}

/// Fetch all reactions from the server after the countdown and merge them
/// into the in-memory answer map. This is the authoritative check at
/// evaluation time: it doesn't matter whether the live sync stream had
/// already delivered (and processed) a given reaction by the time the
/// question closed, only whether the homeserver has it. Users found only on
/// the server (missed on the stream) are added with source "reconciled" and
/// submitted_at = now.
async fn reconcile_reactions(
    client: &Client,
    room: &Room,
    q_event_id: &OwnedEventId,
    answers: &mut HashMap<String, AnswerRecord>,
) {
    use matrix_sdk::ruma::events::AnyMessageLikeEvent;

    let mut server_answers: HashMap<String, u8> = HashMap::new();
    let mut from: Option<String> = None;
    let mut fully_synced = false;
    let mut undecryptable = 0u32;

    loop {
        let relations = match fetch_relations_page(room, q_event_id, from.clone()).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    question_event_id = %q_event_id,
                    "Reaction reconciliation failed after retries — evaluating with \
                     live-stream answers only: {e}"
                );
                break;
            }
        };

        for event in &relations.chunk {
            if event.kind.is_utd() {
                undecryptable += 1;
                continue;
            }
            let Ok(AnyMessageLikeEvent::Reaction(ev)) =
                event.kind.raw().deserialize_as_unchecked::<AnyMessageLikeEvent>()
            else {
                continue;
            };
            let Some(orig) = ev.as_original() else {
                continue; // Redacted — the user un-reacted; correctly excluded.
            };
            // Skip the bot's own 🇦🇧🇨🇩 reactions it posted for tap-to-answer.
            if client
                .user_id()
                .map(|id| id == orig.sender)
                .unwrap_or(false)
            {
                continue;
            }
            let choice = match orig.content.relates_to.key.as_str() {
                "🇦" => 0u8,
                "🇧" => 1,
                "🇨" => 2,
                "🇩" => 3,
                _ => continue,
            };
            // `Room::relations` defaults to backward (most-recent-first)
            // order, so the first entry seen per sender is their latest
            // reaction — relevant if a client doesn't auto-redact a
            // superseded reaction before sending the new one.
            server_answers
                .entry(orig.sender.as_str().to_owned())
                .or_insert(choice);
        }

        match relations.next_batch_token {
            Some(token) => from = Some(token),
            None => {
                fully_synced = true;
                break;
            }
        }
    }

    if undecryptable > 0 {
        warn!(
            question_event_id = %q_event_id,
            count = undecryptable,
            "Some reactions to this question could not be decrypted and were ignored"
        );
    }

    if !fully_synced {
        // Only a partial (or no) view of the server's reaction state is
        // available. Trusting it would risk *dropping* answers the live
        // stream already recorded correctly — worse than doing nothing.
        return;
    }

    let before = answers.len();
    let summary = merge_reconciled_answers(answers, &server_answers, chrono::Utc::now());

    if !summary.is_empty() {
        info!(
            question_event_id = %q_event_id,
            before,
            after = answers.len(),
            added = ?summary.added,
            corrected = ?summary.corrected,
            removed = ?summary.removed,
            "Reconciled reactions against server state"
        );
    }
}

/// What `merge_reconciled_answers` changed, for logging.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReconciliationSummary {
    /// Users present on the server but missing from the live stream —
    /// e.g. a reaction that arrived just before the deadline and hadn't
    /// been processed (or was processed after `active_quiz` was already
    /// drained) when the countdown ended.
    added: Vec<String>,
    /// Users whose live-stream answer didn't match the server's current
    /// reaction (e.g. they changed their answer via a second reaction that
    /// their client didn't redact the first one for).
    corrected: Vec<String>,
    /// Users whose reaction-sourced answer was recorded live but is no
    /// longer present on the server (they un-reacted before the deadline).
    removed: Vec<String>,
}

impl ReconciliationSummary {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.corrected.is_empty() && self.removed.is_empty()
    }
}

/// Merge the server's authoritative reaction state (`server_answers`) into
/// the live-stream-collected `answers`, in place. Pure aside from the
/// caller-supplied `now` (used as `submitted_at` for answers added here),
/// so the merge rules can be unit-tested without a network round trip.
///
/// The stream never sees `m.room.redaction` events, so it can hold a stale
/// reaction answer after the user removed it — the server's current
/// reaction set is authoritative. Rules:
///  • Reaction answer + user gone from server  → remove (they un-reacted)
///  • Text answer (!a/!b/…) + not on server    → keep (text can't be un-sent)
///  • Reaction answer + server has same choice → keep stream (timestamp intact)
///  • Reaction answer + server has diff choice → server wins (missed redact + re-react)
///  • User missing from stream entirely        → add from server
fn merge_reconciled_answers(
    answers: &mut HashMap<String, AnswerRecord>,
    server_answers: &HashMap<String, u8>,
    now: chrono::DateTime<chrono::Utc>,
) -> ReconciliationSummary {
    let removed: Vec<String> = answers
        .iter()
        .filter(|(user_id, rec)| rec.source != "text" && !server_answers.contains_key(*user_id))
        .map(|(user_id, _)| user_id.clone())
        .collect();
    answers.retain(|user_id, rec| rec.source == "text" || server_answers.contains_key(user_id));

    let mut added = Vec::new();
    let mut corrected = Vec::new();
    for (user_id, &server_choice) in server_answers {
        answers
            .entry(user_id.clone())
            .and_modify(|r| {
                // Don't touch text answers — they're always final.
                if r.source != "text" && r.choice != server_choice {
                    r.choice = server_choice;
                    r.source = "reconciled";
                    r.changed_answer = true;
                    corrected.push(user_id.clone());
                }
            })
            .or_insert_with(|| {
                added.push(user_id.clone());
                AnswerRecord {
                    choice: server_choice,
                    source: "reconciled",
                    submitted_at: now,
                    changed_answer: false,
                }
            });
    }

    ReconciliationSummary { added, corrected, removed }
}

// ── Quiz runner ───────────────────────────────────────────────────────────────

pub async fn start_quiz(
    ctx: BotContext,
    client: Client,
    skip_reminder: bool,
    slot_key: Option<String>,
) -> anyhow::Result<()> {
    let _run_guard = Arc::clone(&ctx.quiz_run_lock)
        .try_lock_owned()
        .map_err(|_| anyhow::anyhow!("a quiz round is already running"))?;
    let n_questions = ctx.config.schedule.questions_per_round.max(1);
    let timeout = ctx.config.schedule.answer_timeout_secs;
    let inter_pause = ctx.config.schedule.inter_question_secs;
    // Reminders sorted descending so we fire the earliest one first.
    let mut reminders = ctx.config.schedule.reminder_before_secs.clone();
    reminders.sort_unstable_by(|a, b| b.cmp(a));
    reminders.dedup();

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("Quiz: bot not in room {}", ctx.room_id);
            return Ok(());
        }
    };

    // ── Pre-fetch questions (runs concurrently with the reminder sleep) ───────
    //
    // Spawned here so the API calls (one per category, 6 s apart) happen
    // during the reminder window rather than after it.  If there is no
    // reminder the task still runs in the background while we create the DB
    // round, which reduces (but may not eliminate) the wait before Q1.
    let prefetch_handle = {
        let ctx2 = ctx.clone();
        let n = n_questions as usize;
        tokio::spawn(async move { fetcher::fetch_round_questions(&ctx2, n).await })
    };

    // ── Reminders ─────────────────────────────────────────────────────────────
    // reminders is sorted descending, e.g. [300, 60].
    // We fire each one in order, sleeping the gap to the next, then sleeping
    // the final interval to bring us exactly to quiz-start time.
    if !skip_reminder && !reminders.is_empty() {
        let qs = if n_questions == 1 {
            "question"
        } else {
            "questions"
        };
        for i in 0..reminders.len() {
            let secs_before = reminders[i];
            let time_str = format_countdown(secs_before);
            let plain =
                format!("🧠 Quiz starting in {time_str}! @room\n{n_questions} {qs} incoming.");
            let html  = format!("🧠 <strong>Quiz starting in {time_str}!</strong> @room<br>{n_questions} {qs} incoming.");
            let mut mentions = Mentions::new();
            mentions.room = true;
            room.send(RoomMessageEventContent::text_html(plain, html).add_mentions(mentions))
                .await
                .ok();
            let sleep_secs = if i + 1 < reminders.len() {
                reminders[i] - reminders[i + 1] // gap to next reminder
            } else {
                reminders[i] // final wait until quiz starts
            };
            tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)).await;
        }
    }

    let tz: Tz = ctx
        .config
        .schedule
        .timezone
        .parse()
        .unwrap_or(chrono_tz::UTC);
    let local_date = chrono::Utc::now().with_timezone(&tz).date_naive();
    let leaderboard_month = crate::leaderboard::YearMonth::containing(local_date);

    // ── Mark today for this scheduler slot ────────────────────────────────────
    // Marked regardless of prefetch outcome below: a slot only ever gets one
    // fire attempt per day (the scheduler's fire window is a single minute),
    // so there is nothing to gain by leaving it unmarked on failure, and
    // doing so before we know the outcome keeps a restart from double-firing.
    if let Some(ref key) = slot_key {
        let mut state = ctx.state.lock().await;
        state.last_quiz_dates.insert(key.clone(), local_date);
        if let Err(e) = state.save(&ctx.state_path).await {
            error!("Failed to persist last_quiz_dates: {e}");
        }
    }

    // ── Resolve the preloaded question set ────────────────────────────────────
    //
    // The round runs entirely off this set — no on-demand fetching happens
    // once the round starts. If OpenTDB couldn't supply the full count within
    // its retry/time budget (see `fetch_round_questions`), we shorten the
    // round to what's actually usable instead of discovering the shortfall
    // mid-round. A round that comes up completely empty is skipped outright.
    let questions: Vec<state::FetchedQuestion> = prefetch_handle.await.unwrap_or_default();
    let Some(round_len) = resolve_round_length(questions.len(), n_questions) else {
        error!("Round prefetch returned no usable questions — skipping this round");
        room.send(RoomMessageEventContent::text_plain(
            "⚠️ Couldn't prepare any questions for this round (OpenTDB unavailable) — skipping.",
        ))
        .await
        .ok();
        return Ok(());
    };
    if round_len < n_questions {
        warn!(
            "Round prefetch supplied {round_len}/{n_questions} questions — shortening round"
        );
    }

    // ── Create round in DB ────────────────────────────────────────────────────
    let triggered_by = slot_key
        .as_ref()
        .map(|k| format!("scheduler:{k}"))
        .unwrap_or_else(|| "manual".to_owned());

    let round_id = ctx
        .db
        .create_round(&db::RoundParams {
            room_id: ctx.room_id.as_str(),
            n_questions_planned: round_len as i32,
            triggered_by: &triggered_by,
            config_answer_timeout: timeout as i32,
            config_questions_per_round: n_questions as i32,
            config_timezone: &ctx.config.schedule.timezone,
            config_category_id: ctx.config.trivia.category.map(|c| c as i32),
            config_difficulty: ctx.config.trivia.difficulty.as_deref(),
        })
        .await?;

    let mut round_scores: HashMap<String, (u32, u32)> = HashMap::new();
    let mut questions_asked = 0u32;
    let mut questions_iter = questions.into_iter().take(round_len as usize);

    for q_num in 1..=round_len {
        // Guaranteed to succeed: `round_len` was capped to the number of
        // questions actually gathered above.
        let fetched = questions_iter
            .next()
            .expect("questions_iter has at least round_len items");

        let (choices, correct_index) = shuffle_choices(&fetched);
        let category_group = fetcher::category_group_label(&fetched.category);

        // ── Post question ─────────────────────────────────────────────────────
        let qt = question_text(q_num, round_len, &fetched, &choices, timeout, timeout);
        let initial_text = if q_num == 1 {
            format!("@room\n{qt}")
        } else {
            qt
        };
        let mut q_content = RoomMessageEventContent::text_plain(initial_text);
        if q_num == 1 {
            let mut m = Mentions::new();
            m.room = true;
            q_content = q_content.add_mentions(m);
        }
        let q_event_id = match send_question_with_retry(&room, &q_content).await {
            Ok(id) => id,
            Err(e) => {
                error!(
                    round_id,
                    q_num,
                    round_len,
                    questions_asked,
                    "Ending round early — could not post question after retries: {e:#}"
                );
                room.send(RoomMessageEventContent::text_plain(format!(
                    "⚠️ Lost connection to Matrix while posting Q{q_num}/{round_len} — \
                     ending round early ({questions_asked} question{} completed).",
                    if questions_asked == 1 { "" } else { "s" }
                )))
                .await
                .ok();
                break;
            }
        };
        info!("Q {q_num}/{round_len}: posted (event {q_event_id}, correct slot {correct_index})");

        // ── Insert question in DB ─────────────────────────────────────────────
        let question_id = ctx
            .db
            .insert_question(&db::QuestionParams {
                round_id,
                question_num: q_num as i32,
                matrix_event_id: Some(q_event_id.as_str()),
                category: &fetched.category,
                category_group: &category_group,
                difficulty: &fetched.difficulty,
                question_text: &fetched.question,
                choices: &choices,
                correct_index: correct_index as i16,
                correct_answer_text: &fetched.correct_answer,
                answer_timeout_secs: timeout as i32,
            })
            .await?;

        // ── Bot reacts so users can just tap ──────────────────────────────────
        for emoji in &CHOICE_EMOJIS[..choices.len()] {
            room.send(ReactionEventContent::new(Annotation::new(
                q_event_id.clone(),
                emoji.to_string(),
            )))
            .await
            .ok();
        }

        // ── Register active quiz ──────────────────────────────────────────────
        {
            let mut aq = ctx.active_quiz.lock().await;
            *aq = Some(ActiveQuiz {
                event_id: q_event_id.clone(),
                answers: HashMap::new(),
                correct_index,
            });
        }

        // ── Countdown ─────────────────────────────────────────────────────────
        const EDIT_INTERVAL: u64 = 15;
        let mut remaining = timeout;
        while remaining > EDIT_INTERVAL {
            tokio::time::sleep(tokio::time::Duration::from_secs(EDIT_INTERVAL)).await;
            remaining -= EDIT_INTERVAL;
            room.send(make_edit(
                q_event_id.clone(),
                &question_text(q_num, round_len, &fetched, &choices, timeout, remaining),
            ))
            .await
            .ok();
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(remaining)).await;
        room.send(make_edit(
            q_event_id.clone(),
            &question_text(q_num, round_len, &fetched, &choices, timeout, 0),
        ))
        .await
        .ok();

        // Drain active quiz.
        let evaluated_at = chrono::Utc::now();
        let mut answers: HashMap<String, AnswerRecord> = {
            let mut aq = ctx.active_quiz.lock().await;
            let a = aq.as_ref().map(|q| q.answers.clone()).unwrap_or_default();
            *aq = None;
            a
        };
        info!(
            question_event_id = %q_event_id,
            evaluated_at = %evaluated_at.to_rfc3339(),
            recorded = answers.len(),
            senders = ?answers.keys().collect::<Vec<_>>(),
            "Freezing answers for evaluation (pre-reconciliation)"
        );

        // ── Reconcile reactions from server ───────────────────────────────────
        reconcile_reactions(&client, &room, &q_event_id, &mut answers).await;

        // ── Build correct / wrong lists ───────────────────────────────────────
        let correct_emoji = CHOICE_EMOJIS[correct_index as usize];
        let correct_text = &choices[correct_index as usize];

        let mut correct_users: Vec<&str> = answers
            .iter()
            .filter(|(_, r)| r.choice == correct_index)
            .map(|(k, _)| k.as_str())
            .collect();
        // Keep the choice alongside each wrong user so we can show what they picked.
        let mut wrong_users: Vec<(&str, u8)> = answers
            .iter()
            .filter(|(_, r)| r.choice != correct_index)
            .map(|(k, r)| (k.as_str(), r.choice))
            .collect();
        correct_users.sort();
        wrong_users.sort_by_key(|&(uid, _)| uid);

        // Update round scores.
        for (user_id, rec) in &answers {
            let entry: &mut (u32, u32) = round_scores.entry(user_id.clone()).or_default();
            entry.1 += 1;
            if rec.choice == correct_index {
                entry.0 += 1;
            }
        }

        // ── Persist to DB ─────────────────────────────────────────────────────
        if let Err(e) = ctx
            .db
            .insert_answers(question_id, round_id, &answers, correct_index)
            .await
        {
            error!("DB insert_answers failed: {e}");
        }
        if let Err(e) = ctx
            .db
            .update_question_stats(
                question_id,
                answers.len() as i32,
                correct_users.len() as i32,
                wrong_users.len() as i32,
            )
            .await
        {
            error!("DB update_question_stats failed: {e}");
        }

        questions_asked = q_num;

        // ── Result message ────────────────────────────────────────────────────
        let mut result_lines = vec![format!("✅ {correct_emoji} **{correct_text}**")];
        if answers.is_empty() {
            result_lines.push("No answers.".to_owned());
        } else if correct_users.is_empty() {
            result_lines.push("Nobody got it right 😅".to_owned());
        } else {
            result_lines.push(format!("🎉 {}", correct_users.join(", ")));
        }
        if !wrong_users.is_empty() {
            let wrong_str = wrong_users
                .iter()
                .map(|&(uid, choice)| {
                    let emoji = CHOICE_EMOJIS.get(choice as usize).copied().unwrap_or("?");
                    format!("{uid} ({emoji})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            result_lines.push(format!("❌ {wrong_str}"));
        }
        if q_num < round_len {
            result_lines.push(format!("⏭️ Next in {inter_pause}s"));
        }

        let all_user_ids: Vec<String> = answers.keys().cloned().collect();
        {
            let user_refs: Vec<&str> = all_user_ids.iter().map(String::as_str).collect();
            let names = fetch_names(&room, &user_refs).await;
            if let Err(e) = ctx.db.update_display_names(&names).await {
                warn!("DB update_display_names failed: {e}");
            }
            let send_result = room
                .send(crate::format::mentionify_with_names(
                    &result_lines.join("\n"),
                    &names,
                ))
                .await;

            // If configured, post a quiz explanation (+ image) as a thread reply.
            if let (Ok(resp), Some(api_key)) = (send_result, ctx.config.explainer.api_key.clone()) {
                let result_event_id = resp.response.event_id;
                let model = ctx.config.explainer.model.clone();
                let question = fetched.question.clone();
                let answer = fetched.correct_answer.clone();
                let room2 = room.clone();
                let client2 = client.clone();
                tokio::spawn(async move {
                    let Some(result) =
                        crate::explainer::explain(&question, &answer, &api_key, &model).await
                    else {
                        return;
                    };

                    // Upload and post the image first (no quote box) so it
                    // appears above the explanation text.
                    if let Some(img_url) = result.image_url {
                        if let Some((bytes, ct)) =
                            crate::explainer::fetch_image_bytes(&img_url).await
                        {
                            use matrix_sdk::ruma::events::room::message::{
                                ImageMessageEventContent, MessageType,
                            };
                            let mime: mime::Mime = ct.parse().unwrap_or(mime::IMAGE_JPEG);
                            let dims = crate::explainer::image_dimensions(&bytes);
                            let size = bytes.len();
                            if let Ok(upload) = client2.media().upload(&mime, bytes, None).await {
                                let mut img_content = ImageMessageEventContent::plain(
                                    answer.clone(),
                                    upload.content_uri,
                                );
                                let mut info = ImageInfo::new();
                                info.mimetype = Some(ct);
                                info.size = UInt::new(size as u64);
                                if let Some((w, h)) = dims {
                                    info.width = UInt::new(w as u64);
                                    info.height = UInt::new(h as u64);
                                }
                                img_content.info = Some(Box::new(info));
                                let mut img_msg =
                                    RoomMessageEventContent::new(MessageType::Image(img_content));
                                img_msg.relates_to = Some(Relation::Thread(
                                    Thread::without_fallback(result_event_id.clone()),
                                ));
                                room2.send(img_msg).await.ok();
                            }
                        }
                    }

                    // Post the explanation text as a reply (shows quote box).
                    let mut content = RoomMessageEventContent::text_plain(&result.text);
                    content.relates_to = Some(Relation::Thread(Thread::reply(
                        result_event_id.clone(),
                        result_event_id,
                    )));
                    room2.send(content).await.ok();
                });
            }
        }

        if q_num < round_len {
            tokio::time::sleep(tokio::time::Duration::from_secs(inter_pause)).await;
        }
    }

    // ── Finalise round ────────────────────────────────────────────────────────

    // Fix leaderboard totals: a user who only answered some questions still
    // "played" the full round.  Set total_count = questions_asked for everyone
    // so the leaderboard reflects questions-in-round, not questions-answered.
    for entry in round_scores.values_mut() {
        entry.1 = questions_asked;
    }

    if let Err(e) = ctx
        .db
        .finish_round_with_scores(round_id, questions_asked as i32, &round_scores)
        .await
    {
        error!("DB finish_round_with_scores failed: {e}");
    }

    // ── Round summary ─────────────────────────────────────────────────────────
    {
        let mut summary_lines = vec![format!("🏁 Round done · {questions_asked} Qs")];

        if round_scores.is_empty() {
            summary_lines.push(String::new());
            summary_lines.push("Nobody got it right.".to_owned());
        } else {
            let mut podium: Vec<(&String, u32, u32)> =
                round_scores.iter().map(|(u, &(c, t))| (u, c, t)).collect();
            podium.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            summary_lines.push(String::new());
            summary_lines.push("🎯 This round:".to_owned());
            for (i, (user, correct, _)) in podium.iter().take(5).enumerate() {
                let medal = match i {
                    0 => "🥇",
                    1 => "🥈",
                    2 => "🥉",
                    _ => "▪️",
                };
                summary_lines.push(format!("{medal} {} · {correct}/{questions_asked}", user));
            }
        }

        // Kept so its player names can be added to the mention map below —
        // `leaderboard_text` embeds raw mxids that only this round's podium
        // (via `fetch_names`) would otherwise resolve.
        let mut monthly_board: Option<Vec<db::MonthlyLeaderboardEntry>> = None;
        if let Ok((start, end)) = leaderboard_month.utc_bounds(tz) {
            match (
                ctx.db.monthly_leaderboard(start, end).await,
                ctx.db.question_count_between(start, end).await,
            ) {
                (Ok(board), Ok(question_count)) => {
                    summary_lines.push(String::new());
                    summary_lines.extend(
                        crate::leaderboard::leaderboard_text(
                            leaderboard_month.month_name(),
                            question_count,
                            &board,
                        )
                        .lines()
                        .map(str::to_owned),
                    );
                    monthly_board = Some(board);
                }
                (board, count) => {
                    warn!(
                        "Could not build monthly round leaderboard: board={:?}, count={:?}",
                        board.err(),
                        count.err()
                    );
                }
            }
        }

        // Mention names: start from the monthly board's stored display names
        // (covers players outside this round), then let this round's live
        // room-state lookup override with fresher names for its participants.
        let user_ids: Vec<&str> = round_scores.keys().map(String::as_str).collect();
        let mut names = monthly_board
            .as_deref()
            .map(crate::leaderboard::mention_names)
            .unwrap_or_default();
        names.extend(fetch_names(&room, &user_ids).await);
        room.send(crate::format::mentionify_with_names(
            &summary_lines.join("\n"),
            &names,
        ))
        .await
        .ok();
    }

    info!("Quiz round finished ({questions_asked} questions)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_round_length_caps_at_questions_gathered() {
        assert_eq!(resolve_round_length(3, 5), Some(3));
    }

    #[test]
    fn resolve_round_length_caps_at_configured_count() {
        assert_eq!(resolve_round_length(10, 5), Some(5));
    }

    #[test]
    fn resolve_round_length_matches_when_exact() {
        assert_eq!(resolve_round_length(5, 5), Some(5));
    }

    #[test]
    fn resolve_round_length_is_none_when_nothing_was_gathered() {
        assert_eq!(resolve_round_length(0, 5), None);
    }

    // ── Reaction handling ───────────────────────────────────────────────────

    fn event_id(s: &str) -> OwnedEventId {
        s.try_into().unwrap()
    }

    fn answer(choice: u8, source: &'static str) -> AnswerRecord {
        AnswerRecord {
            choice,
            source,
            submitted_at: chrono::Utc::now(),
            changed_answer: false,
        }
    }

    fn active_quiz(question: &str) -> ActiveQuiz {
        ActiveQuiz {
            event_id: event_id(question),
            answers: HashMap::new(),
            correct_index: 0,
        }
    }

    #[test]
    fn record_answer_first_time_is_new() {
        let mut quiz = active_quiz("$q1:example.org");
        let outcome = quiz.record_answer("@alice:example.org".to_owned(), 0, "reaction");
        assert_eq!(outcome, AnswerOutcome::New);
        assert_eq!(quiz.answers["@alice:example.org"].choice, 0);
    }

    #[test]
    fn record_answer_changing_choice_is_replaced_and_flagged() {
        let mut quiz = active_quiz("$q1:example.org");
        quiz.record_answer("@alice:example.org".to_owned(), 0, "reaction");
        let outcome = quiz.record_answer("@alice:example.org".to_owned(), 2, "reaction");
        assert_eq!(outcome, AnswerOutcome::Replaced { previous: 0 });
        assert_eq!(quiz.answers["@alice:example.org"].choice, 2);
        assert!(quiz.answers["@alice:example.org"].changed_answer);
    }

    #[test]
    fn record_answer_duplicate_reaction_is_unchanged() {
        let mut quiz = active_quiz("$q1:example.org");
        quiz.record_answer("@alice:example.org".to_owned(), 1, "reaction");
        let outcome = quiz.record_answer("@alice:example.org".to_owned(), 1, "reaction");
        assert_eq!(outcome, AnswerOutcome::Unchanged);
        assert!(!quiz.answers["@alice:example.org"].changed_answer);
    }

    #[test]
    fn apply_reaction_accepts_for_the_active_question() {
        let mut active = Some(active_quiz("$q1:example.org"));
        let result = apply_reaction(
            &mut active,
            &event_id("$q1:example.org"),
            "@alice:example.org".to_owned(),
            0,
        );
        assert_eq!(result, ReactionResult::Accepted(AnswerOutcome::New));
        assert!(active.unwrap().answers.contains_key("@alice:example.org"));
    }

    #[test]
    fn apply_reaction_ignores_a_reaction_to_a_stale_question() {
        let mut active = Some(active_quiz("$q2:example.org"));
        let result = apply_reaction(
            &mut active,
            &event_id("$q1:example.org"), // an older, already-closed question
            "@alice:example.org".to_owned(),
            0,
        );
        assert_eq!(result, ReactionResult::WrongQuestion);
        assert!(active.unwrap().answers.is_empty());
    }

    #[test]
    fn apply_reaction_ignores_when_no_question_is_active() {
        let mut active: Option<ActiveQuiz> = None;
        let result = apply_reaction(
            &mut active,
            &event_id("$q1:example.org"),
            "@alice:example.org".to_owned(),
            0,
        );
        assert_eq!(result, ReactionResult::NoActiveQuestion);
    }

    #[test]
    fn apply_reaction_records_multiple_near_simultaneous_senders() {
        // Concurrent reactions are serialized through the same mutex the
        // real handler locks, so back-to-back calls model that correctly.
        let mut active = Some(active_quiz("$q1:example.org"));
        apply_reaction(&mut active, &event_id("$q1:example.org"), "@a:x.org".to_owned(), 0);
        apply_reaction(&mut active, &event_id("$q1:example.org"), "@b:x.org".to_owned(), 1);
        apply_reaction(&mut active, &event_id("$q1:example.org"), "@c:x.org".to_owned(), 2);
        let answers = active.unwrap().answers;
        assert_eq!(answers.len(), 3);
        assert_eq!(answers["@a:x.org"].choice, 0);
        assert_eq!(answers["@b:x.org"].choice, 1);
        assert_eq!(answers["@c:x.org"].choice, 2);
    }

    // ── Reconciliation merge ─────────────────────────────────────────────────

    #[test]
    fn merge_adds_a_reaction_the_live_stream_missed() {
        // Models a reaction sent just before the deadline: fully delivered
        // to the homeserver, but not yet processed (or processed after
        // `active_quiz` was already drained) by the live stream.
        let mut answers = HashMap::new();
        let server = HashMap::from([("@late:example.org".to_owned(), 2u8)]);

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert_eq!(summary.added, vec!["@late:example.org".to_owned()]);
        assert_eq!(answers["@late:example.org"].choice, 2);
        assert_eq!(answers["@late:example.org"].source, "reconciled");
    }

    #[test]
    fn merge_does_not_lose_an_answer_already_received_live() {
        // The core finalization-robustness guarantee: an answer the live
        // stream already recorded, and that the server agrees with, must
        // survive reconciliation untouched (including its original
        // timestamp/source).
        let mut answers = HashMap::new();
        answers.insert("@alice:example.org".to_owned(), answer(1, "reaction"));
        let server = HashMap::from([("@alice:example.org".to_owned(), 1u8)]);

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert!(summary.is_empty());
        assert_eq!(answers["@alice:example.org"].source, "reaction");
        assert_eq!(answers.len(), 1);
    }

    #[test]
    fn merge_corrects_a_changed_answer_the_stream_missed() {
        let mut answers = HashMap::new();
        answers.insert("@alice:example.org".to_owned(), answer(0, "reaction"));
        let server = HashMap::from([("@alice:example.org".to_owned(), 3u8)]);

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert_eq!(summary.corrected, vec!["@alice:example.org".to_owned()]);
        assert_eq!(answers["@alice:example.org"].choice, 3);
        assert!(answers["@alice:example.org"].changed_answer);
    }

    #[test]
    fn merge_removes_an_answer_the_user_un_reacted() {
        let mut answers = HashMap::new();
        answers.insert("@alice:example.org".to_owned(), answer(0, "reaction"));
        let server = HashMap::new(); // no reaction present anymore

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert_eq!(summary.removed, vec!["@alice:example.org".to_owned()]);
        assert!(answers.is_empty());
    }

    #[test]
    fn merge_never_drops_a_text_answer() {
        let mut answers = HashMap::new();
        answers.insert("@alice:example.org".to_owned(), answer(0, "text"));
        let server = HashMap::new(); // text answers have no server-side reaction

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert!(summary.is_empty());
        assert_eq!(answers["@alice:example.org"].source, "text");
    }

    #[test]
    fn merge_handles_multiple_senders_reacting_at_once() {
        let mut answers = HashMap::new();
        answers.insert("@already:example.org".to_owned(), answer(0, "reaction"));
        let server = HashMap::from([
            ("@already:example.org".to_owned(), 0u8),
            ("@new1:example.org".to_owned(), 1u8),
            ("@new2:example.org".to_owned(), 3u8),
        ]);

        let summary = merge_reconciled_answers(&mut answers, &server, chrono::Utc::now());

        assert_eq!(answers.len(), 3);
        assert_eq!(summary.added.len(), 2);
        assert!(summary.added.contains(&"@new1:example.org".to_owned()));
        assert!(summary.added.contains(&"@new2:example.org".to_owned()));
    }
}
