//! Fetches trivia questions from the Open Trivia Database (opentdb.com).
//!
//! Uses a session token to avoid repeating questions until the full pool is
//! exhausted, then resets the token automatically.

use std::collections::{HashMap, HashSet};

use base64::{engine::general_purpose::STANDARD, Engine};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{state::FetchedQuestion, BotContext};

const TOKEN_URL: &str = "https://opentdb.com/api_token.php";
const API_URL: &str = "https://opentdb.com/api.php";

/// Category groups for balanced random selection.
///
/// A random group is picked first, then a random category within it.
/// This prevents over-represented super-categories (Entertainment has 9
/// sub-categories; without grouping it would be chosen ~37% of the time).
///
/// Each group has equal probability; sub-categories within a group have
/// equal probability among themselves.
///
/// The name field matches the `excluded_categories` config option
/// (case-insensitive; "&" and "and" are treated as equivalent).
pub const CATEGORY_GROUPS: &[(&str, &[u32])] = &[
    ("General Knowledge", &[9]),
    ("Entertainment", &[10, 11, 12, 13, 14, 15, 16, 29, 31, 32]),
    ("Science & Technology", &[17, 18, 19, 30]),
    ("Mythology", &[20]),
    ("Sports", &[21]),
    ("Geography", &[22]),
    ("History", &[23]),
    ("Politics", &[24]),
    ("Art", &[25]),
    ("Celebrities", &[26]),
    ("Animals", &[27]),
    ("Vehicles", &[28]),
];

/// Normalise a category name for exclusion matching:
/// lower-case and replace " & " with " and ".
pub fn normalise(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .replace('&', " and ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map OpenTDB's raw category names back to the bot's balanced groups.
pub fn category_group_for_category(category: &str) -> Option<&'static str> {
    let c = normalise(category);
    match c.as_str() {
        "general knowledge" => Some("General Knowledge"),
        "science and nature"
        | "science: computers"
        | "science: mathematics"
        | "science: gadgets" => Some("Science & Technology"),
        "mythology" => Some("Mythology"),
        "sports" => Some("Sports"),
        "geography" => Some("Geography"),
        "history" => Some("History"),
        "politics" => Some("Politics"),
        "art" => Some("Art"),
        "celebrities" => Some("Celebrities"),
        "animals" => Some("Animals"),
        "vehicles" => Some("Vehicles"),
        _ if c.starts_with("entertainment:") => Some("Entertainment"),
        _ => None,
    }
}

pub fn category_group_label(category: &str) -> String {
    category_group_for_category(category)
        .unwrap_or(category)
        .to_owned()
}

/// Return the subset of `CATEGORY_GROUPS` not excluded by config.
/// Falls back to all groups if every group is excluded (avoids an empty pool).
pub fn active_groups<'a>(excluded: &[String]) -> Vec<(&'a str, &'a [u32])> {
    let excluded_norm: Vec<String> = excluded.iter().map(|s| normalise(s)).collect();
    let filtered: Vec<_> = CATEGORY_GROUPS
        .iter()
        .filter(|(name, _)| !excluded_norm.contains(&normalise(name)))
        .copied()
        .collect();
    if filtered.is_empty() {
        CATEGORY_GROUPS.to_vec()
    } else {
        filtered
    }
}

pub fn category_is_active(category: &str, excluded: &[String]) -> bool {
    let Some(group) = category_group_for_category(category) else {
        return true;
    };
    active_groups(excluded)
        .iter()
        .any(|(name, _)| normalise(name) == normalise(group))
}

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
                Err(e) => {
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

    Err(last_err.context(format!(
        "OpenTDB unreachable after {MAX_NET_RETRIES} attempts"
    )))
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
    category: String,
    difficulty: String,
    question: String,
    correct_answer: String,
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

fn question_is_valid(q: &FetchedQuestion) -> bool {
    if q.category.trim().is_empty()
        || q.difficulty.trim().is_empty()
        || q.question.trim().is_empty()
        || q.correct_answer.trim().is_empty()
        || q.incorrect_answers.len() != 3
    {
        return false;
    }

    let mut answers = HashSet::new();
    answers.insert(normalise(q.correct_answer.trim()));
    q.incorrect_answers
        .iter()
        .all(|answer| !answer.trim().is_empty() && answers.insert(normalise(answer.trim())))
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
    let resp: TokenResponse = reqwest::get(format!("{TOKEN_URL}?command=reset&token={token}"))
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
            Ok(t) => t,
            Err(e) => {
                warn!("ensure_token failed: {e}");
                continue;
            }
        };

        // Use the configured category if set; otherwise pick a random group
        // then a random sub-category within it.  Two-level selection gives
        // each thematic group equal probability regardless of how many
        // sub-categories it contains.
        let category: u32 = trivia.category.unwrap_or_else(|| {
            let mut rng = rand::thread_rng();
            let groups = active_groups(&trivia.excluded_categories);
            let (_, ids) = groups.choose(&mut rng).expect("non-empty");
            *ids.choose(&mut rng).expect("non-empty")
        });

        let mut url = format!(
            "{API_URL}?amount={amount}&type=multiple&encode=base64&token={token}&category={category}"
        );
        if let Some(diff) = &trivia.difficulty {
            url.push_str(&format!("&difficulty={diff}"));
        }

        let resp = match api_get_with_retry(&url).await {
            Ok(r) => r,
            Err(e) => {
                warn!("OpenTDB prefetch network error (attempt {attempt}): {e}");
                continue;
            }
        };

        match resp.response_code {
            0 => {
                let decoded: Vec<FetchedQuestion> = resp
                    .results
                    .unwrap_or_default()
                    .into_iter()
                    .map(|q| FetchedQuestion {
                        category: decode(&q.category),
                        difficulty: decode(&q.difficulty),
                        question: decode(&q.question),
                        correct_answer: decode(&q.correct_answer),
                        incorrect_answers: q.incorrect_answers.iter().map(|s| decode(s)).collect(),
                    })
                    .collect();
                let total_decoded = decoded.len();
                let questions: Vec<_> = decoded.into_iter().filter(question_is_valid).collect();
                let n = questions.len();
                if n < total_decoded {
                    warn!(
                        "OpenTDB category {category}: discarded {} malformed questions",
                        total_decoded - n
                    );
                }
                if n == 0 {
                    warn!("OpenTDB category {category}: response contained no usable questions");
                    continue;
                }
                let mut state = ctx.state.lock().await;
                state.cached_questions.extend(questions);
                state.save(&ctx.state_path).await?;
                let total = state.cached_questions.len();
                info!(
                    "Prefetched {n} questions from OpenTDB category {category} ({total} in cache)"
                );
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct CategoryChoice {
    group: String,
    category_id: u32,
}

/// Prefer groups outside the recent window, then choose the least-used group.
/// Recent exclusions are relaxed when necessary and groups are not repeated
/// within a round until every active group has been used.
fn select_round_categories<R: Rng + ?Sized>(
    groups: &[(&str, &[u32])],
    initial_counts: &HashMap<String, i64>,
    recent: &[String],
    n: usize,
    rng: &mut R,
) -> Vec<CategoryChoice> {
    let mut counts = initial_counts.clone();
    let recent_norm: HashSet<String> = recent.iter().map(|name| normalise(name)).collect();
    let mut used_in_round = HashSet::new();
    let mut choices: Vec<CategoryChoice> = Vec::with_capacity(n);

    while choices.len() < n {
        if used_in_round.len() == groups.len() {
            used_in_round.clear();
        }

        let available: Vec<_> = groups
            .iter()
            .copied()
            .filter(|(name, _)| !used_in_round.contains(&normalise(name)))
            .collect();
        let last_group = choices.last().map(|choice| normalise(&choice.group));
        let non_repeating: Vec<_> = available
            .iter()
            .copied()
            .filter(|(name, _)| {
                groups.len() == 1 || last_group.as_deref() != Some(normalise(name).as_str())
            })
            .collect();
        let available = if non_repeating.is_empty() {
            available
        } else {
            non_repeating
        };
        let preferred: Vec<_> = available
            .iter()
            .copied()
            .filter(|(name, _)| !recent_norm.contains(&normalise(name)))
            .collect();
        let pool = if preferred.is_empty() {
            &available
        } else {
            &preferred
        };
        let min_count = pool
            .iter()
            .map(|(name, _)| counts.get(*name).copied().unwrap_or(0))
            .min()
            .unwrap_or(0);
        let least_used: Vec<_> = pool
            .iter()
            .copied()
            .filter(|(name, _)| counts.get(*name).copied().unwrap_or(0) == min_count)
            .collect();
        let (name, ids) = least_used.choose(rng).expect("active category pool is non-empty");
        let category_id = ids
            .choose(rng)
            .copied()
            .expect("category group has IDs");

        choices.push(CategoryChoice {
            group: (*name).to_owned(),
            category_id,
        });
        used_in_round.insert(normalise(name));
        *counts.entry((*name).to_owned()).or_insert(0) += 1;
    }

    choices
}

async fn pick_round_categories(ctx: &BotContext, n: usize) -> Vec<CategoryChoice> {
    let groups = active_groups(&ctx.config.trivia.excluded_categories);
    let counts: HashMap<String, i64> = ctx
        .db
        .category_group_counts()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let recent = ctx
        .db
        .recent_category_groups(ctx.config.trivia.recent_category_window)
        .await
        .unwrap_or_else(|e| {
            warn!("Could not load recent category history: {e}");
            Vec::new()
        });
    let candidate_names: Vec<_> = groups.iter().map(|(name, _)| *name).collect();
    info!(
        candidates = ?candidate_names,
        excluded_recent = ?recent,
        lifetime_counts = ?counts,
        "Selecting round categories"
    );
    let mut rng = rand::thread_rng();
    let choices = select_round_categories(&groups, &counts, &recent, n, &mut rng);
    for choice in &choices {
        info!(
            category_group = %choice.group,
            category_id = choice.category_id,
            "Selected round category"
        );
    }
    choices
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
            Ok(t) => t,
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
            Ok(r) => r,
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
                        category: decode(&q.category),
                        difficulty: decode(&q.difficulty),
                        question: decode(&q.question),
                        correct_answer: decode(&q.correct_answer),
                        incorrect_answers: q.incorrect_answers.iter().map(|s| decode(s)).collect(),
                    };
                    if !question_is_valid(&fetched) {
                        warn!("Discarding malformed OpenTDB question for category {category}");
                        continue;
                    }
                    let already_asked = ctx
                        .db
                        .question_exists(&fetched.question)
                        .await
                        .unwrap_or(false);
                    if !already_asked || attempt == MAX_SKIP {
                        if attempt == MAX_SKIP && already_asked {
                            warn!("Reusing duplicate question for category {category} — pool may be exhausted");
                        }
                        return Ok(fetched);
                    }
                    // Duplicate — try again.
                    info!(
                        "Skipping duplicate for category {category} ({}/{})",
                        attempt + 1,
                        MAX_SKIP
                    );
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
                Ok(q) => questions.push(q),
                Err(e) => {
                    warn!("next_question fallback failed: {e}");
                    break;
                }
            }
        }
        return questions;
    }

    let categories = pick_round_categories(ctx, n).await;
    info!(
        "Pre-fetching {} round questions from categories: {:?}",
        n, categories
    );

    // OpenTDB enforces ~1 request per 5 s per IP (response_code 5).
    // We wait between calls so we don't get rate-limited mid-prefetch.
    const RATE_LIMIT_SECS: u64 = 6;

    let mut questions = Vec::with_capacity(n);
    let recent = ctx
        .db
        .recent_category_groups(ctx.config.trivia.recent_category_window)
        .await
        .unwrap_or_default();
    let mut avoid_groups: HashSet<String> = recent.iter().map(|name| normalise(name)).collect();

    for (i, choice) in categories.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(RATE_LIMIT_SECS)).await;
        }
        match fetch_one(ctx, choice.category_id).await {
            Ok(q)
                if category_group_for_category(&q.category)
                    .is_some_and(|group| normalise(group) == normalise(&choice.group)) =>
            {
                info!(
                    "Round question ready: category {} (\"{}\")",
                    choice.category_id,
                    q.category
                );
                avoid_groups.insert(normalise(&choice.group));
                questions.push(q);
            }
            Ok(q) => {
                warn!(
                    requested_group = %choice.group,
                    requested_category_id = choice.category_id,
                    returned_category = %q.category,
                    "OpenTDB returned an unexpected or recently used category; using cache fallback"
                );
                match next_question_avoiding(ctx, &avoid_groups).await {
                    Ok(fallback) => {
                        let group = normalise(&category_group_label(&fallback.category));
                        avoid_groups.insert(group);
                        questions.push(fallback);
                    }
                    Err(e) => warn!("Cache fallback also failed: {e}"),
                }
            }
            Err(e) => {
                warn!(
                    "fetch_one failed for category {}: {e} — falling back to cache",
                    choice.category_id
                );
                match next_question_avoiding(ctx, &avoid_groups).await {
                    Ok(q) => {
                        let group = normalise(&category_group_label(&q.category));
                        avoid_groups.insert(group);
                        questions.push(q);
                    }
                    Err(e2) => warn!("Cache fallback also failed: {e2}"),
                }
            }
        }
    }
    questions
}

async fn next_question_avoiding(
    ctx: &BotContext,
    avoid_groups: &HashSet<String>,
) -> anyhow::Result<FetchedQuestion> {
    let is_empty = ctx.state.lock().await.cached_questions.is_empty();
    if is_empty {
        prefetch(ctx).await?;
    }

    let cache_len = ctx.state.lock().await.cached_questions.len();
    for _ in 0..cache_len {
        let q = ctx
            .state
            .lock()
            .await
            .cached_questions
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("question cache became empty"))?;
        let group = normalise(&category_group_label(&q.category));
        if !question_is_valid(&q) {
            warn!("Discarding malformed question already present in cache");
            continue;
        }
        if avoid_groups.contains(&group)
            || !category_is_active(&q.category, &ctx.config.trivia.excluded_categories)
        {
            ctx.state.lock().await.cached_questions.push_back(q);
            continue;
        }
        if !ctx.db.question_exists(&q.question).await.unwrap_or(false) {
            info!(category = %q.category, "Selected category-aware cache fallback");
            return Ok(q);
        }
    }

    warn!(
        excluded_recent = ?avoid_groups,
        "No cache fallback satisfied category diversity; relaxing the recent-category window"
    );
    next_question(ctx).await
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

        let q = ctx
            .state
            .lock()
            .await
            .cached_questions
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("OpenTDB returned no questions"))?;

        if !question_is_valid(&q) {
            warn!("Discarding malformed question already present in cache");
            continue;
        }

        if ctx.config.trivia.category.is_none()
            && !category_is_active(&q.category, &ctx.config.trivia.excluded_categories)
        {
            info!(
                "Skipping cached question from excluded category {}",
                q.category
            );
            continue;
        }

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

#[cfg(test)]
mod tests {
    use rand::{rngs::StdRng, SeedableRng};

    use super::*;

    #[test]
    fn underused_group_is_not_selected_three_rounds_in_a_row() {
        let groups: &[(&str, &[u32])] = &[
            ("Politics", &[24]),
            ("History", &[23]),
            ("Art", &[25]),
        ];
        let mut counts = HashMap::from([
            ("Politics".to_owned(), 0),
            ("History".to_owned(), 50),
            ("Art".to_owned(), 50),
        ]);
        let mut recent = Vec::new();
        let mut selected = Vec::new();
        let mut rng = StdRng::seed_from_u64(7);

        for _ in 0..3 {
            let choice = select_round_categories(groups, &counts, &recent, 1, &mut rng)
                .pop()
                .unwrap();
            *counts.entry(choice.group.clone()).or_default() += 1;
            recent.insert(0, choice.group.clone());
            recent.truncate(2);
            selected.push(choice.group);
        }

        assert_eq!(selected[0], "Politics");
        assert_ne!(selected[1], "Politics");
        assert_ne!(selected[2], "Politics");
    }

    #[test]
    fn a_round_uses_distinct_groups_when_enough_are_active() {
        let groups: &[(&str, &[u32])] = &[
            ("Politics", &[24]),
            ("History", &[23]),
            ("Art", &[25]),
            ("Animals", &[27]),
        ];
        let mut rng = StdRng::seed_from_u64(9);
        let choices = select_round_categories(groups, &HashMap::new(), &[], 4, &mut rng);
        let unique: HashSet<_> = choices.iter().map(|choice| &choice.group).collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn small_category_pools_relax_without_adjacent_repeats() {
        let groups: &[(&str, &[u32])] = &[("Politics", &[24]), ("History", &[23])];
        let recent = vec!["Politics".to_owned(), "History".to_owned()];
        let mut rng = StdRng::seed_from_u64(11);
        let choices = select_round_categories(groups, &HashMap::new(), &recent, 5, &mut rng);

        assert_eq!(choices.len(), 5);
        assert!(choices.windows(2).all(|pair| pair[0].group != pair[1].group));
    }

    #[test]
    fn rejects_empty_and_duplicate_answers() {
        let malformed = FetchedQuestion {
            category: "History".to_owned(),
            difficulty: "easy".to_owned(),
            question: "Question?".to_owned(),
            correct_answer: "Same".to_owned(),
            incorrect_answers: vec!["same".to_owned(), "Other".to_owned(), "Third".to_owned()],
        };
        assert!(!question_is_valid(&malformed));
    }

    #[test]
    fn category_aliases_are_normalized_consistently() {
        assert_eq!(normalise("  Science  &  Technology "), "science and technology");
        assert_eq!(
            category_group_for_category("Science & Nature"),
            Some("Science & Technology")
        );
    }
}
