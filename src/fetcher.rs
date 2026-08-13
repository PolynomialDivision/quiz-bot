//! Fetches trivia questions from the Open Trivia Database (opentdb.com).
//!
//! Two independent, complementary defenses against repeats:
//!  - An OpenTDB **session token** (`token=...` on every question request):
//!    OpenTDB itself won't hand back a question already served to that
//!    token, until the token's pool for the query is exhausted
//!    (`response_code = 4`), at which point it's reset (not replaced —
//!    reset keeps the same token, just clears its "seen" list) so the same
//!    token keeps being reused indefinitely. The token is persisted in
//!    `state.json` and requested at most once (see [`ensure_token`]).
//!  - The bot's own persistent, cross-restart question history in SQLite
//!    (see `crate::db::Db::question_recently_asked`), which catches
//!    duplicates the token can't: OpenTDB's per-token memory doesn't
//!    survive a token reset/replacement, and it can't detect near-dupes
//!    that differ only in punctuation. The DB check runs on every
//!    candidate regardless of what the token already filtered out.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Utc};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::{state::FetchedQuestion, BotContext};

const TOKEN_URL: &str = "https://opentdb.com/api_token.php";
const API_URL: &str = "https://opentdb.com/api.php";

/// How long a question stays "recently asked" (and therefore avoided) after
/// it's served. Some OpenTDB categories have well under 100 questions total,
/// so deduping against unbounded history means those pools permanently
/// exhaust and every draw becomes a forced repeat. Letting matches age out
/// after ~2 months keeps the pool usable indefinitely while still keeping
/// any individual question rare on the timescale a player would notice.
pub const QUESTION_REUSE_COOLDOWN_DAYS: i64 = 60;

// ── Global request throttle ───────────────────────────────────────────────────
//
// OpenTDB enforces roughly one request per 5 s per IP, across *all* endpoints
// and tokens. Every call site used to manage its own ad hoc delays (a fixed
// sleep between round categories, a sleep only after an explicit rate-limit
// response, …), which meant bursts of near-simultaneous requests — e.g. one
// `fetch_one` call skipping several duplicate questions — routinely blew
// through the limit. OpenTDB then returned response_code 5 for every request
// in the burst, which silently ate through each call's bounded retry budget
// before a single one succeeded. Serializing every request through one gate
// removes that failure mode at the source instead of papering over it with
// more retries.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(5_500);

static LAST_REQUEST: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

/// How much longer to wait before the next request is allowed, given when
/// the previous one went out. Pure and independent of the global clock/state
/// so the spacing math can be unit-tested without real sleeps.
fn remaining_wait(previous_request: Instant, now: Instant, min_interval: Duration) -> Duration {
    min_interval.saturating_sub(now.saturating_duration_since(previous_request))
}

/// Block until it's safe to issue another OpenTDB request without tripping
/// the per-IP rate limit. Must be called immediately before every request.
async fn throttle() {
    let mut last = LAST_REQUEST.lock().await;
    if let Some(prev) = *last {
        let wait = remaining_wait(prev, Instant::now(), MIN_REQUEST_INTERVAL);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
    *last = Some(Instant::now());
}

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
        throttle().await;
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

#[derive(Deserialize, Debug)]
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

fn category_can_follow(
    previous_normalized_group: Option<&str>,
    candidate_category: &str,
    active_group_count: usize,
) -> bool {
    active_group_count <= 1
        || previous_normalized_group
            != Some(normalise(&category_group_label(candidate_category)).as_str())
}

// ── Token management ──────────────────────────────────────────────────────────
//
// Every question request (`prefetch`, `fetch_one`) carries `token=...`, so
// OpenTDB itself skips questions already served to this token. The token is
// cached in `ctx.state.opentdb_token` (persisted to state.json) so it's
// requested once, not per question, and survives a bot restart.
//
// `next_question`'s low-cache background prefetch (spawned via
// `tokio::spawn`, not covered by the round-level `quiz_run_lock`) can run
// concurrently with a round's own `fetch_one` calls. Without synchronization
// two callers could both see no cached token and each request/reset one
// independently, splitting OpenTDB's own dedup memory across two live
// tokens. `TOKEN_OP_LOCK` serializes token creation/reset so at most one
// such operation is ever in flight.

/// Serializes OpenTDB token creation and reset. Held only while a network
/// round trip for the token itself is in flight — ordinary question
/// requests (the hot path) never touch this lock.
static TOKEN_OP_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Short, non-reversible identifier for correlating token log lines —
/// never log the token itself, since it's a live credential against a
/// third-party API (anyone holding it can drain our request budget).
fn token_fingerprint(token: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

/// `GET {base}?command=request` — ask OpenTDB for a brand-new session token.
async fn request_token_at(base: &str) -> anyhow::Result<TokenResponse> {
    throttle().await;
    Ok(reqwest::get(format!("{base}?command=request"))
        .await?
        .json()
        .await?)
}

/// `GET {base}?command=reset&token=...` — clear a token's "already served"
/// memory so it can be reused. The token string itself is unchanged.
async fn reset_token_at(base: &str, token: &str) -> anyhow::Result<TokenResponse> {
    throttle().await;
    Ok(reqwest::get(format!("{base}?command=reset&token={token}"))
        .await?
        .json()
        .await?)
}

/// Double-checked-locking "get or create" for a cached value that must only
/// ever be produced once under concurrent callers: return the cached value
/// if present; otherwise take `op_lock`, re-check (another caller may have
/// just filled it while we were waiting), and only call `fetch` if it's
/// still empty.
///
/// Not used by production code — `ensure_token` needs the exact same
/// shape but operates on `ctx.state` (a `Mutex<State>`, not a bare
/// `Mutex<Option<String>>`), so it's hand-written rather than calling
/// this. This exists purely so the double-checked-locking algorithm
/// itself — the thing that makes concurrent `ensure_token` calls safe —
/// has a direct, network-free, non-BotContext test: see
/// `token_tests::acquire_token_single_flights_concurrent_callers`.
#[cfg(test)]
async fn acquire_token<F, Fut>(
    cached: &Mutex<Option<String>>,
    op_lock: &Mutex<()>,
    fetch: F,
) -> anyhow::Result<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    if let Some(tok) = cached.lock().await.clone() {
        return Ok(tok);
    }
    let _guard = op_lock.lock().await;
    if let Some(tok) = cached.lock().await.clone() {
        return Ok(tok);
    }
    let token = fetch().await?;
    *cached.lock().await = Some(token.clone());
    Ok(token)
}

/// Return the stored session token, requesting a fresh one only if none is
/// cached yet. Concurrent callers are serialized by `TOKEN_OP_LOCK` — see
/// the module-level doc comment on ``Token management``.
///
/// `token_url` is injectable so tests can point this at a local mock
/// server; production call sites always pass the `TOKEN_URL` constant.
async fn ensure_token(ctx: &BotContext, token_url: &str) -> anyhow::Result<String> {
    if let Some(tok) = ctx.state.lock().await.opentdb_token.clone() {
        return Ok(tok);
    }
    let _guard = TOKEN_OP_LOCK.lock().await;
    if let Some(tok) = ctx.state.lock().await.opentdb_token.clone() {
        debug!("OpenTDB token was obtained by a concurrent caller while waiting");
        return Ok(tok);
    }

    let resp = request_token_at(token_url).await?;
    if resp.response_code != 0 {
        anyhow::bail!("OpenTDB token request failed (code {})", resp.response_code);
    }
    let token = resp.token.unwrap_or_default();
    {
        let mut state = ctx.state.lock().await;
        state.opentdb_token = Some(token.clone());
        state.save(&ctx.state_path).await?;
    }
    info!(token_fp = %token_fingerprint(&token), "Obtained new OpenTDB session token");
    Ok(token)
}

/// Reset a token after its question pool is exhausted (`response_code = 4`).
/// The token string is unchanged — only its "already served" memory is
/// cleared — so callers keep using the same value afterward; nothing needs
/// to be re-persisted to `state.json`. Serialized by `TOKEN_OP_LOCK` so two
/// concurrent code-4 responses for the same token don't both reset it.
///
/// Takes no `BotContext`/state at all — by construction, a reset can never
/// clear the cached token (the earlier bug: the old code did exactly that
/// right after a successful reset, discarding a token that was still
/// perfectly valid and forcing a wasted extra "request new token" call).
async fn reset_token(token: &str, token_url: &str) -> anyhow::Result<()> {
    let _guard = TOKEN_OP_LOCK.lock().await;
    let resp = reset_token_at(token_url, token).await?;
    if resp.response_code != 0 {
        anyhow::bail!("OpenTDB token reset failed (code {})", resp.response_code);
    }
    info!(token_fp = %token_fingerprint(token), "Reset OpenTDB session token (pool exhausted)");
    Ok(())
}

/// Clear the cached token, but only if it still matches `token` — a
/// concurrent caller may have already replaced it with a fresh one while
/// this now-stale request was in flight, and we must not discard that.
/// Used when OpenTDB reports the token invalid/expired (`response_code = 3`).
async fn clear_token_if_matches(ctx: &BotContext, token: &str) -> anyhow::Result<()> {
    let _guard = TOKEN_OP_LOCK.lock().await;
    let mut state = ctx.state.lock().await;
    if state.opentdb_token.as_deref() == Some(token) {
        state.opentdb_token = None;
        state.save(&ctx.state_path).await?;
        info!(
            token_fp = %token_fingerprint(token),
            "Cleared invalid/expired OpenTDB session token"
        );
    } else {
        debug!("Skipped clearing OpenTDB token — already replaced by a concurrent caller");
    }
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

        let token = match ensure_token(ctx, TOKEN_URL).await {
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
                clear_token_if_matches(ctx, &token).await?;
                token_refreshed = true;
            }
            3 => warn!("OpenTDB token still not found after refresh — retrying"),
            // Code 4: every question for the current query has been seen —
            // reset it. Reset keeps the same token value, so it stays
            // cached in state.json; nothing to persist here.
            4 if !token_refreshed => {
                warn!("OpenTDB token exhausted, resetting");
                reset_token(&token, TOKEN_URL).await?;
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
/// questions asked within the reuse cooldown (see
/// [`QUESTION_REUSE_COOLDOWN_DAYS`]).  Does not touch the shared cache.
async fn fetch_one(ctx: &BotContext, category: u32) -> anyhow::Result<FetchedQuestion> {
    const MAX_SKIP: usize = 5;
    let difficulty = ctx.config.trivia.difficulty.clone();

    let mut token_refreshed = false;
    // Best duplicate seen so far, kept only in case every attempt turns out
    // to be a within-cooldown repeat: we'd rather resurface the one that's
    // gone longest without being asked than whichever was drawn last.
    let mut best_duplicate: Option<(FetchedQuestion, Option<DateTime<Utc>>)> = None;

    for attempt in 0..=MAX_SKIP {
        // At most one token-reset per call; network errors get their own retry inside api_get_with_retry.
        let token = match ensure_token(ctx, TOKEN_URL).await {
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
                    let recently_asked = ctx
                        .db
                        .question_recently_asked(&fetched.question, QUESTION_REUSE_COOLDOWN_DAYS)
                        .await
                        .unwrap_or(false);
                    if !recently_asked {
                        return Ok(fetched);
                    }

                    let last_asked_at = ctx
                        .db
                        .question_last_asked_at(&fetched.question)
                        .await
                        .unwrap_or(None);
                    let is_better = match &best_duplicate {
                        None => true,
                        Some((_, best_at)) => last_asked_at < *best_at,
                    };
                    if is_better {
                        best_duplicate = Some((fetched, last_asked_at));
                    }

                    if attempt == MAX_SKIP {
                        // Every candidate this call drew was within the reuse
                        // cooldown — pool for this category is genuinely tight
                        // right now. Resurface whichever has gone longest
                        // without being asked rather than a fresher repeat.
                        let (chosen, chosen_at) =
                            best_duplicate.expect("set above on this iteration");
                        warn!(
                            "Reusing duplicate question for category {category} \
                             (last asked {chosen_at:?}) — pool within cooldown window"
                        );
                        return Ok(chosen);
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
                clear_token_if_matches(ctx, &token).await?;
                token_refreshed = true;
            }
            3 => anyhow::bail!("OpenTDB token not found even after refresh"),
            // Code 4: token exhausted — reset and retry once. Reset keeps
            // the same token value (it just clears the "already served"
            // memory), so — unlike code 3 — the cached token must NOT be
            // discarded here: doing so would throw away a token that was
            // just successfully reset and force a wasted extra round trip
            // to request a brand-new one on the next attempt.
            4 if !token_refreshed => {
                warn!("OpenTDB token exhausted for category {category}, resetting");
                reset_token(&token, TOKEN_URL).await?;
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

    // Spacing between requests is handled centrally by `throttle()`, so no
    // per-call sleep is needed here. What we do bound is total wall-clock
    // time: per-slot fallbacks (cache miss → alternate category → alternate
    // category) can each cost several throttled requests, and this runs
    // during the pre-quiz reminder window, so it must not run indefinitely.
    // Any slots left unfilled when the budget expires are simply omitted —
    // the caller shortens the round rather than fetching on demand later.
    const ROUND_PREFETCH_BUDGET: Duration = Duration::from_secs(180);
    let deadline = Instant::now() + ROUND_PREFETCH_BUDGET;

    let mut questions: Vec<FetchedQuestion> = Vec::with_capacity(n);
    let active_group_count = active_groups(&ctx.config.trivia.excluded_categories).len();
    let recent = ctx
        .db
        .recent_category_groups(ctx.config.trivia.recent_category_window)
        .await
        .unwrap_or_default();
    let mut avoid_groups: HashSet<String> = recent.iter().map(|name| normalise(name)).collect();

    for choice in categories {
        if Instant::now() >= deadline {
            warn!(
                gathered = questions.len(),
                requested = n,
                "Round prefetch budget ({:?}) exhausted — proceeding with questions gathered so far",
                ROUND_PREFETCH_BUDGET
            );
            break;
        }
        let previous_group = questions
            .last()
            .map(|q| normalise(&category_group_label(&q.category)));
        match fetch_one(ctx, choice.category_id).await {
            Ok(q)
                if category_group_for_category(&q.category)
                    .is_some_and(|group| normalise(group) == normalise(&choice.group))
                    && category_can_follow(
                        previous_group.as_deref(),
                        &q.category,
                        active_group_count,
                    ) =>
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
                    previous_group = ?previous_group,
                    "OpenTDB returned an unexpected or consecutive category; using diverse fallback"
                );
                match next_question_avoiding(ctx, &avoid_groups, previous_group.as_deref()).await {
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
                match next_question_avoiding(ctx, &avoid_groups, previous_group.as_deref()).await {
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

    if questions.len() < n {
        warn!(
            gathered = questions.len(),
            requested = n,
            "Round prefetch produced fewer questions than requested — round will be shortened"
        );
    } else {
        info!(gathered = questions.len(), "Round prefetch complete");
    }

    questions
}

async fn next_question_avoiding(
    ctx: &BotContext,
    preferred_avoid_groups: &HashSet<String>,
    previous_group: Option<&str>,
) -> anyhow::Result<FetchedQuestion> {
    let is_empty = ctx.state.lock().await.cached_questions.is_empty();
    if is_empty {
        prefetch(ctx).await?;
    }

    if let Some(q) = cached_question_excluding(ctx, preferred_avoid_groups).await {
        info!(category = %q.category, "Selected preferred diverse cache fallback");
        return Ok(q);
    }

    let strict_avoid: HashSet<String> = previous_group.iter().map(|group| normalise(group)).collect();
    if let Some(q) = cached_question_excluding(ctx, &strict_avoid).await {
        info!(
            category = %q.category,
            excluded_recent = ?preferred_avoid_groups,
            "Relaxed recent history while preserving consecutive-category exclusion"
        );
        return Ok(q);
    }

    let mut alternatives: Vec<_> = active_groups(&ctx.config.trivia.excluded_categories)
        .into_iter()
        .filter(|(name, _)| previous_group != Some(normalise(name).as_str()))
        .collect();
    alternatives.shuffle(&mut rand::thread_rng());
    alternatives.sort_by_key(|(name, _)| preferred_avoid_groups.contains(&normalise(name)));

    for (name, ids) in alternatives.iter().take(2) {
        let category_id = *ids
            .choose(&mut rand::thread_rng())
            .expect("active category group has IDs");
        match fetch_one(ctx, category_id).await {
            Ok(q)
                if category_group_for_category(&q.category)
                    .is_some_and(|group| normalise(group) == normalise(name))
                    && category_can_follow(previous_group, &q.category, alternatives.len() + 1) =>
            {
                info!(
                    category_group = %name,
                    category_id,
                    "Fetched alternate category after cache fallback miss"
                );
                return Ok(q);
            }
            Ok(q) => warn!(
                returned_category = %q.category,
                previous_group = ?previous_group,
                "Alternate category fetch still returned the previous group"
            ),
            Err(e) => warn!("Alternate category fetch failed for {name} ({category_id}): {e}"),
        }
    }

    if !alternatives.is_empty() {
        anyhow::bail!(
            "no usable fallback outside previous category {:?}; refusing a consecutive category",
            previous_group
        );
    }

    warn!("Only one active category group; consecutive category is unavoidable");
    next_question(ctx).await
}

async fn cached_question_excluding(
    ctx: &BotContext,
    excluded_groups: &HashSet<String>,
) -> Option<FetchedQuestion> {
    let cache_len = ctx.state.lock().await.cached_questions.len();
    for _ in 0..cache_len {
        let q = ctx.state.lock().await.cached_questions.pop_front()?;
        let group = normalise(&category_group_label(&q.category));
        if !question_is_valid(&q) {
            warn!("Discarding malformed question already present in cache");
            continue;
        }
        if excluded_groups.contains(&group)
            || !category_is_active(&q.category, &ctx.config.trivia.excluded_categories)
        {
            ctx.state.lock().await.cached_questions.push_back(q);
            continue;
        }
        let recently_asked = ctx
            .db
            .question_recently_asked(&q.question, QUESTION_REUSE_COOLDOWN_DAYS)
            .await
            .unwrap_or(false);
        if !recently_asked {
            info!(category = %q.category, "Selected category-aware cache fallback");
            return Some(q);
        }
    }
    None
}

/// Pop the next question from the cache, skipping any already asked in a
/// previous round.  Fetches a fresh batch if the cache runs empty.
///
/// After MAX_SKIP consecutive duplicates we give up deduplication and return
/// the next available question — this prevents an infinite loop when the entire
/// OpenTDB pool has been exhausted.
pub async fn next_question(ctx: &BotContext) -> anyhow::Result<FetchedQuestion> {
    const MAX_SKIP: usize = 30;
    // Best duplicate seen so far, used only if every candidate this call
    // pops turns out to be within the reuse cooldown — see `fetch_one`.
    let mut best_duplicate: Option<(FetchedQuestion, Option<DateTime<Utc>>)> = None;

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

        // Check whether this question was asked within the reuse cooldown.
        let recently_asked = ctx
            .db
            .question_recently_asked(&q.question, QUESTION_REUSE_COOLDOWN_DAYS)
            .await
            .unwrap_or(false);
        if !recently_asked {
            return Ok(q);
        }

        let last_asked_at = ctx.db.question_last_asked_at(&q.question).await.unwrap_or(None);
        let is_better = match &best_duplicate {
            None => true,
            Some((_, best_at)) => last_asked_at < *best_at,
        };
        if is_better {
            best_duplicate = Some((q, last_asked_at));
        }

        if attempt == MAX_SKIP {
            // Entire reachable pool is within the cooldown window — reuse
            // rather than hang, but prefer whichever duplicate has gone
            // longest without being asked.
            let (chosen, chosen_at) = best_duplicate.expect("set above on this iteration");
            warn!(
                "Skipped {MAX_SKIP} duplicate questions — pool within cooldown window, \
                 reusing the least-recently-asked one (last asked {chosen_at:?})."
            );
            return Ok(chosen);
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
    fn remaining_wait_is_zero_once_the_interval_has_elapsed() {
        let t0 = Instant::now();
        let after_interval = t0 + MIN_REQUEST_INTERVAL;
        assert_eq!(remaining_wait(t0, after_interval, MIN_REQUEST_INTERVAL), Duration::ZERO);
        let well_after = t0 + MIN_REQUEST_INTERVAL + Duration::from_secs(60);
        assert_eq!(remaining_wait(t0, well_after, MIN_REQUEST_INTERVAL), Duration::ZERO);
    }

    #[test]
    fn remaining_wait_covers_exactly_the_gap_to_the_interval() {
        let t0 = Instant::now();
        let one_second_in = t0 + Duration::from_secs(1);
        assert_eq!(
            remaining_wait(t0, one_second_in, Duration::from_secs(5)),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn remaining_wait_at_the_same_instant_is_the_full_interval() {
        let now = Instant::now();
        assert_eq!(remaining_wait(now, now, MIN_REQUEST_INTERVAL), MIN_REQUEST_INTERVAL);
    }

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

    #[test]
    fn consecutive_normalized_categories_are_rejected_when_alternatives_exist() {
        assert!(!category_can_follow(Some("geography"), " Geography ", 10));
        assert!(category_can_follow(Some("geography"), "History", 10));
        assert!(category_can_follow(Some("geography"), "Geography", 1));
    }
}

#[cfg(test)]
mod token_tests {
    //! Exercises the real `ensure_token` / `reset_token` /
    //! `clear_token_if_matches` functions — not a reimplementation —
    //! against a local mock OpenTDB server (`wiremock`) and a real,
    //! network-free `BotContext` (matrix-sdk's own `MockClientBuilder`,
    //! gated behind its `testing` feature).

    use std::{collections::HashSet, path::Path, sync::Arc};

    use matrix_sdk::ruma::RoomId;
    use tokio::sync::Mutex as TokioMutex;
    use wiremock::{
        matchers::{method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::{config::Config, state::State, BotContext};

    const TEST_CONFIG_TOML: &str = r#"
        [matrix]
        homeserver   = "http://localhost"
        user_id      = "@bot:example.org"
        access_token = "test-token"
        device_id    = "TESTDEV"

        [schedule]
        room_id    = "!room:example.org"
        quiz_times = ["12:00"]
    "#;

    /// A real `BotContext` — real in-memory DB, real (network-free) matrix
    /// Client, real `State` backed by `state_path` — so `ensure_token(ctx)`
    /// runs exactly as it does in production.
    async fn test_ctx(state_path: std::path::PathBuf) -> BotContext {
        let client = matrix_sdk::test_utils::client::MockClientBuilder::new(None)
            .unlogged()
            .build()
            .await;
        let db = Arc::new(crate::db::Db::open(Path::new(":memory:")).await.unwrap());
        db.migrate().await.unwrap();
        let config: Config = toml::from_str(TEST_CONFIG_TOML).unwrap();

        BotContext {
            state: Arc::new(TokioMutex::new(State::default())),
            state_path,
            config: Arc::new(config),
            admin_users: HashSet::new(),
            room_id: <&RoomId>::try_from("!room:example.org").unwrap().to_owned(),
            active_quiz: Arc::new(TokioMutex::new(None)),
            quiz_run_lock: Arc::new(TokioMutex::new(())),
            client,
            db,
        }
    }

    fn temp_state_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "quiz-bot-token-test-{name}-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn token_response_body(token: &str) -> serde_json::Value {
        serde_json::json!({ "response_code": 0, "token": token })
    }

    // ── normal use ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_token_requests_once_and_reuses_the_cached_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response_body("tok-abc")))
            .expect(1) // must be called exactly once, not once per question
            .mount(&server)
            .await;

        let ctx = test_ctx(temp_state_path("normal-use")).await;
        let base = format!("{}/api_token.php", server.uri());

        let first = ensure_token(&ctx, &base).await.unwrap();
        let second = ensure_token(&ctx, &base).await.unwrap();

        assert_eq!(first, "tok-abc");
        assert_eq!(second, "tok-abc");
        // wiremock verifies `.expect(1)` on drop — a second HTTP call would
        // fail this test.
    }

    // ── restart / recovery ──────────────────────────────────────────────

    #[tokio::test]
    async fn token_survives_a_simulated_restart_without_a_second_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response_body("tok-persisted")))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/api_token.php", server.uri());
        let state_path = temp_state_path("restart");

        {
            let ctx = test_ctx(state_path.clone()).await;
            let token = ensure_token(&ctx, &base).await.unwrap();
            assert_eq!(token, "tok-persisted");
        } // ctx (and its in-memory state) dropped — simulates process exit.

        {
            // Fresh BotContext loading state from the same file, as happens
            // on a real restart.
            let mut ctx = test_ctx(state_path.clone()).await;
            ctx.state = Arc::new(TokioMutex::new(State::load(&state_path).await.unwrap()));
            let token = ensure_token(&ctx, &base).await.unwrap();
            assert_eq!(token, "tok-persisted");
        }

        let _ = std::fs::remove_file(&state_path);
    }

    // ── exhausted token (response_code = 4) ─────────────────────────────

    #[tokio::test]
    async fn reset_keeps_the_same_token_cached_no_second_token_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response_body("tok-xyz")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "reset"))
            .and(query_param("token", "tok-xyz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response_code": 0, "token": serde_json::Value::Null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ctx = test_ctx(temp_state_path("exhausted")).await;
        let base = format!("{}/api_token.php", server.uri());

        let token = ensure_token(&ctx, &base).await.unwrap();
        assert_eq!(token, "tok-xyz");

        // The real `reset_token` used by fetch_one/prefetch's code-4
        // handler — it takes no `ctx`/state at all, so by construction it
        // cannot clear the cached token (the bug that was fixed: the old
        // code did exactly that right after a successful reset, discarding
        // a token that was still valid and forcing a wasted second
        // "request new token" call).
        reset_token(&token, &base).await.unwrap();

        let token_after_reset = ctx.state.lock().await.opentdb_token.clone();
        assert_eq!(
            token_after_reset.as_deref(),
            Some("tok-xyz"),
            "resetting a token must not discard it from the cache"
        );

        // Confirms no second `command=request` call happened: `.expect(1)`
        // on the mock above is checked when `server` drops at end of test.
    }

    // ── invalid / expired token (response_code = 3) ─────────────────────

    #[tokio::test]
    async fn clear_token_if_matches_removes_a_stale_token() {
        let ctx = test_ctx(temp_state_path("invalid")).await;
        ctx.state.lock().await.opentdb_token = Some("stale-token".to_owned());

        clear_token_if_matches(&ctx, "stale-token").await.unwrap();

        assert_eq!(ctx.state.lock().await.opentdb_token, None);
    }

    #[tokio::test]
    async fn clear_token_if_matches_preserves_a_token_replaced_concurrently() {
        // Simulates: request A reads "old-token", gets a stale code-3 for it;
        // meanwhile request B already replaced the cache with "new-token".
        // A's cleanup must not clobber B's fresh token.
        let ctx = test_ctx(temp_state_path("invalid-race")).await;
        ctx.state.lock().await.opentdb_token = Some("new-token".to_owned());

        clear_token_if_matches(&ctx, "old-token").await.unwrap();

        assert_eq!(
            ctx.state.lock().await.opentdb_token.as_deref(),
            Some("new-token")
        );
    }

    #[tokio::test]
    async fn expired_token_is_re_requested_after_being_cleared() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response_body("tok-1")))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response_body("tok-2")))
            .mount(&server)
            .await;

        let ctx = test_ctx(temp_state_path("expired")).await;
        let base = format!("{}/api_token.php", server.uri());

        let first = ensure_token(&ctx, &base).await.unwrap();
        assert_eq!(first, "tok-1");

        // OpenTDB reports "Token Not Found" (expired after 6h idle).
        clear_token_if_matches(&ctx, &first).await.unwrap();

        let second = ensure_token(&ctx, &base).await.unwrap();
        assert_eq!(second, "tok-2");
    }

    // ── API errors ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn token_request_at_parses_a_non_zero_response_code_without_erroring() {
        // `request_token_at` is a pure fetch-and-parse — it hands the
        // response code back for the caller to interpret, rather than
        // deciding for itself what's an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response_code": 2, "token": serde_json::Value::Null
            })))
            .mount(&server)
            .await;

        let base = format!("{}/api_token.php", server.uri());
        let resp = request_token_at(&base).await.unwrap();
        assert_eq!(resp.response_code, 2);
    }

    #[tokio::test]
    async fn ensure_token_surfaces_a_non_zero_response_code_as_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response_code": 2, "token": serde_json::Value::Null
            })))
            .mount(&server)
            .await;

        let ctx = test_ctx(temp_state_path("bad-code")).await;
        let base = format!("{}/api_token.php", server.uri());

        let err = ensure_token(&ctx, &base).await.unwrap_err();
        assert!(format!("{err:#}").contains("code 2"), "error was: {err:#}");
        // A failed token request must not poison the cache — a later
        // retry should still be possible.
        assert_eq!(ctx.state.lock().await.opentdb_token, None);
    }

    #[tokio::test(start_paused = true)]
    async fn api_get_with_retry_recovers_from_transient_failures() {
        let server = MockServer::start().await;
        // First two requests: HTTP 500. Third: a valid, empty result set.
        Mock::given(method("GET"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response_code": 0, "results": []
            })))
            .mount(&server)
            .await;

        let url = format!("{}/api.php", server.uri());
        // start_paused lets the exponential backoff between retries
        // (1s, 2s, ...) elapse instantly instead of costing real wall time.
        let resp = api_get_with_retry(&url).await.unwrap();
        assert_eq!(resp.response_code, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn api_get_with_retry_gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let url = format!("{}/api.php", server.uri());
        let result = api_get_with_retry(&url).await;
        assert!(result.is_err(), "should give up rather than retry forever");
    }

    // ── concurrent requests ──────────────────────────────────────────────

    #[tokio::test]
    async fn concurrent_ensure_token_calls_request_the_token_exactly_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api_token.php"))
            .and(query_param("command", "request"))
            // A small delay widens the window for two callers to race past
            // the first (unlocked) cache check before either has written
            // the result back — the scenario `TOKEN_OP_LOCK` must prevent
            // from producing two independent token requests.
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(token_response_body("tok-concurrent"))
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let ctx = Arc::new(test_ctx(temp_state_path("concurrent")).await);
        let base = format!("{}/api_token.php", server.uri());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let ctx = Arc::clone(&ctx);
            let base = base.clone();
            handles.push(tokio::spawn(
                async move { ensure_token(&ctx, &base).await.unwrap() },
            ));
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        assert!(
            results.iter().all(|t| t == "tok-concurrent"),
            "every concurrent caller must observe the single winning token"
        );
        // `.expect(1)` on the mock (checked on drop) proves only one HTTP
        // request actually went out despite 8 concurrent callers.
    }

    #[tokio::test]
    async fn acquire_token_single_flights_concurrent_callers() {
        // Same property as above, but against the decoupled, network-free
        // `acquire_token` primitive directly.
        let cached = Arc::new(Mutex::new(None));
        let op_lock = Arc::new(Mutex::new(()));
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let cached = Arc::clone(&cached);
            let op_lock = Arc::clone(&op_lock);
            let call_count = Arc::clone(&call_count);
            handles.push(tokio::spawn(async move {
                acquire_token(&cached, &op_lock, || {
                    let call_count = Arc::clone(&call_count);
                    async move {
                        call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok("only-token".to_owned())
                    }
                })
                .await
                .unwrap()
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(results.iter().all(|t| t == "only-token"));
    }
}
