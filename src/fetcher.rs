//! Fetches trivia questions from the Open Trivia Database (opentdb.com).
//!
//! Uses a session token to avoid repeating questions until the full pool is
//! exhausted, then resets the token automatically.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{BotContext, state::FetchedQuestion};

const TOKEN_URL: &str = "https://opentdb.com/api_token.php";
const API_URL:   &str = "https://opentdb.com/api.php";

// ── OpenTDB response shapes ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    response_code: u8,
    token: Option<String>,
}

#[derive(Deserialize)]
struct ApiResponse {
    response_code: u8,
    results: Option<Vec<ApiQuestion>>,
}

#[derive(Deserialize)]
struct ApiQuestion {
    category:          String,
    difficulty:        String,
    question:          String,
    correct_answer:    String,
    incorrect_answers: Vec<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode a base64-encoded OpenTDB string field.
fn decode(s: &str) -> String {
    STANDARD
        .decode(s)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| s.to_owned())
}

// ── Token management ──────────────────────────────────────────────────────────

/// Return the stored session token, requesting a fresh one if none exists.
async fn ensure_token(ctx: &BotContext) -> anyhow::Result<String> {
    {
        let state = ctx.state.lock().await;
        if let Some(tok) = &state.opentdb_token {
            return Ok(tok.clone());
        }
    }
    let resp: TokenResponse = reqwest::get(format!("{TOKEN_URL}?command=request"))
        .await?
        .json()
        .await?;
    if resp.response_code != 0 {
        anyhow::bail!("OpenTDB token request failed (code {})", resp.response_code);
    }
    let token = resp.token.unwrap_or_default();
    {
        let mut state = ctx.state.lock().await;
        state.opentdb_token = Some(token.clone());
        state.save(&ctx.state_path).await?;
    }
    info!("Obtained new OpenTDB session token");
    Ok(token)
}

/// Reset a token after its question pool is exhausted.
async fn reset_token(_ctx: &BotContext, token: &str) -> anyhow::Result<()> {
    let resp: TokenResponse =
        reqwest::get(format!("{TOKEN_URL}?command=reset&token={token}"))
            .await?
            .json()
            .await?;
    if resp.response_code != 0 {
        anyhow::bail!("OpenTDB token reset failed (code {})", resp.response_code);
    }
    info!("Reset OpenTDB session token");
    Ok(())
}

// ── Fetching ──────────────────────────────────────────────────────────────────

/// Fetch a batch of questions from OpenTDB and append them to the cache.
/// Returns the number of questions added.
pub async fn prefetch(ctx: &BotContext) -> anyhow::Result<usize> {
    let trivia = &ctx.config.trivia;
    let amount = trivia.batch_size.clamp(1, 50);

    // At most one token reset retry — avoids infinite loops.
    for attempt in 0u8..2 {
        let token = ensure_token(ctx).await?;

        let mut url = format!(
            "{API_URL}?amount={amount}&type=multiple&encode=base64&token={token}"
        );
        if let Some(cat) = trivia.category {
            url.push_str(&format!("&category={cat}"));
        }
        if let Some(diff) = &trivia.difficulty {
            url.push_str(&format!("&difficulty={diff}"));
        }

        let resp: ApiResponse = reqwest::get(&url).await?.json().await?;

        match resp.response_code {
            0 => {
                let questions: Vec<FetchedQuestion> = resp
                    .results
                    .unwrap_or_default()
                    .into_iter()
                    .map(|q| FetchedQuestion {
                        category:          decode(&q.category),
                        difficulty:        decode(&q.difficulty),
                        question:          decode(&q.question),
                        correct_answer:    decode(&q.correct_answer),
                        incorrect_answers: q.incorrect_answers.iter().map(|s| decode(s)).collect(),
                    })
                    .collect();
                let n = questions.len();
                let mut state = ctx.state.lock().await;
                state.cached_questions.extend(questions);
                state.save(&ctx.state_path).await?;
                let total = state.cached_questions.len();
                info!("Prefetched {n} questions from OpenTDB ({total} in cache)");
                return Ok(n);
            }
            4 if attempt == 0 => {
                // Token pool exhausted — reset and retry.
                warn!("OpenTDB token exhausted, resetting");
                reset_token(ctx, &token).await?;
            }
            5 => anyhow::bail!("OpenTDB rate-limited — wait a few seconds and try again"),
            c => anyhow::bail!("OpenTDB API error (response_code {c})"),
        }
    }

    anyhow::bail!("OpenTDB prefetch failed after token reset")
}

/// Pop the next question from the cache, fetching a fresh batch if empty.
/// Also triggers a background refill when the cache runs low.
pub async fn next_question(ctx: &BotContext) -> anyhow::Result<FetchedQuestion> {
    // Pop from cache
    let question = {
        let mut state = ctx.state.lock().await;
        state.cached_questions.pop_front()
    };

    if let Some(q) = question {
        // If the cache is getting low, refill in the background.
        let remaining = ctx.state.lock().await.cached_questions.len();
        if remaining < 3 {
            let ctx2 = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = prefetch(&ctx2).await {
                    warn!("Background prefetch failed: {e}");
                }
            });
        }
        return Ok(q);
    }

    // Cache was empty — fetch synchronously.
    prefetch(ctx).await?;
    ctx.state
        .lock()
        .await
        .cached_questions
        .pop_front()
        .ok_or_else(|| anyhow::anyhow!("OpenTDB returned no questions"))
}
