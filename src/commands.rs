use anyhow::{anyhow, Result};
use matrix_sdk::ruma::OwnedUserId;
use tracing::error;

use crate::{BotContext, fetcher};

pub async fn handle(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let cmd = body.split_whitespace().next().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "!startquiz"     => cmd_startquiz(ctx, sender).await,
        "!prefetch"      => cmd_prefetch(ctx, sender).await,
        "!scores"
        | "!leaderboard" => cmd_scores(ctx).await,
        "!mystats"       => cmd_mystats(ctx, sender).await,
        "!help"          => Ok(Some(help_text())),
        _                => Ok(None),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
                "✅ Fetched {n} questions from OpenTDB.  \
                 Cache: {cached_before} → {total}."
            )))
        }
        Err(e) => Ok(Some(format!("❌ Prefetch failed: {e}"))),
    }
}

// ── !scores / !leaderboard ────────────────────────────────────────────────────

async fn cmd_scores(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let board = state.leaderboard();
    if board.is_empty() {
        return Ok(Some(
            "No scores yet — no quizzes have been played.".to_owned()
        ));
    }
    let mut lines = vec![format!(
        "🏆 Leaderboard  ({} quiz round(s) played)",
        state.results.len(),
    )];
    lines.push(String::new());
    for (i, (user, correct, total)) in board.iter().enumerate() {
        let pct   = if *total > 0 { correct * 100 / total } else { 0 };
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!(
            "{medal} {:>2}. {} — {}/{} correct ({}%)",
            i + 1, user, correct, total, pct,
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ── !mystats ──────────────────────────────────────────────────────────────────

async fn cmd_mystats(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let user  = sender.as_str();
    let stats = state.user_stats();

    match stats.get(user) {
        None => Ok(Some(
            "You haven't answered any quiz questions yet.".to_owned()
        )),
        Some(&(correct, total)) => {
            let pct   = if total > 0 { correct * 100 / total } else { 0 };
            let board = state.leaderboard();
            let rank  = board
                .iter()
                .position(|(u, _, _)| u == user)
                .map(|i| i + 1);
            let rank_str = rank
                .map(|r| format!("  |  rank #{r} of {}", board.len()))
                .unwrap_or_default();
            Ok(Some(format!(
                "📊 Your stats: {correct}/{total} correct ({pct}%){rank_str}"
            )))
        }
    }
}

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🧠 Quiz Bot commands:

  !scores / !leaderboard   — show the ranking of all players
  !mystats                 — your personal score and rank
  !help                    — show this help

Admin commands:
  !startquiz               — start a quiz right now
  !prefetch                — manually pre-fetch a question batch from OpenTDB

During a quiz, submit your answer in either way:
  • React with 🇦 🇧 🇨 🇩  (reaction is hidden immediately after)
  • Type  !a  !b  !c  !d  (message is hidden immediately after)
You can change your answer at any time before the timer runs out.

Questions are sourced automatically from https://opentdb.com/."
        .to_owned()
}
