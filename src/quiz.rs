use std::collections::HashMap;

use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedEventId,
        events::{
            reaction::ReactionEventContent,
            relation::Annotation,
            room::message::{ReplacementMetadata, RoomMessageEventContent},
        },
    },
};
use rand::seq::SliceRandom;
use tracing::{error, info, warn};

use crate::{BotContext, fetcher, state};

use matrix_sdk::ruma::events::Mentions;

/// Look up the display names of `user_ids` from room state.
/// Falls back to the localpart when no display name is set.
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

/// Regional-indicator emoji for each answer slot (A–D).
pub const CHOICE_EMOJIS: [&str; 4] = ["🇦", "🇧", "🇨", "🇩"];

// ── Active quiz state (in-memory only) ───────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ActiveQuiz {
    /// Event ID of the question message — used to match incoming reactions.
    pub event_id: OwnedEventId,
    /// user_id → choice_index.  Last answer wins (overwrite allowed).
    pub answers: HashMap<String, u8>,
    /// Which slot (0–3) holds the correct answer.
    pub correct_index: u8,
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

fn difficulty_icon(d: &str) -> &'static str {
    match d { "easy" => "🟢", "medium" => "🟡", "hard" => "🔴", _ => "⚪" }
}

/// Render a 10-block "time remaining" bar, e.g. `████████░░`.
fn time_bar(remaining: u64, total: u64) -> String {
    const W: usize = 10;
    let filled = if total > 0 { (remaining * W as u64 / total) as usize } else { 0 };
    format!("{}{}", "█".repeat(filled), "░".repeat(W - filled))
}

/// Build the question message lines.  `remaining_secs` drives the countdown
/// header; on the initial post pass `total_secs` for both.
fn question_text(
    q_num: u32,
    n_questions: u32,
    fetched: &state::FetchedQuestion,
    choices: &[String],
    total_secs: u64,
    remaining_secs: u64,
) -> String {
    let icon = difficulty_icon(&fetched.difficulty);
    let bar  = time_bar(remaining_secs, total_secs);
    let mut lines = vec![
        format!(
            "❓ Question {q_num}/{n_questions}  {icon} {} · {}  — ⏳ {remaining_secs}s  {bar}",
            fetched.difficulty, fetched.category,
        ),
        String::new(),
        fetched.question.clone(),
        String::new(),
    ];
    for (i, choice) in choices.iter().enumerate() {
        lines.push(format!("{}  {}", CHOICE_EMOJIS[i], choice));
    }
    lines.push(String::new());
    lines.push(format!(
        "React with {} to answer!",
        CHOICE_EMOJIS[..choices.len()].join("  "),
    ));
    lines.join("\n")
}

/// Build an edit (m.replace) for an already-posted message.
fn make_edit(event_id: OwnedEventId, text: &str) -> RoomMessageEventContent {
    RoomMessageEventContent::text_plain(text)
        .make_replacement(ReplacementMetadata::new(event_id, None))
}

// ── Reaction reconciliation ───────────────────────────────────────────────────

/// After the countdown ends, fetch all reactions on `q_event_id` from the
/// server and replace the in-memory answers with the server's ground truth.
///
/// The server chunk is most-recent-first, so for each user we take only their
/// first occurrence (= latest reaction).
///
/// If the server query succeeds (at least one page returned without error),
/// `answers` is completely replaced with the server data — no in-memory
/// fallback.  If the query fails entirely, `answers` is left unchanged so
/// at least the in-memory state is used.
async fn reconcile_reactions(
    client: &Client,
    room:   &Room,
    q_event_id: &OwnedEventId,
    answers: &mut HashMap<String, u8>,
) {
    use matrix_sdk::ruma::{
        api::client::relations::get_relating_events_with_rel_type_and_event_type::v1 as api,
        events::{AnyMessageLikeEvent, TimelineEventType, relation::RelationType},
    };

    let mut server_answers: HashMap<String, u8> = HashMap::new();
    let mut query_succeeded = false;
    let mut from: Option<String> = None;
    loop {
        let mut req = api::Request::new(
            room.room_id().to_owned(),
            q_event_id.clone(),
            RelationType::Annotation,
            TimelineEventType::from("m.reaction"),
        );
        req.from = from.clone();
        let resp = match client.send(req).await {
            Ok(r)  => { query_succeeded = true; r }
            Err(e) => { warn!("Reaction reconciliation failed: {e}"); break; }
        };

        // or_insert: chunk is most-recent-first, first occurrence = latest reaction.
        for raw in &resp.chunk {
            let Ok(AnyMessageLikeEvent::Reaction(ev)) = raw.deserialize() else { continue };
            let Some(orig) = ev.as_original() else { continue };
            let choice = match orig.content.relates_to.key.as_str() {
                "🇦" => 0u8, "🇧" => 1, "🇨" => 2, "🇩" => 3, _ => continue,
            };
            server_answers.entry(orig.sender.as_str().to_owned()).or_insert(choice);
        }

        match resp.next_batch {
            Some(token) => from = Some(token),
            None        => break,
        }
    }

    if query_succeeded {
        // Server is the complete truth — replace in-memory state entirely.
        *answers = server_answers;
    }
    // If query failed, answers is unchanged (in-memory fallback).
}

// ── Quiz runner ───────────────────────────────────────────────────────────────

/// Run a full quiz round (N questions back-to-back), then post a round summary.
/// `skip_reminder` — pass `true` for manual `!startquiz` (starts immediately).
/// `slot_key` — the "HH:MM" config entry that triggered this run; used to mark
///              that slot as done today.  `None` for manually started quizzes.
/// Intended to be called inside `tokio::spawn`.
pub async fn start_quiz(
    ctx: BotContext,
    client: Client,
    skip_reminder: bool,
    slot_key: Option<String>,
) -> anyhow::Result<()> {
    let n_questions   = ctx.config.schedule.questions_per_round.max(1);
    let timeout       = ctx.config.schedule.answer_timeout_secs;
    let inter_pause   = ctx.config.schedule.inter_question_secs;
    let reminder_secs = ctx.config.schedule.reminder_before_secs;
    // Unique ID for this round — stamped on every QuizResult so unanswered
    // questions can be counted as wrong for partial-round participants.
    let round_id: u64 = chrono::Utc::now().timestamp_millis() as u64;

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("Quiz: bot not in room {}", ctx.room_id);
            return Ok(());
        }
    };

    // ── Reminder ──────────────────────────────────────────────────────────────
    if !skip_reminder && reminder_secs > 0 {
        let mins = reminder_secs / 60;
        let time_str = if mins >= 1 {
            format!("{} minute{}", mins, if mins == 1 { "" } else { "s" })
        } else {
            format!("{} seconds", reminder_secs)
        };
        let qs = if n_questions == 1 { "question" } else { "questions" };
        let plain = format!(
            "🧠 Quiz starting in {time_str}! @room\nGet ready — {n_questions} {qs} incoming.",
        );
        let html = format!(
            "🧠 <strong>Quiz starting in {time_str}!</strong> @room<br>Get ready — {n_questions} {qs} incoming.",
        );
        let mut mentions = Mentions::new();
        mentions.room = true;
        let content = RoomMessageEventContent::text_html(plain, html)
            .add_mentions(mentions);
        room.send(content).await.ok();
        tokio::time::sleep(tokio::time::Duration::from_secs(reminder_secs)).await;
    }

    // ── Mark today for this slot (after reminder sleep — crash during reminder
    //    = re-fires next minute tick, which is intentional) ─────────────────────
    if let Some(ref key) = slot_key {
        let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local_date = chrono::Utc::now().with_timezone(&tz).date_naive();
        let mut state = ctx.state.lock().await;
        state.last_quiz_dates.insert(key.clone(), local_date);
        if let Err(e) = state.save(&ctx.state_path).await {
            error!("Failed to persist last_quiz_dates: {e}");
        }
    }

    // round_scores: user_id → correct count this round
    let mut round_scores: HashMap<String, u32> = HashMap::new();

    for q_num in 1..=n_questions {
        // ── Fetch next question ───────────────────────────────────────────────
        let fetched = match fetcher::next_question(&ctx).await {
            Ok(q)  => q,
            Err(e) => {
                error!("Could not fetch question {q_num}: {e}");
                room.send(RoomMessageEventContent::text_plain(
                    "⚠️ Could not fetch a question from OpenTDB — ending round early.",
                ))
                .await
                .ok();
                break;
            }
        };

        let (choices, correct_index) = shuffle_choices(&fetched);

        // ── Post question ─────────────────────────────────────────────────────
        let resp = room
            .send(RoomMessageEventContent::text_plain(
                question_text(q_num, n_questions, &fetched, &choices, timeout, timeout),
            ))
            .await
            .map_err(|e| anyhow::anyhow!("send question failed: {e}"))?;
        let q_event_id: OwnedEventId = resp.response.event_id;
        info!("Q {q_num}/{n_questions}: posted (event {q_event_id}, correct slot {correct_index})");

        // ── Bot reacts with all choices so users can just tap ─────────────────
        for emoji in &CHOICE_EMOJIS[..choices.len()] {
            room.send(ReactionEventContent::new(
                Annotation::new(q_event_id.clone(), emoji.to_string()),
            ))
            .await
            .ok();
        }

        // ── Register active quiz ──────────────────────────────────────────────
        {
            let mut aq = ctx.active_quiz.lock().await;
            *aq = Some(ActiveQuiz {
                event_id: q_event_id.clone(),
                answers:  HashMap::new(),
                correct_index,
            });
        }

        // ── Countdown + collect reactions ─────────────────────────────────────
        const EDIT_INTERVAL: u64 = 15;
        let mut remaining = timeout;
        while remaining > EDIT_INTERVAL {
            tokio::time::sleep(tokio::time::Duration::from_secs(EDIT_INTERVAL)).await;
            remaining -= EDIT_INTERVAL;
            let text = question_text(q_num, n_questions, &fetched, &choices, timeout, remaining);
            room.send(make_edit(q_event_id.clone(), &text)).await.ok();
        }
        // Sleep the remainder
        tokio::time::sleep(tokio::time::Duration::from_secs(remaining)).await;

        let mut answers: HashMap<String, u8> = {
            let mut aq = ctx.active_quiz.lock().await;
            let a = aq.as_ref().map(|q| q.answers.clone()).unwrap_or_default();
            *aq = None;
            a
        };

        // ── Reconcile with server to catch any reactions missed on the stream ─
        reconcile_reactions(&client, &room, &q_event_id, &mut answers).await;

        // ── Build per-question result ─────────────────────────────────────────
        let correct_emoji = CHOICE_EMOJIS[correct_index as usize];
        let correct_text  = &choices[correct_index as usize];

        let mut correct_users: Vec<&str> = answers.iter()
            .filter(|(_, &v)| v == correct_index)
            .map(|(k, _)| k.as_str())
            .collect();
        let mut wrong_users: Vec<&str> = answers.iter()
            .filter(|(_, &v)| v != correct_index)
            .map(|(k, _)| k.as_str())
            .collect();
        correct_users.sort();
        wrong_users.sort();

        // Update round scores
        for user in &correct_users {
            *round_scores.entry(user.to_string()).or_default() += 1;
        }

        let mut result_lines = vec![
            format!("⏱️ Time's up! Correct answer: {correct_emoji} **{correct_text}**"),
        ];
        if answers.is_empty() {
            result_lines.push("No answers received.".to_owned());
        } else if correct_users.is_empty() {
            result_lines.push("Nobody got it right 😅".to_owned());
        } else {
            result_lines.push(format!("🎉 Correct: {}", correct_users.join(", ")));
        }
        if !wrong_users.is_empty() {
            result_lines.push(format!("❌ Wrong: {}", wrong_users.join(", ")));
        }
        if q_num < n_questions {
            result_lines.push(format!("⏸️ Next question in {inter_pause}s…"));
        }

        // ── Fetch display names before answers is moved ───────────────────────
        let all_users: Vec<String> = answers.keys().cloned().collect();

        // ── Persist result ────────────────────────────────────────────────────
        {
            let mut state = ctx.state.lock().await;
            state.results.push(state::QuizResult {
                round_id,
                question_text:  fetched.question.clone(),
                category:       fetched.category.clone(),
                difficulty:     fetched.difficulty.clone(),
                correct_answer: fetched.correct_answer.clone(),
                correct_index,
                asked_at:       chrono::Utc::now(),
                answers,
            });
            if let Err(e) = state.save(&ctx.state_path).await {
                error!("Failed to save question result: {e}");
            }
        }

        // ── Send result in main chat ──────────────────────────────────────────
        {
            let user_refs: Vec<&str> = all_users.iter().map(String::as_str).collect();
            let names = fetch_names(&room, &user_refs).await;
            room.send(crate::format::mentionify_with_names(
                &result_lines.join("\n"), &names,
            )).await.ok();
        }

        // ── Pause before next question ────────────────────────────────────────
        if q_num < n_questions {
            tokio::time::sleep(tokio::time::Duration::from_secs(inter_pause)).await;
        }
    }

    // ── Round summary ─────────────────────────────────────────────────────────
    if n_questions > 1 {
        let state = ctx.state.lock().await;

        let mut summary_lines = vec![
            format!("🏁 Round complete! {n_questions} questions"),
        ];

        // This-round podium — sorted by score desc, then username asc
        if round_scores.is_empty() {
            summary_lines.push(String::new());
            summary_lines.push("No correct answers this round.".to_owned());
        } else {
            let mut podium: Vec<(&String, &u32)> = round_scores.iter().collect();
            podium.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            summary_lines.push(String::new());
            summary_lines.push(format!("🎯 This round:"));
            for (i, (user, &correct)) in podium.iter().take(5).enumerate() {
                let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "▪️" };
                summary_lines.push(format!("{medal} {}. {} — {correct}/{n_questions}", i + 1, user));
            }
        }

        // All-time leaderboard — sorted by accuracy, then correct count, then total, then name
        let board = state.leaderboard();
        if !board.is_empty() {
            summary_lines.push(String::new());
            summary_lines.push(format!(
                "🏆 All-time leaderboard ({} questions played):",
                state.results.len(),
            ));
            for (i, (user, correct, total)) in board.iter().take(5).enumerate() {
                let pct   = if *total > 0 {
                    (*correct as f64 / *total as f64 * 100.0).round() as u32
                } else {
                    0
                };
                let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "▪️" };
                summary_lines.push(format!(
                    "{medal} {}. {} — {}/{} ({}%)",
                    i + 1, user, correct, total, pct,
                ));
            }
        }

        {
            // Collect user IDs from both the round podium and the all-time
            // board so display names are resolved for everyone shown.
            let mut uid_set: std::collections::HashSet<&str> = round_scores
                .keys()
                .map(String::as_str)
                .collect();
            for (user, _, _) in board.iter().take(5) {
                uid_set.insert(user.as_str());
            }
            let summary_users: Vec<&str> = uid_set.into_iter().collect();
            let names = fetch_names(&room, &summary_users).await;
            room.send(crate::format::mentionify_with_names(
                &summary_lines.join("\n"), &names,
            )).await.ok();
        }
    }

    info!("Quiz round finished ({n_questions} questions)");
    Ok(())
}
