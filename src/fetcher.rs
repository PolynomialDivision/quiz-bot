//! Fetches trivia questions from the Open Trivia Database (opentdb.com).
//!
//! Uses a session token to avoid repeating questions until the full pool is
//! exhausted, then resets the token automatically.

use base64::{engine::general_purpose::STANDARD, Engine};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{BotContext, state::FetchedQuestion};

const TOKEN_URL: &str = "https://opentdb.com/api_token.php";
const API_URL:   &str = "https://opentdb.com/api.php";

/// Category groups for balanced random selection.
///
/// A random group is picked first, then a random category within it.
/// This prevents over-represented super-categories (Entertainment has 9
/// sub-categories; without grouping it would be chosen ~37% of the time).
///
/// Each group has equal probability; sub-categories within a group have
/// equal probability among themselves.
const CATEGORY_GROUPS: &[&[u32]] = &[
    &[9],                              // General Knowledge
    &[10, 11, 12, 13, 14, 15, 16, 29, 31, 32], // Entertainment (all variants)
    &[17, 18, 19, 30],                 // Science & Technology
    &[20],                             // Mythology
    &[21],                             // Sports
    &[22],                             // Geography
    &[23],                             // History
    &[24],                             // Politics
    &[25],                             // Art
    &[26],                             // Celebrities
    &[27],                             // Animals
    &[28],                             // Vehicles
];

// ── Resilient HTTP helper ─────────────────────────────────────────────────────

/// GET `url`, parse as `ApiResponse`, retrying on network/parse errors.
/// Gives up after `MAX_NET_RETRIES` attempts with exponential backoff.
/// Response-code errors (e.g. rate-limit, bad token) are returned as-is
/// for the caller to handle — only transport-level failures are retried here.
async fn api_get_with_retry(url: &str) -> anyhow::Result<ApiResponse> {
    const MAX_NET_RETRIES: u32 = 5;
    let mut last_err = anyhow::anyhow!("no attempts made");

    for attempt in 0..MAX_NET_RETRIES {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(30); // 1 s, 2 s, 4 s, 8 s, 16 s
            warn!("OpenTDB: network retry {attempt}/{MAX_NET_RETRIES} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }
        match reqwest::get(url).await {
            Ok(resp) => match resp.json::<ApiResponse>().await {
                Ok(api) => return Ok(api),
                Err(e)  => {
                    warn!("OpenTDB: response parse error: {e}");
                    last_err = e.into();
                }
            },
            Err(e) => {
                warn!("OpenTDB: request error: {e}");
                last_err = e.into();
            }
        }
    }

    Err(last_err.context(format!("OpenTDB unreachable after {MAX_NET_RETRIES} attempts")))
}

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

    const MAX_ATTEMPTS: u32 = 5;
    let mut token_refreshed = false;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(30); // 1 s, 2 s, 4 s, 8 s, …
            warn!("OpenTDB prefetch retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let token = match ensure_token(ctx).await {
            Ok(t)  => t,
            Err(e) => { warn!("ensure_token failed: {e}"); continue; }
        };

        // Use the configured category if set; otherwise pick a random group
        // then a random sub-category within it.  Two-level selection gives
        // each thematic group equal probability regardless of how many
        // sub-categories it contains.
        let category: u32 = trivia.category.unwrap_or_else(|| {
            let mut rng = rand::thread_rng();
            let group = CATEGORY_GROUPS.choose(&mut rng).expect("non-empty");
            *group.choose(&mut rng).expect("non-empty")
        });

        let mut url = format!(
            "{API_URL}?amount={amount}&type=multiple&encode=base64&token={token}&category={category}"
        );
        if let Some(diff) = &trivia.difficulty {
            url.push_str(&format!("&difficulty={diff}"));
        }

        let resp = match api_get_with_retry(&url).await {
            Ok(r)  => r,
            Err(e) => {
                warn!("OpenTDB prefetch network error (attempt {attempt}): {e}");
                continue;
            }
        };

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
                info!("Prefetched {n} questions from OpenTDB category {category} ({total} in cache)");
                return Ok(n);
            }
            // Code 3: token expired after 6 h inactivity — clear it and retry.
            3 if !token_refreshed => {
                warn!("OpenTDB session token expired, requesting a new one");
                let mut state = ctx.state.lock().await;
                state.opentdb_token = None;
                state.save(&ctx.state_path).await?;
                token_refreshed = true;
            }
            3 => warn!("OpenTDB token still not found after refresh — retrying"),
            // Code 4: every question for the current query has been seen — reset.
            4 if !token_refreshed => {
                warn!("OpenTDB token exhausted, resetting");
                reset_token(ctx, &token).await?;
                token_refreshed = true;
            }
            4 => warn!("OpenTDB token still exhausted after reset — retrying"),
            // Code 5: rate-limited — the backoff delay at loop top handles the wait.
            5 => warn!("OpenTDB rate-limited — will retry after backoff"),
            c => warn!("OpenTDB API error (response_code {c}) — retrying"),
        }
    }

    anyhow::bail!("OpenTDB prefetch failed after {MAX_ATTEMPTS} attempts")
}

/// Pick `n` category IDs from distinct groups.
/// Groups are shuffled; if n > num_groups we wrap around (some groups used twice).
fn pick_round_categories(n: usize) -> Vec<u32> {
    let mut rng = rand::thread_rng();
    let mut groups: Vec<&[u32]> = CATEGORY_GROUPS.to_vec();
    groups.shuffle(&mut rng);
    groups
        .iter()
        .cycle()
        .take(n)
        .map(|g| *g.choose(&mut rng).unwrap())
        .collect()
}

/// Fetch exactly one question from a specific OpenTDB category, skipping
/// already-asked questions.  Does not touch the shared cache.
async fn fetch_one(ctx: &BotContext, category: u32) -> anyhow::Result<FetchedQuestion> {
    const MAX_SKIP: usize = 5;
    let difficulty = ctx.config.trivia.difficulty.clone();

    let mut token_refreshed = false;

    for attempt in 0..=MAX_SKIP {
        // At most one token-reset per call; network errors get their own retry inside api_get_with_retry.
        let token = match ensure_token(ctx).await {
            Ok(t)  => t,
            Err(e) => {
                warn!("fetch_one: ensure_token failed (attempt {attempt}): {e}");
                if attempt < MAX_SKIP {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    continue;
                }
                anyhow::bail!("ensure_token failed after all retries: {e}");
            }
        };

        let mut url = format!(
            "{API_URL}?amount=1&type=multiple&encode=base64&token={token}&category={category}"
        );
        if let Some(ref diff) = difficulty {
            url.push_str(&format!("&difficulty={diff}"));
        }

        let resp = match api_get_with_retry(&url).await {
            Ok(r)  => r,
            Err(e) => {
                warn!("fetch_one: network error (attempt {attempt}): {e}");
                if attempt < MAX_SKIP {
                    continue;
                }
                anyhow::bail!("fetch_one network error after all retries: {e}");
            }
        };

        match resp.response_code {
            0 => {
                if let Some(q) = resp.results.unwrap_or_default().into_iter().next() {
                    let fetched = FetchedQuestion {
                        category:          decode(&q.category),
                        difficulty:        decode(&q.difficulty),
                        question:          decode(&q.question),
                        correct_answer:    decode(&q.correct_answer),
                        incorrect_answers: q.incorrect_answers.iter().map(|s| decode(s)).collect(),
                    };
                    let already_asked =
                        ctx.db.question_exists(&fetched.question).await.unwrap_or(false);
                    if !already_asked || attempt == MAX_SKIP {
                        if attempt == MAX_SKIP && already_asked {
                            warn!("Reusing duplicate question for category {category} — pool may be exhausted");
                        }
                        return Ok(fetched);
                    }
                    // Duplicate — try again.
                    info!("Skipping duplicate for category {category} ({}/{})", attempt + 1, MAX_SKIP);
                    continue;
                }
                anyhow::bail!("OpenTDB returned empty results for category {category}");
            }
            // Code 3: token expired — clear and retry once.
            3 if !token_refreshed => {
                warn!("OpenTDB token not found (expired) for category {category}, refreshing");
                let mut state = ctx.state.lock().await;
                state.opentdb_token = None;
                state.save(&ctx.state_path).await?;
                token_refreshed = true;
            }
            3 => anyhow::bail!("OpenTDB token not found even after refresh"),
            // Code 4: token exhausted — reset and retry once.
            4 if !token_refreshed => {
                warn!("OpenTDB token exhausted for category {category}, resetting");
                reset_token(ctx, &token).await?;
                let mut state = ctx.state.lock().await;
                state.opentdb_token = None;
                state.save(&ctx.state_path).await?;
                token_refreshed = true;
            }
            4 => anyhow::bail!("OpenTDB token still exhausted after reset"),
            // Code 5: rate-limited — wait and retry.
            5 => {
                warn!("OpenTDB rate-limited on category {category} — waiting 6s");
                tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;
            }
            c => {
                warn!("OpenTDB API error (code {c}) for category {category} — retrying");
            }
        }
    }
    anyhow::bail!("fetch_one failed after all retries for category {category}")
}

/// Pre-fetch one question per category for an upcoming round.
///
/// Categories are drawn from distinct groups so every question in the round
/// comes from a different thematic area.  All API calls happen here, before
/// the round starts, so there are no delays between questions.
///
/// Falls back to the generic cache path for any category that fails.
pub async fn fetch_round_questions(ctx: &BotContext, n: usize) -> Vec<FetchedQuestion> {
    // If a category is locked in config, use that for every question and rely
    // on the old next_question path (no per-category pre-fetch needed).
    if ctx.config.trivia.category.is_some() {
        let mut questions = Vec::with_capacity(n);
        for _ in 0..n {
            match next_question(ctx).await {
                Ok(q)  => questions.push(q),
                Err(e) => { warn!("next_question fallback failed: {e}"); break; }
            }
        }
        return questions;
    }

    let categories = pick_round_categories(n);
    info!(
        "Pre-fetching {} round questions from categories: {:?}",
        n, categories
    );

    // OpenTDB enforces ~1 request per 5 s per IP (response_code 5).
    // We wait between calls so we don't get rate-limited mid-prefetch.
    const RATE_LIMIT_SECS: u64 = 6;

    let mut questions = Vec::with_capacity(n);
    for (i, category) in categories.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(RATE_LIMIT_SECS)).await;
        }
        match fetch_one(ctx, category).await {
            Ok(q) => {
                info!("Round question ready: category {category} (\"{}\")", q.category);
                questions.push(q);
            }
            Err(e) => {
                warn!("fetch_one failed for category {category}: {e} — falling back to cache");
                match next_question(ctx).await {
                    Ok(q)  => questions.push(q),
                    Err(e2) => warn!("Cache fallback also failed: {e2}"),
                }
            }
        }
    }
    questions
}

/// Pop the next question from the cache, skipping any already asked in a
/// previous round.  Fetches a fresh batch if the cache runs empty.
///
/// After MAX_SKIP consecutive duplicates we give up deduplication and return
/// the next available question — this prevents an infinite loop when the entire
/// OpenTDB pool has been exhausted.
pub async fn next_question(ctx: &BotContext) -> anyhow::Result<FetchedQuestion> {
    const MAX_SKIP: usize = 30;

    for attempt in 0..=MAX_SKIP {
        // Ensure the cache has at least one item.
        {
            let is_empty = ctx.state.lock().await.cached_questions.is_empty();
            if is_empty {
                prefetch(ctx).await?;
            }
        }

        let q = ctx.state
            .lock()
            .await
            .cached_questions
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("OpenTDB returned no questions"))?;

        // Trigger a background refill when the cache is getting low.
        {
            let remaining = ctx.state.lock().await.cached_questions.len();
            if remaining < 3 {
                let ctx2 = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = prefetch(&ctx2).await {
                        warn!("Background prefetch failed: {e}");
                    }
                });
            }
        }

        // Check whether this question has already been asked in a past round.
        let already_asked = ctx.db.question_exists(&q.question).await.unwrap_or(false);
        if !already_asked {
            return Ok(q);
        }

        if attempt == MAX_SKIP {
            // Entire reachable pool seems exhausted — reuse rather than hang.
            warn!(
                "Skipped {MAX_SKIP} duplicate questions — \
                 OpenTDB pool may be exhausted, reusing a question."
            );
            return Ok(q);
        }

        info!(
            "Skipping already-asked question ({}/{MAX_SKIP})",
            attempt + 1
        );
    }

    unreachable!()
}
