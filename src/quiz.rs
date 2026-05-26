use std::collections::HashMap;

use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedEventId,
        events::{
            reaction::ReactionEventContent,
            relation::{Annotation, Thread},
            room::message::{ReplacementMetadata, Relation, RoomMessageEventContent},
        },
    },
};
use rand::seq::SliceRandom;
use tracing::{error, info, warn};

use crate::{BotContext, db::{self, AnswerRecord}, fetcher, state};

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
    pub event_id:      OwnedEventId,
    /// Per-user answer records.  `record_answer` handles change-tracking.
    pub answers:       HashMap<String, AnswerRecord>,
    pub correct_index: u8,
}

impl ActiveQuiz {
    /// Record or update a user's answer.  Sets `changed_answer = true` when
    /// the user picks a different option than their previous one.
    pub fn record_answer(&mut self, user_id: String, choice: u8, source: &'static str) {
        let now = chrono::Utc::now();
        self.answers
            .entry(user_id)
            .and_modify(|r| {
                let changed = r.choice != choice;
                r.choice       = choice;
                r.source       = source;
                r.submitted_at = now;
                if changed { r.changed_answer = true; }
            })
            .or_insert(AnswerRecord {
                choice,
                source,
                submitted_at:   now,
                changed_answer: false,
            });
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

fn difficulty_icon(d: &str) -> &'static str {
    match d { "easy" => "🟢", "medium" => "🟡", "hard" => "🔴", _ => "⚪" }
}

fn time_bar(remaining: u64, total: u64) -> String {
    const W: usize = 10;
    let filled = if total > 0 { (remaining * W as u64 / total) as usize } else { 0 };
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
    let bar  = time_bar(remaining_secs, total_secs);
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

// ── Reaction reconciliation ───────────────────────────────────────────────────

/// Fetch all reactions from the server after the countdown and merge them into
/// the in-memory answer map.  Users found only on the server (missed on the
/// stream) are added with source "reconciled" and submitted_at = now.
async fn reconcile_reactions(
    client:     &Client,
    room:       &Room,
    q_event_id: &OwnedEventId,
    answers:    &mut HashMap<String, AnswerRecord>,
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
        match client.send(req).await {
            Ok(resp) => {
                query_succeeded = true;
                for raw in &resp.chunk {
                    let Ok(AnyMessageLikeEvent::Reaction(ev)) = raw.deserialize() else { continue };
                    let Some(orig) = ev.as_original() else { continue };
                    // Skip the bot's own 🇦🇧🇨🇩 reactions it posted for tap-to-answer.
                    if client.user_id().map(|id| id == orig.sender).unwrap_or(false) {
                        continue;
                    }
                    let choice = match orig.content.relates_to.key.as_str() {
                        "🇦" => 0u8, "🇧" => 1, "🇨" => 2, "🇩" => 3, _ => continue,
                    };
                    server_answers
                        .entry(orig.sender.as_str().to_owned())
                        .or_insert(choice);
                }
                match resp.next_batch {
                    Some(token) => from = Some(token),
                    None        => break,
                }
            }
            Err(e) => {
                warn!("Reaction reconciliation failed: {e}");
                break;
            }
        }
    }

    if !query_succeeded {
        return;
    }

    let now = chrono::Utc::now();

    // The stream never sees m.room.redaction events, so it can hold a stale
    // reaction answer after the user removed it.  The server's current reaction
    // set is authoritative.
    //
    // Rules:
    //  • Reaction answer + user gone from server  → remove (they un-reacted)
    //  • Text answer (!a/!b/…) + not on server   → keep (text can't be un-sent)
    //  • Reaction answer + server has same choice → keep stream (timestamp intact)
    //  • Reaction answer + server has diff choice → server wins (missed redact + re-react)
    //  • User missing from stream entirely        → add from server

    // Step 1: drop stale reaction answers for users who removed all reactions.
    answers.retain(|user_id, rec| {
        rec.source == "text" || server_answers.contains_key(user_id)
    });

    // Step 2: correct / fill in from server.
    for (user_id, &server_choice) in &server_answers {
        answers
            .entry(user_id.clone())
            .and_modify(|r| {
                // Don't touch text answers — they're always final.
                if r.source != "text" && r.choice != server_choice {
                    r.choice         = server_choice;
                    r.source         = "reconciled";
                    r.changed_answer = true;
                }
            })
            .or_insert(AnswerRecord {
                choice:         server_choice,
                source:         "reconciled",
                submitted_at:   now,
                changed_answer: false,
            });
    }
}

// ── Quiz runner ───────────────────────────────────────────────────────────────

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
        let n    = n_questions as usize;
        tokio::spawn(async move { fetcher::fetch_round_questions(&ctx2, n).await })
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
        let plain = format!("🧠 Quiz starting in {time_str}! @room\n{n_questions} {qs} incoming.");
        let html  = format!("🧠 <strong>Quiz starting in {time_str}!</strong> @room<br>{n_questions} {qs} incoming.");
        let mut mentions = Mentions::new();
        mentions.room = true;
        room.send(RoomMessageEventContent::text_html(plain, html).add_mentions(mentions))
            .await
            .ok();
        tokio::time::sleep(tokio::time::Duration::from_secs(reminder_secs)).await;
    }

    // ── Mark today for this scheduler slot ────────────────────────────────────
    if let Some(ref key) = slot_key {
        let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local_date = chrono::Utc::now().with_timezone(&tz).date_naive();
        let mut state = ctx.state.lock().await;
        state.last_quiz_dates.insert(key.clone(), local_date);
        if let Err(e) = state.save(&ctx.state_path).await {
            error!("Failed to persist last_quiz_dates: {e}");
        }
    }

    // ── Create round in DB ────────────────────────────────────────────────────
    let triggered_by = slot_key
        .as_ref()
        .map(|k| format!("scheduler:{k}"))
        .unwrap_or_else(|| "manual".to_owned());

    let round_id = ctx.db
        .create_round(&db::RoundParams {
            room_id:                    ctx.room_id.as_str(),
            n_questions_planned:        n_questions as i32,
            triggered_by:               &triggered_by,
            config_answer_timeout:      timeout as i32,
            config_questions_per_round: n_questions as i32,
            config_timezone:            &ctx.config.schedule.timezone,
            config_category_id:         ctx.config.trivia.category.map(|c| c as i32),
            config_difficulty:          ctx.config.trivia.difficulty.as_deref(),
        })
        .await?;

    let mut round_scores: HashMap<String, (u32, u32)> = HashMap::new();
    let mut questions_asked = 0u32;

    // Await the prefetch task started before the reminder.
    let mut prefetched = prefetch_handle.await.unwrap_or_default();
    let mut prefetched_iter = prefetched.drain(..);

    for q_num in 1..=n_questions {
        // ── Fetch question ────────────────────────────────────────────────────
        let fetched = match prefetched_iter.next() {
            Some(q) => q,
            None => {
                error!("Pre-fetched questions exhausted at question {q_num}");
                room.send(RoomMessageEventContent::text_plain(
                    "⚠️ Could not fetch question · ending round early.",
                )).await.ok();
                break;
            }
        };

        let (choices, correct_index) = shuffle_choices(&fetched);

        // ── Post question ─────────────────────────────────────────────────────
        let qt = question_text(q_num, n_questions, &fetched, &choices, timeout, timeout);
        let initial_text = if q_num == 1 { format!("@room\n{qt}") } else { qt };
        let mut q_content = RoomMessageEventContent::text_plain(initial_text);
        if q_num == 1 {
            let mut m = Mentions::new();
            m.room = true;
            q_content = q_content.add_mentions(m);
        }
        let resp = room
            .send(q_content)
            .await
            .map_err(|e| anyhow::anyhow!("send question failed: {e}"))?;
        let q_event_id: OwnedEventId = resp.response.event_id;
        info!("Q {q_num}/{n_questions}: posted (event {q_event_id}, correct slot {correct_index})");

        // ── Insert question in DB ─────────────────────────────────────────────
        let question_id = ctx.db
            .insert_question(&db::QuestionParams {
                round_id,
                question_num:        q_num as i32,
                matrix_event_id:     Some(q_event_id.as_str()),
                category:            &fetched.category,
                difficulty:          &fetched.difficulty,
                question_text:       &fetched.question,
                choices:             &choices,
                correct_index:       correct_index as i16,
                correct_answer_text: &fetched.correct_answer,
                answer_timeout_secs: timeout as i32,
            })
            .await?;

        // ── Bot reacts so users can just tap ──────────────────────────────────
        for emoji in &CHOICE_EMOJIS[..choices.len()] {
            room.send(ReactionEventContent::new(
                Annotation::new(q_event_id.clone(), emoji.to_string()),
            )).await.ok();
        }

        // ── Register active quiz ──────────────────────────────────────────────
        {
            let mut aq = ctx.active_quiz.lock().await;
            *aq = Some(ActiveQuiz {
                event_id:      q_event_id.clone(),
                answers:       HashMap::new(),
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
                &question_text(q_num, n_questions, &fetched, &choices, timeout, remaining),
            )).await.ok();
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(remaining)).await;

        // Drain active quiz.
        let mut answers: HashMap<String, AnswerRecord> = {
            let mut aq = ctx.active_quiz.lock().await;
            let a = aq.as_ref().map(|q| q.answers.clone()).unwrap_or_default();
            *aq = None;
            a
        };

        // ── Reconcile reactions from server ───────────────────────────────────
        reconcile_reactions(&client, &room, &q_event_id, &mut answers).await;

        // ── Build correct / wrong lists ───────────────────────────────────────
        let correct_emoji = CHOICE_EMOJIS[correct_index as usize];
        let correct_text  = &choices[correct_index as usize];

        let mut correct_users: Vec<&str> = answers.iter()
            .filter(|(_, r)| r.choice == correct_index)
            .map(|(k, _)| k.as_str())
            .collect();
        // Keep the choice alongside each wrong user so we can show what they picked.
        let mut wrong_users: Vec<(&str, u8)> = answers.iter()
            .filter(|(_, r)| r.choice != correct_index)
            .map(|(k, r)| (k.as_str(), r.choice))
            .collect();
        correct_users.sort();
        wrong_users.sort_by_key(|&(uid, _)| uid);

        // Update round scores.
        for (user_id, rec) in &answers {
            let entry: &mut (u32, u32) = round_scores.entry(user_id.clone()).or_default();
            entry.1 += 1;
            if rec.choice == correct_index { entry.0 += 1; }
        }

        // ── Persist to DB ─────────────────────────────────────────────────────
        if let Err(e) = ctx.db
            .insert_answers(question_id, round_id, &answers, correct_index)
            .await
        {
            error!("DB insert_answers failed: {e}");
        }
        if let Err(e) = ctx.db
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
        let mut result_lines = vec![
            format!("✅ {correct_emoji} **{correct_text}**"),
        ];
        if answers.is_empty() {
            result_lines.push("No answers.".to_owned());
        } else if correct_users.is_empty() {
            result_lines.push("Nobody got it right 😅".to_owned());
        } else {
            result_lines.push(format!("🎉 {}", correct_users.join(", ")));
        }
        if !wrong_users.is_empty() {
            let wrong_str = wrong_users.iter()
                .map(|&(uid, choice)| {
                    let emoji = CHOICE_EMOJIS.get(choice as usize).copied().unwrap_or("?");
                    format!("{uid} ({emoji})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            result_lines.push(format!("❌ {wrong_str}"));
        }
        if q_num < n_questions {
            result_lines.push(format!("⏭️ Next in {inter_pause}s"));
        }

        let all_user_ids: Vec<String> = answers.keys().cloned().collect();
        {
            let user_refs: Vec<&str> = all_user_ids.iter().map(String::as_str).collect();
            let names = fetch_names(&room, &user_refs).await;
            if let Err(e) = ctx.db.update_display_names(&names).await {
                warn!("DB update_display_names failed: {e}");
            }
            let send_result = room.send(crate::format::mentionify_with_names(
                &result_lines.join("\n"), &names,
            )).await;

            // If configured, post a quiz explanation as a thread reply.
            if let (Ok(resp), Some(api_key)) = (
                send_result,
                ctx.config.explainer.api_key.clone(),
            ) {
                let result_event_id = resp.response.event_id;
                let model    = ctx.config.explainer.model.clone();
                let question = fetched.question.clone();
                let answer   = fetched.correct_answer.clone();
                let room2    = room.clone();
                tokio::spawn(async move {
                    if let Some(explanation) =
                        crate::explainer::explain(&question, &answer, &api_key, &model).await
                    {
                        let mut content = RoomMessageEventContent::text_plain(&explanation);
                        content.relates_to = Some(Relation::Thread(Thread::reply(
                            result_event_id.clone(),
                            result_event_id,
                        )));
                        room2.send(content).await.ok();
                    }
                });
            }
        }

        if q_num < n_questions {
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

    if let Err(e) = ctx.db.finish_round(round_id, questions_asked as i32).await {
        error!("DB finish_round failed: {e}");
    }
    if let Err(e) = ctx.db.write_round_scores(round_id, &round_scores).await {
        error!("DB write_round_scores failed: {e}");
    }

    // ── Round summary ─────────────────────────────────────────────────────────
    if n_questions > 1 {
        let mut summary_lines = vec![format!("🏁 Round done · {questions_asked} Qs")];

        if round_scores.is_empty() {
            summary_lines.push(String::new());
            summary_lines.push("Nobody got it right.".to_owned());
        } else {
            let mut podium: Vec<(&String, u32, u32)> = round_scores
                .iter()
                .map(|(u, &(c, t))| (u, c, t))
                .collect();
            podium.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            summary_lines.push(String::new());
            summary_lines.push("🎯 This round:".to_owned());
            for (i, (user, correct, _)) in podium.iter().take(5).enumerate() {
                let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "▪️" };
                summary_lines.push(format!("{medal} {} · {correct}/{questions_asked}", user));
            }
        }

        match ctx.db.leaderboard().await {
            Ok(board) if !board.is_empty() => {
                let q_count = ctx.db.question_count().await.unwrap_or(0);
                summary_lines.push(String::new());
                summary_lines.push(format!("🏆 All-time · {q_count} Qs:"));
                for (i, entry) in board.iter().take(5).enumerate() {
                    let pct   = if entry.total_questions > 0 {
                        (entry.total_correct * 100 / entry.total_questions) as u32
                    } else { 0 };
                    let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "▪️" };
                    summary_lines.push(format!(
                        "{medal} {} · {}/{} · {}%",
                        entry.user_id, entry.total_correct, entry.total_questions, pct,
                    ));
                }

                let mut uid_set: std::collections::HashSet<&str> =
                    round_scores.keys().map(String::as_str).collect();
                for e in board.iter().take(5) { uid_set.insert(&e.user_id); }
                let uid_vec: Vec<&str> = uid_set.into_iter().collect();
                let names = fetch_names(&room, &uid_vec).await;
                room.send(crate::format::mentionify_with_names(
                    &summary_lines.join("\n"), &names,
                )).await.ok();
            }
            Ok(_) | Err(_) => {
                room.send(RoomMessageEventContent::text_plain(
                    &summary_lines.join("\n"),
                )).await.ok();
            }
        }
    }

    info!("Quiz round finished ({questions_asked} questions)");
    Ok(())
}
