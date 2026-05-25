use anyhow::{anyhow, Result};
use chrono::Timelike as _;
use chrono_tz::Tz;
use matrix_sdk::ruma::OwnedUserId;
use tracing::error;

use crate::{BotContext, config::ScheduleConfig, fetcher, state::ScheduledOnce};

pub async fn handle(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let cmd = body.split_whitespace().next().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "!startquiz"     => cmd_startquiz(ctx, sender).await,
        "!schedulequiz"  => cmd_schedulequiz(ctx, sender, body).await,
        "!cancelquiz"    => cmd_cancelquiz(ctx, sender, body).await,
        "!prefetch"      => cmd_prefetch(ctx, sender).await,
        "!resetstats"    => cmd_resetstats(ctx, sender, body).await,
        "!scores"
        | "!leaderboard" => cmd_scores(ctx).await,
        "!mystats"       => cmd_mystats(ctx, sender).await,
        "!categories"    => cmd_categories(ctx).await,
        "!fastest"       => cmd_fastest(ctx).await,
        "!help"          => Ok(Some(help_text())),
        _                => Ok(None),
    }
}

fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    if ctx.admin_users.contains(sender) {
        Ok(())
    } else {
        Err(anyhow!("__not_admin__"))
    }
}

// ── !startquiz ────────────────────────────────────────────────────────────────

async fn cmd_startquiz(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    {
        let aq = ctx.active_quiz.lock().await;
        if aq.is_some() {
            return Ok(Some("⚠️ A quiz is already in progress!".to_owned()));
        }
    }

    let ctx2   = ctx.clone();
    let client = ctx.client.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::quiz::start_quiz(ctx2, client, true, None).await {
            error!("Manual quiz error: {e}");
        }
    });

    Ok(Some(format!(
        "🎯 Quiz starting!  You have {} seconds to answer.",
        ctx.config.schedule.answer_timeout_secs,
    )))
}

// ── !prefetch ─────────────────────────────────────────────────────────────────

async fn cmd_prefetch(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let cached_before = ctx.state.lock().await.cached_questions.len();
    match fetcher::prefetch(ctx).await {
        Ok(n)  => {
            let total = ctx.state.lock().await.cached_questions.len();
            Ok(Some(format!(
                "✅ Fetched {n} questions from OpenTDB.  Cache: {cached_before} → {total}."
            )))
        }
        Err(e) => Ok(Some(format!("❌ Prefetch failed: {e}"))),
    }
}

// ── !resetstats ───────────────────────────────────────────────────────────────

async fn cmd_resetstats(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let confirmed = body.split_whitespace().nth(1).unwrap_or("") == "confirm";
    if !confirmed {
        return Ok(Some(
            "⚠️ This will delete ALL quiz history — rounds, questions, answers, scores and players.\n\
             To confirm: !resetstats confirm".to_owned()
        ));
    }

    match ctx.db.reset_stats().await {
        Ok(()) => Ok(Some(
            "✅ All stats have been reset. Leaderboard and history wiped.".to_owned()
        )),
        Err(e) => {
            error!("reset_stats failed: {e}");
            Ok(Some("❌ Reset failed — check the logs.".to_owned()))
        }
    }
}

// ── !scores / !leaderboard ────────────────────────────────────────────────────

async fn cmd_scores(ctx: &BotContext) -> Result<Option<String>> {
    let board = match ctx.db.leaderboard().await {
        Ok(b)  => b,
        Err(e) => {
            error!("DB leaderboard error: {e}");
            return Ok(Some("❌ Could not read leaderboard from database.".to_owned()));
        }
    };
    if board.is_empty() {
        return Ok(Some("No scores yet — no quizzes have been played.".to_owned()));
    }
    let round_count = ctx.db.round_count().await.unwrap_or(0);
    let mut lines = vec![format!("🏆 Leaderboard  ({} round(s) played)", round_count)];
    lines.push(String::new());
    for (i, entry) in board.iter().enumerate() {
        let pct   = if entry.total_questions > 0 {
            entry.total_correct * 100 / entry.total_questions
        } else { 0 };
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!(
            "{medal} {:>2}. {} — {}/{} correct ({}%, {} round(s), score {:.2})",
            i + 1, entry.user_id,
            entry.total_correct, entry.total_questions, pct,
            entry.rounds_played, entry.wilson_score,
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ── !mystats ──────────────────────────────────────────────────────────────────

async fn cmd_mystats(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    let user = sender.as_str();

    let stats = match ctx.db.user_stats(user).await {
        Err(e) => {
            error!("DB user_stats error: {e}");
            return Ok(Some("❌ Could not read stats from database.".to_owned()));
        }
        Ok(None) => return Ok(Some(
            "You haven't answered any quiz questions yet.".to_owned()
        )),
        Ok(Some(s)) => s,
    };

    let pct = if stats.total_questions > 0 {
        stats.total_correct * 100 / stats.total_questions
    } else { 0 };

    let board = ctx.db.leaderboard().await.unwrap_or_default();
    let rank  = board.iter().position(|e| e.user_id == user).map(|i| i + 1);
    let rank_str = rank
        .map(|r| format!("  |  rank #{r} of {}", board.len()))
        .unwrap_or_default();

    let mut lines = vec![format!(
        "📊 Your stats: {}/{} correct ({}%)  |  {} round(s) played{rank_str}",
        stats.total_correct, stats.total_questions, pct, stats.rounds_played,
    )];

    // Best / worst category (requires ≥ 2 answers per category).
    let cat_stats = ctx.db.user_category_stats(user).await.unwrap_or_default();
    if cat_stats.len() >= 2 {
        let best  = cat_stats.first().unwrap();
        let worst = cat_stats.last().unwrap();
        let best_pct  = best.correct  * 100 / best.answered;
        let worst_pct = worst.correct * 100 / worst.answered;
        lines.push(format!("🏆 Best:  {} ({}%)", best.category,  best_pct));
        lines.push(format!("😬 Worst: {} ({}%)", worst.category, worst_pct));
    }

    Ok(Some(lines.join("\n")))
}

// ── !categories ───────────────────────────────────────────────────────────────

async fn cmd_categories(ctx: &BotContext) -> Result<Option<String>> {
    let stats = match ctx.db.category_stats().await {
        Ok(s)  => s,
        Err(e) => {
            error!("category_stats: {e}");
            return Ok(Some("❌ Could not read category stats from database.".to_owned()));
        }
    };
    if stats.is_empty() {
        return Ok(Some("No questions asked yet.".to_owned()));
    }

    let total_q: i64 = stats.iter().map(|s| s.questions_asked).sum();
    let max_asked    = stats.iter().map(|s| s.questions_asked).max().unwrap_or(1);

    let mut lines = vec![
        format!("📊 Categories  ({} questions asked)", total_q),
        String::new(),
    ];

    const BAR_W: usize = 10;
    for s in &stats {
        let filled  = (s.questions_asked * BAR_W as i64 / max_asked) as usize;
        let bar     = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_W - filled));
        let pct     = if s.total_answers > 0 {
            s.correct_answers * 100 / s.total_answers
        } else { 0 };
        lines.push(format!(
            "{bar}  {:>2}q  {:>3}% ✓  {}",
            s.questions_asked, pct, s.category,
        ));
    }

    Ok(Some(lines.join("\n")))
}

// ── !fastest ──────────────────────────────────────────────────────────────────

async fn cmd_fastest(ctx: &BotContext) -> Result<Option<String>> {
    let board = match ctx.db.speed_leaderboard().await {
        Ok(b)  => b,
        Err(e) => {
            error!("speed_leaderboard: {e}");
            return Ok(Some("❌ Could not read speed stats from database.".to_owned()));
        }
    };
    if board.is_empty() {
        return Ok(Some(
            "Not enough data yet — need at least 3 correct answers per player.".to_owned()
        ));
    }

    let mut lines = vec![
        "⚡ Speed Leaderboard  (correct answers only, min. 3 samples)".to_owned(),
        String::new(),
    ];
    for (i, e) in board.iter().enumerate() {
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!(
            "{medal} {:>2}. {} — avg {:.1}s  ({} correct answers)",
            i + 1, e.user_id, e.avg_secs, e.sample_count,
        ));
    }

    Ok(Some(lines.join("\n")))
}

// ── !schedulequiz ─────────────────────────────────────────────────────────────

async fn cmd_schedulequiz(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    // Collect all tokens after the command name, stripping any surrounding quotes.
    let time_arg = body
        .splitn(2, char::is_whitespace)
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');

    if time_arg.is_empty() {
        // Show pending one-time quizzes.
        let entries = ctx.state.lock().await.scheduled_once.clone();
        if entries.is_empty() {
            return Ok(Some(
                "No one-time quizzes scheduled.\n\
                 Usage: !schedulequiz HH:MM".to_owned()
            ));
        }
        let mut lines = vec!["📅 Pending one-time quizzes:".to_owned()];
        for e in &entries {
            lines.push(format!("  • {} on {}", e.quiz_time, e.date));
        }
        return Ok(Some(lines.join("\n")));
    }

    let (qh, qm) = match ScheduleConfig::parse_quiz_time(time_arg) {
        Some(t) => t,
        None    => return Ok(Some(format!(
            "❌ Invalid time \"{time_arg}\" — use HH:MM, e.g. !schedulequiz 21:00"
        ))),
    };

    let tz: Tz     = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now  = chrono::Utc::now().with_timezone(&tz);
    let offset     = ctx.config.schedule.reminder_before_secs as i64;

    let quiz_secs  = (qh * 3600 + qm * 60) as i64;
    let fire_secs  = (quiz_secs - offset).rem_euclid(86400);
    let now_secs   = (local_now.hour() * 3600
        + local_now.minute() * 60
        + local_now.second()) as i64;

    // If the fire moment has already passed today, schedule for tomorrow.
    let date = if now_secs >= fire_secs {
        local_now.date_naive() + chrono::Duration::days(1)
    } else {
        local_now.date_naive()
    };

    let quiz_time = format!("{qh:02}:{qm:02}");
    let entry     = ScheduledOnce { quiz_time: quiz_time.clone(), date };

    {
        let mut state = ctx.state.lock().await;
        if state.scheduled_once.iter().any(|e| e.quiz_time == quiz_time && e.date == date) {
            return Ok(Some(format!(
                "⚠️ A quiz at {quiz_time} on {date} is already scheduled."
            )));
        }
        state.scheduled_once.push(entry);
        state.save(&ctx.state_path).await?;
    }

    let day_str   = if date == local_now.date_naive() { "today".to_owned() } else { "tomorrow".to_owned() };
    let fire_hour = (fire_secs / 3600) as u32;
    let fire_min  = ((fire_secs % 3600) / 60) as u32;

    if offset > 0 {
        Ok(Some(format!(
            "✅ Quiz scheduled for {day_str} at {quiz_time} \
             (reminder fires at {fire_hour:02}:{fire_min:02}).\n\
             Cancel with: !cancelquiz {quiz_time}"
        )))
    } else {
        Ok(Some(format!(
            "✅ Quiz scheduled for {day_str} at {quiz_time}.\n\
             Cancel with: !cancelquiz {quiz_time}"
        )))
    }
}

// ── !cancelquiz ───────────────────────────────────────────────────────────────

async fn cmd_cancelquiz(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let time_arg = body
        .splitn(2, char::is_whitespace)
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');

    if time_arg.is_empty() {
        return Ok(Some(
            "Usage: !cancelquiz HH:MM\n\
             See pending quizzes with: !schedulequiz".to_owned()
        ));
    }

    let (qh, qm) = match ScheduleConfig::parse_quiz_time(time_arg) {
        Some(t) => t,
        None    => return Ok(Some(format!(
            "❌ Invalid time \"{time_arg}\" — use HH:MM, e.g. !cancelquiz 21:00"
        ))),
    };

    let quiz_time = format!("{qh:02}:{qm:02}");

    let mut state   = ctx.state.lock().await;
    let before      = state.scheduled_once.len();
    state.scheduled_once.retain(|e| e.quiz_time != quiz_time);
    let removed     = before - state.scheduled_once.len();
    state.save(&ctx.state_path).await?;

    if removed == 0 {
        Ok(Some(format!("⚠️ No scheduled quiz found for {quiz_time}.")))
    } else {
        Ok(Some(format!("✅ Cancelled {removed} quiz(zes) at {quiz_time}.")))
    }
}

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🧠 Quiz Bot commands:

  !scores / !leaderboard   — show the ranking of all players
  !mystats                 — your personal score, rank and best/worst category
  !categories              — bar chart of every category asked with accuracy
  !fastest                 — speed leaderboard (avg seconds to correct answer)
  !help                    — show this help

Admin commands:
  !startquiz               — start a quiz right now (no reminder)
  !schedulequiz HH:MM      — schedule a one-time quiz (with reminder)
  !schedulequiz            — list pending one-time quizzes
  !cancelquiz HH:MM        — cancel a pending one-time quiz
  !prefetch                — manually pre-fetch a question batch from OpenTDB
  !resetstats confirm      — wipe all quiz history and reset the leaderboard

During a quiz, submit your answer in either way:
  • React with 🇦 🇧 🇨 🇩  (reaction is hidden immediately after)
  • Type  !a  !b  !c  !d  (message is hidden immediately after)
You can change your answer at any time before the timer runs out.

Questions are sourced automatically from https://opentdb.com/."
        .to_owned()
}
