//! TriviaQA as a second question source, alongside OpenTDB (`fetcher.rs`).
//!
//! TriviaQA supplies the question and the single verified correct answer,
//! but no wrong answers, category or difficulty. The LLM (Groq, the same
//! integration `explainer.rs` uses — see `crate::groq`) prepares those, in
//! batches of `BATCH_SIZE` questions per call:
//! - **suitable?** — time-bound, ambiguous, garbled or possibly outdated
//!   questions are rejected and never asked;
//! - **category** — one of the bot's own groups (`fetcher::CATEGORY_GROUPS`),
//!   with a description of what each covers (`GROUP_DESCRIPTIONS`);
//! - **difficulty** — easy/medium/hard by a fixed rubric;
//! - **3 wrong answers** of the same kind and form as the correct one.
//!
//! The LLM never changes the question or correct answer; its reply is parsed
//! exclusively into those four fields (`validate_item`). Display-only cleanup
//! is deterministic: CSV-doubled quotes and all-caps answers
//! (`clean_question`, `display_answer`).
//!
//! ## How it fits the existing architecture
//! - Results are cached on the `triviaqa_pool` row together with the
//!   `GENERATION_VERSION` that produced them; bumping it re-classifies older
//!   rows. `keep_stocked` keeps `READY_TARGET` questions classified in the
//!   background, so a round never waits for the LLM.
//! - A prepared question is an ordinary `state::FetchedQuestion` with a real
//!   category group and difficulty, so quiz.rs, format.rs and the DB need no
//!   source-specific handling, and category exclusion / round diversity /
//!   difficulty filters apply unchanged — a round slot asks for its planned
//!   group (`next_question`'s `wanted_group`).
//! - Dedup/history is the *shared* `questions` table.
//! - Which source is used is an admin runtime setting
//!   (`QuizSettings::question_source`: mixed / opentdb / triviaqa), or per
//!   round via `!startquiz triviaqa`. Any slot TriviaQA can't fill falls back
//!   to OpenTDB, so TriviaQA is additive, never a hard dependency.

use std::collections::HashSet;
use std::io::Read;
use std::sync::LazyLock;

use anyhow::Context;
use rand::Rng;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    config::QuestionSource,
    db::{Db, TriviaQaImportRow, TriviaQaRow},
    fetcher::{self, normalise, QUESTION_REUSE_COOLDOWN_DAYS},
    groq,
    state::FetchedQuestion,
    BotContext,
};

/// Bump whenever the prompt or its rules change: rows classified by an
/// older version are re-classified (in the background) before being served.
pub const GENERATION_VERSION: i64 = 2;
/// Questions per classification call — one prompt handles a whole batch.
pub const BATCH_SIZE: usize = 10;
/// Ready (classified, suitable) questions the background task keeps in stock.
const READY_TARGET: i64 = 300;
/// Candidates fetched per lookup, to skip recently asked ones.
const READY_CANDIDATES: usize = 20;

/// What each category group covers — the LLM only sees the names otherwise
/// and files e.g. a novel's setting under Geography. Must list every entry
/// of `fetcher::CATEGORY_GROUPS` (checked by a test).
const GROUP_DESCRIPTIONS: &[(&str, &str)] = &[
    ("General Knowledge", "everyday facts, words and language, food and drink, customs, mixed trivia that fits no other group"),
    ("Entertainment", "film, TV, music, books and literature (incl. authors, characters, settings), theatre, video and board games, comics, anime"),
    ("Science & Technology", "physics, chemistry, biology, medicine, the human body, space, maths, computing, inventions, gadgets"),
    ("Mythology", "myths, legends, gods and heroes, folklore, religious stories"),
    ("Sports", "sports, athletes, teams, competitions, rules of games, the Olympics"),
    ("Geography", "countries, capitals, cities, rivers, mountains, flags, where places or landmarks are"),
    ("History", "past events, wars, rulers, historical figures, eras, discoveries"),
    ("Politics", "governments, politicians, elections, political systems, international organisations"),
    ("Art", "painting, sculpture, architecture, artists, art movements, design"),
    ("Celebrities", "famous people's lives, nicknames, relationships and scandals (not their works)"),
    ("Animals", "animals, pets, breeds, zoology"),
    ("Vehicles", "cars, trains, ships, aircraft, their makers, models and logos"),
];

/// Built at call time so the category list stays in sync with
/// `fetcher::CATEGORY_GROUPS` (the bot's single source of truth).
fn generation_system_prompt() -> String {
    let categories: Vec<String> = fetcher::CATEGORY_GROUPS
        .iter()
        .map(|(name, _)| {
            let description = GROUP_DESCRIPTIONS
                .iter()
                .find(|(n, _)| n == name)
                .map_or("", |(_, d)| d);
            format!("- \"{name}\": {description}")
        })
        .collect();
    format!(
        "You prepare trivia questions for a multiple-choice quiz played by adults in a \
         Matrix chat. For each ITEM (id, question, correct answer) decide:

1. suitable — false if the question should not be asked: its answer is time-bound \
or may be outdated (\"currently\", \"this year\", future tense, records, holders), \
the question is ambiguous, incomplete or garbled, the answer looks wrong, it needs an \
image or audio, or the question gives the answer away. Give a short reason.
2. category — exactly one of these names, by what the question is about:
{}
3. difficulty — for a quiz-interested adult who sees 4 options:
   easy: most people know it; medium: needs some general knowledge; hard: specialist \
   or obscure knowledge.
4. wrong_answers — 3 answers that are clearly wrong but plausible to someone unsure: \
same kind of thing as the correct answer (person/place/year/number/title), similar \
length and formatting, clearly different from each other, never the correct answer, \
a synonym or a partial form of it.

Never change the question or the correct answer.
Reply with one JSON object only:
{{\"items\": [{{\"id\": <id>, \"suitable\": true, \"reason\": \"\", \"category\": \"<name>\", \
\"difficulty\": \"easy|medium|hard\", \"wrong_answers\": [\"…\", \"…\", \"…\"]}}]}}",
        categories.join("\n")
    )
}

// ── Dataset schema (matches the official TriviaQA JSON files) ─────────────────

#[derive(Deserialize)]
struct Dataset {
    #[serde(rename = "Data")]
    data: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "Question")]
    question: String,
    #[serde(rename = "Answer")]
    answer: Answer,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "Value")]
    value: String,
    #[serde(rename = "Aliases", default)]
    aliases: Vec<String>,
}

// ── Dataset download + parsing ─────────────────────────────────────────────────

/// Filenames the official TriviaQA release uses, in preference order, when
/// `dataset_url` points straight at a `.tar.gz` archive (the actual shape
/// the official distribution ships in — see
/// https://nlp.cs.washington.edu/triviaqa/). "*-dev.json" splits comfortably
/// exceed any reasonable `max_pool_size` while staying far smaller than the
/// multi-GB "*-train.json" splits; "*-without-answers*" files have no
/// correct answer to grade against and are never usable here.
const PREFERRED_ARCHIVE_ENTRIES: &[&str] = &[
    "unfiltered-web-dev.json",
    "wikipedia-dev.json",
    "verified-wikipedia-dev.json",
    "unfiltered-web-train.json",
    "wikipedia-train.json",
];

fn gunzip_if_needed(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .context("gzip decompression failed")?;
    Ok(out)
}

/// Extract the raw JSON bytes to parse out of a downloaded dataset payload,
/// which may be:
/// - Plain JSON, as-is.
/// - Gzip-compressed JSON (magic `1f 8b`), transparently decompressed.
/// - A (gzipped) tar archive bundling several TriviaQA JSON files — the
///   official distribution's actual format — in which case the first
///   `PREFERRED_ARCHIVE_ENTRIES` match present is used, falling back to the
///   first `*.json` entry that isn't a "-without-answers" file. Reads the
///   archive twice (list entry names cheaply, then fetch only the chosen
///   one's content) so an unwanted multi-GB split in the same archive is
///   never actually read into memory.
fn extract_dataset_json(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let decompressed = gunzip_if_needed(bytes)?;

    if decompressed.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{') {
        return Ok(decompressed);
    }

    let list_json_entries = || -> anyhow::Result<Vec<String>> {
        let mut archive = tar::Archive::new(std::io::Cursor::new(&decompressed));
        let mut paths = Vec::new();
        for entry in archive
            .entries()
            .context("payload is neither recognizable JSON nor a tar.gz dataset")?
        {
            let path = entry
                .context("reading tar entry")?
                .path()?
                .to_string_lossy()
                .into_owned();
            if path.ends_with(".json") {
                paths.push(path);
            }
        }
        Ok(paths)
    };
    let json_paths = list_json_entries()?;

    let chosen = PREFERRED_ARCHIVE_ENTRIES
        .iter()
        .find_map(|preferred| json_paths.iter().find(|p| p.ends_with(*preferred)))
        .or_else(|| json_paths.iter().find(|p| !p.contains("without-answers")))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("archive contained no usable TriviaQA JSON file"))?;

    let mut archive = tar::Archive::new(std::io::Cursor::new(&decompressed));
    for entry in archive.entries().context("re-reading tar archive")? {
        let mut entry = entry.context("reading tar entry")?;
        if entry.path()?.to_string_lossy() == chosen {
            let mut content = Vec::new();
            entry
                .read_to_end(&mut content)
                .context("reading chosen tar entry")?;
            return Ok(content);
        }
    }
    anyhow::bail!("tar entry {chosen:?} vanished between listing and reading it");
}

/// Parse a TriviaQA dataset payload (plain JSON, gzipped JSON, or a
/// `.tar.gz` bundling several JSON files — see `extract_dataset_json`).
/// Filters out entries with an empty question or answer, and caps the
/// result at `max_entries` — the full unfiltered dataset has hundreds of
/// thousands of rows, far more than any single bot needs.
fn parse_dataset(bytes: &[u8], max_entries: usize) -> anyhow::Result<Vec<TriviaQaImportRow>> {
    let json_bytes = extract_dataset_json(bytes)?;

    let dataset: Dataset = serde_json::from_slice(&json_bytes)
        .map_err(|e| anyhow::anyhow!("TriviaQA JSON did not match the expected schema: {e}"))?;

    let rows = dataset
        .data
        .into_iter()
        .filter(|e| !e.question.trim().is_empty() && !e.answer.value.trim().is_empty())
        .take(max_entries)
        .map(|e| TriviaQaImportRow {
            question_text: e.question.trim().to_owned(),
            correct_answer: e.answer.value.trim().to_owned(),
            aliases: e
                .answer
                .aliases
                .into_iter()
                .map(|a| a.trim().to_owned())
                .filter(|a| !a.is_empty())
                .collect(),
        })
        .collect();

    Ok(rows)
}

/// Download `url` with a small bounded retry — this only ever runs once
/// (see `ensure_ingested`), so it's not worth the full throttled-retry
/// machinery `fetcher.rs` uses for the live, rate-limited OpenTDB API.
async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = anyhow::anyhow!("no attempts made");

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let delay = 5u64.pow(attempt);
            warn!("TriviaQA: download retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }
        match reqwest::get(url).await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last_err = e.into(),
            },
            Ok(resp) => last_err = anyhow::anyhow!("HTTP {}", resp.status()),
            Err(e) => last_err = e.into(),
        }
    }
    Err(last_err.context(format!(
        "TriviaQA dataset download failed after {MAX_ATTEMPTS} attempts"
    )))
}

// ── One-time ingestion ──────────────────────────────────────────────────────────

static INGEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn ingest_kv_key(dataset_url: &str, max_pool_size: usize) -> String {
    // Encodes the settings that would make a previous ingest stale, so
    // changing either in config triggers a fresh download+ingest instead
    // of silently keeping the old pool.
    format!("triviaqa_ingested:{max_pool_size}:{dataset_url}")
}

/// Download and ingest the TriviaQA dataset into `triviaqa_pool`, but only
/// if it hasn't already been done for the current `dataset_url` /
/// `max_pool_size` — this is what keeps the (potentially large) dataset
/// from being re-downloaded on every startup. Safe to call repeatedly
/// (e.g. once at startup and again lazily before a round) and safe to call
/// concurrently — `INGEST_LOCK` plus a post-lock re-check ensures at most
/// one download/ingest ever runs.
///
/// Logs and returns `Ok(())` on any failure (disabled, misconfigured,
/// download/parse error) rather than propagating — TriviaQA is optional,
/// so an ingestion problem must not affect bot startup or quiz rounds;
/// `should_offer`/`next_question` simply see an empty pool and the caller
/// falls back to OpenTDB.
pub async fn ensure_ingested(ctx: BotContext) {
    if let Err(e) = ensure_ingested_inner(&ctx).await {
        warn!("TriviaQA: ingestion failed: {e:#}");
    }
}

async fn ensure_ingested_inner(ctx: &BotContext) -> anyhow::Result<()> {
    let cfg = &ctx.config.trivia.triviaqa;
    if ctx.settings.get().question_source == QuestionSource::Opentdb {
        return Ok(());
    }
    let Some(dataset_url) = cfg.dataset_url.as_deref() else {
        warn!("TriviaQA: enabled but trivia.triviaqa.dataset_url is not set — staying disabled");
        return Ok(());
    };

    let key = ingest_kv_key(dataset_url, cfg.max_pool_size);
    if already_ingested(&ctx.db, &key).await? {
        return Ok(());
    }

    let _guard = INGEST_LOCK.lock().await;
    // Re-check now that we hold the lock — a concurrent caller (e.g. the
    // startup task and a lazily-triggered one) may have just finished.
    if already_ingested(&ctx.db, &key).await? {
        return Ok(());
    }

    info!(
        "TriviaQA: ingesting dataset from {dataset_url} (max {} entries)",
        cfg.max_pool_size
    );
    let bytes = download(dataset_url).await?;
    let rows = parse_dataset(&bytes, cfg.max_pool_size)?;
    if rows.is_empty() {
        anyhow::bail!("dataset at {dataset_url} contained no usable Q/A pairs");
    }
    let inserted = ctx.db.insert_triviaqa_entries(rows).await?;
    ctx.db.kv_set(&key, "done").await?;
    info!(
        "TriviaQA: ingested {inserted} new question(s); pool now has {} total",
        ctx.db.triviaqa_pool_count().await.unwrap_or(-1)
    );
    Ok(())
}

async fn already_ingested(db: &Db, key: &str) -> anyhow::Result<bool> {
    Ok(db.kv_get(key).await?.is_some() && db.triviaqa_pool_count().await? > 0)
}

// ── Text cleanup ──────────────────────────────────────────────────────────────

/// Tidy a dataset question for display: CSV-style doubled quotes, a pair of
/// quotes around the whole question, and a missing question mark.
pub fn clean_question(raw: &str) -> String {
    let mut q = raw.trim().replace("\"\"", "\"");
    if q.len() > 2 && q.starts_with('"') && q.ends_with('"') {
        q = q[1..q.len() - 1].trim().to_owned();
    }
    const QUESTION_WORDS: &[&str] = &[
        "what", "which", "who", "whose", "whom", "where", "when", "why", "how", "in", "on", "at",
        "of", "for", "from", "by", "to", "is", "are", "was", "were", "did", "does", "do", "can",
        "name",
    ];
    let first = q
        .split_whitespace()
        .next()
        .map(|w| w.to_lowercase())
        .unwrap_or_default();
    let ends_bare = q.chars().last().is_some_and(|c| c.is_alphanumeric());
    if ends_bare && QUESTION_WORDS.contains(&first.as_str()) && first != "name" {
        q.push('?');
    }
    q
}

/// Answers the dataset stores in all caps (`SCOTTISH TERRIER`) stand out
/// next to normally written wrong answers and give themselves away — write
/// them in title case. Short all-caps words (≤ 4 letters: `USA`, `NATO`,
/// `AC/DC`) are left alone as likely acronyms.
pub fn display_answer(raw: &str) -> String {
    let answer = raw.trim();
    let letters: Vec<char> = answer.chars().filter(|c| c.is_alphabetic()).collect();
    let all_caps = !letters.is_empty() && letters.iter().all(|c| c.is_uppercase());
    if !all_caps || letters.len() <= 4 {
        return answer.to_owned();
    }
    answer
        .split(' ')
        .enumerate()
        .map(|(i, word)| {
            let lower = word.to_lowercase();
            if i > 0
                && matches!(
                    lower.as_str(),
                    "of" | "the" | "and" | "in" | "on" | "de" | "la"
                )
            {
                return lower;
            }
            let mut chars = lower.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Classification + distractor generation/validation ─────────────────────────

/// Match free-form LLM output against the bot's actual category groups
/// (`fetcher::CATEGORY_GROUPS`) — the single source of truth — so a match is
/// guaranteed to plug straight into the bot's category filtering/diversity.
fn canonicalize_category_group(raw: &str) -> Option<&'static str> {
    fetcher::CATEGORY_GROUPS
        .iter()
        .find(|(name, _)| normalise(name) == normalise(raw))
        .map(|(name, _)| *name)
}

fn canonicalize_difficulty(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "easy" => Some("easy"),
        "medium" => Some("medium"),
        "hard" => Some("hard"),
        _ => None,
    }
}

/// Keep up to 3 usable distractors — cleans numbering/quotes, and drops
/// anything blank, too long, a duplicate, or matching the correct
/// answer/an alias.
fn parse_distractor_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    correct_answer: &str,
    aliases: &[String],
) -> Vec<String> {
    let forbidden: HashSet<String> = std::iter::once(correct_answer)
        .chain(aliases.iter().map(String::as_str))
        .map(normalise)
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut distractors = Vec::new();

    for line in lines {
        let cleaned = line
            .trim()
            .trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ' ')
            })
            .trim_matches(|c: char| c == '"' || c == '\'')
            .trim();

        if cleaned.is_empty() || cleaned.len() > 200 {
            continue;
        }
        let key = normalise(cleaned);
        if forbidden.contains(&key) || !seen.insert(key) {
            continue;
        }
        distractors.push(cleaned.to_owned());
        if distractors.len() == 3 {
            break;
        }
    }

    distractors
}

#[derive(Deserialize)]
struct BatchReply {
    items: Vec<ItemReply>,
}

#[derive(Deserialize)]
struct ItemReply {
    id: i64,
    #[serde(default = "yes")]
    suitable: bool,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    difficulty: String,
    #[serde(default)]
    wrong_answers: Vec<String>,
}

fn yes() -> bool {
    true
}

/// What the LLM decided about one question.
#[derive(Debug, PartialEq)]
enum Verdict {
    Ready {
        category_group: &'static str,
        difficulty: &'static str,
        distractors: Vec<String>,
    },
    Unsuitable(String),
    /// Unusable reply for this item — try again in a later batch.
    Invalid,
}

/// Validate one item of a batch reply against its pool row. Pure and
/// network-free.
fn validate_item(item: &ItemReply, row: &TriviaQaRow) -> Verdict {
    if !item.suitable {
        let reason = item.reason.trim();
        return Verdict::Unsuitable(if reason.is_empty() {
            "unsuitable".to_owned()
        } else {
            reason.chars().take(200).collect()
        });
    }
    let (Some(category_group), Some(difficulty)) = (
        canonicalize_category_group(&item.category),
        canonicalize_difficulty(&item.difficulty),
    ) else {
        return Verdict::Invalid;
    };
    let answer = display_answer(&row.correct_answer);
    let distractors = parse_distractor_lines(
        item.wrong_answers.iter().map(String::as_str),
        &answer,
        &row.aliases,
    );
    if distractors.len() < 3 {
        return Verdict::Invalid;
    }
    Verdict::Ready {
        category_group,
        difficulty,
        distractors,
    }
}

/// Parse a batch reply (a JSON object, possibly wrapped in prose or a code
/// fence) and validate every item that belongs to `rows`.
fn parse_batch(raw: &str, rows: &[TriviaQaRow]) -> Vec<(i64, Verdict)> {
    let json = match (raw.find('{'), raw.rfind('}')) {
        (Some(start), Some(end)) if end > start => &raw[start..=end],
        _ => return Vec::new(),
    };
    let Ok(reply) = serde_json::from_str::<BatchReply>(json) else {
        return Vec::new();
    };
    reply
        .items
        .iter()
        .filter_map(|item| {
            let row = rows.iter().find(|r| r.id == item.id)?;
            Some((item.id, validate_item(item, row)))
        })
        .collect()
}

/// Outcome of one `classify_batch` call.
#[derive(Debug, Default, Clone, Copy)]
pub struct BatchOutcome {
    pub ready: usize,
    pub rejected: usize,
    pub failed: usize,
}

/// Classify up to `BATCH_SIZE` pool rows that aren't ready under the current
/// `GENERATION_VERSION` in a single LLM call, and store the results:
/// category, difficulty and wrong answers — or a rejection for unsuitable
/// questions. `None` if the LLM is unavailable (no key, bad model, …).
pub async fn classify_batch(ctx: &BotContext) -> Option<BatchOutcome> {
    let api_key = ctx.config.explainer.api_key.as_deref()?;
    let rows = match ctx
        .db
        .triviaqa_unclassified(GENERATION_VERSION, BATCH_SIZE)
        .await
    {
        Ok(rows) if !rows.is_empty() => rows,
        Ok(_) => return Some(BatchOutcome::default()),
        Err(e) => {
            warn!("TriviaQA: reading unclassified rows failed: {e:#}");
            return None;
        }
    };
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "question": clean_question(&r.question_text),
                "correct_answer": display_answer(&r.correct_answer),
            })
        })
        .collect();
    let user_content = serde_json::json!({ "items": items }).to_string();

    let raw = groq::complete(
        api_key,
        &ctx.config.explainer.model,
        &generation_system_prompt(),
        &user_content,
        groq::Options {
            max_tokens: 6000,
            json: true,
        },
    )
    .await?;

    let mut outcome = BatchOutcome::default();
    let verdicts = parse_batch(&raw, &rows);
    for row in &rows {
        match verdicts
            .iter()
            .find(|(id, _)| *id == row.id)
            .map(|(_, v)| v)
        {
            Some(Verdict::Ready {
                category_group,
                difficulty,
                distractors,
            }) => {
                if let Err(e) = ctx
                    .db
                    .save_triviaqa_generation(
                        row.id,
                        GENERATION_VERSION,
                        category_group,
                        difficulty,
                        distractors,
                    )
                    .await
                {
                    warn!("TriviaQA: saving classification failed: {e:#}");
                }
                outcome.ready += 1;
            }
            Some(Verdict::Unsuitable(reason)) => {
                info!(question = %row.question_text, %reason, "TriviaQA: rejected as unsuitable");
                ctx.db
                    .reject_triviaqa(row.id, GENERATION_VERSION, reason)
                    .await
                    .ok();
                outcome.rejected += 1;
            }
            Some(Verdict::Invalid) | None => outcome.failed += 1,
        }
    }
    info!(?outcome, "TriviaQA: classified a batch");
    Some(outcome)
}

/// Background task: keep `READY_TARGET` classified questions in stock, so a
/// round never waits for the LLM. Idles while the source is `opentdb`.
pub async fn keep_stocked(ctx: BotContext) {
    loop {
        let wait = match stock_step(&ctx).await {
            Some(true) => 30,
            _ => 600,
        };
        tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
    }
}

/// One step of `keep_stocked`: `Some(true)` if it classified a batch and
/// more are needed.
async fn stock_step(ctx: &BotContext) -> Option<bool> {
    if ctx.settings.get().question_source == QuestionSource::Opentdb
        || ctx.config.explainer.api_key.is_none()
    {
        return None;
    }
    ensure_ingested(ctx.clone()).await;
    let stats = ctx.db.triviaqa_stats(GENERATION_VERSION).await.ok()?;
    if stats.total == 0 || stats.ready >= READY_TARGET {
        return Some(false);
    }
    let outcome = classify_batch(ctx).await?;
    Some(outcome.ready + outcome.rejected + outcome.failed > 0)
}

// ── Public: offer a question for a round slot ────────────────────────────────

/// Whether this question slot should be attempted from TriviaQA at all:
/// the source setting allows it, a Groq key is configured, and — in `mixed`
/// mode — a random roll against `triviaqa_share`.
pub fn should_offer(ctx: &BotContext, source: QuestionSource) -> bool {
    if ctx.config.explainer.api_key.is_none() {
        return false;
    }
    match source {
        QuestionSource::Opentdb => false,
        QuestionSource::Triviaqa => true,
        QuestionSource::Mixed => {
            rand::thread_rng().gen_bool(ctx.settings.get().triviaqa_share.clamp(0.0, 1.0))
        }
    }
}

/// Which category groups a TriviaQA question may belong to, mirroring how
/// OpenTDB itself is restricted by `trivia.category` /
/// `trivia.excluded_categories`.
fn allowed_category_groups(ctx: &BotContext) -> Option<Vec<&'static str>> {
    let trivia = &ctx.config.trivia;
    match trivia.category {
        Some(id) => fetcher::category_group_for_id(id).map(|group| vec![group]),
        None => Some(
            fetcher::active_groups(&trivia.excluded_categories)
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        ),
    }
}

/// Turn a ready pool row into a quiz question (cleaned text, display-cased
/// answer).
fn to_question(row: TriviaQaRow) -> Option<FetchedQuestion> {
    Some(FetchedQuestion {
        category: row.category_group?,
        difficulty: row.difficulty?,
        question: clean_question(&row.question_text),
        correct_answer: display_answer(&row.correct_answer),
        incorrect_answers: row.distractors.filter(|d| d.len() == 3)?,
    })
}

/// Produce one TriviaQA question, or an error the caller should treat as
/// "fall back to OpenTDB for this slot". Only serves already classified
/// questions (see `keep_stocked`) — the round never waits for the LLM,
/// except when nothing is in stock yet at all.
///
/// `wanted_group` is the category group the round planned for this slot
/// (keeps rounds varied); `exclude_normalized` the questions already picked
/// for the current round.
pub async fn next_question(
    ctx: &BotContext,
    exclude_normalized: &HashSet<String>,
    wanted_group: Option<&str>,
) -> anyhow::Result<FetchedQuestion> {
    let Some(allowed) = allowed_category_groups(ctx) else {
        anyhow::bail!(
            "fixed OpenTDB category {:?} has no TriviaQA-mapped group",
            ctx.config.trivia.category
        );
    };
    let groups: Vec<&str> = match wanted_group {
        Some(wanted) => allowed
            .into_iter()
            .filter(|g| normalise(g) == normalise(wanted))
            .collect(),
        None => allowed,
    };
    if groups.is_empty() {
        anyhow::bail!("group {wanted_group:?} is not active");
    }
    let difficulty = ctx.config.trivia.difficulty.as_deref();

    for round in 0..2 {
        let candidates = ctx
            .db
            .triviaqa_ready(GENERATION_VERSION, &groups, difficulty, READY_CANDIDATES)
            .await?;
        for row in candidates {
            let text = clean_question(&row.question_text);
            if exclude_normalized.contains(&crate::db::normalize_question_text(&text)) {
                continue;
            }
            let recently_asked = ctx
                .db
                .question_recently_asked(&text, QUESTION_REUSE_COOLDOWN_DAYS)
                .await
                .unwrap_or(false);
            if recently_asked {
                continue;
            }
            let id = row.id;
            if let Some(q) = to_question(row) {
                info!(id, question = %q.question, category = %q.category, difficulty = %q.difficulty, "TriviaQA: question ready");
                return Ok(q);
            }
        }
        // Nothing in stock for this slot. Only a round without a specific
        // group waits for one fresh batch; a planned group falls back to
        // OpenTDB right away.
        if round == 0 && wanted_group.is_none() {
            ensure_ingested(ctx.clone()).await;
            if classify_batch(ctx).await.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    anyhow::bail!("no classified TriviaQA question in stock for {groups:?}")
}

/// Admin preview: a ready question with its classification, without
/// recording it as asked.
pub async fn preview(
    ctx: &BotContext,
    group: Option<&str>,
) -> anyhow::Result<Option<(i64, FetchedQuestion)>> {
    let groups: Vec<&str> = group.into_iter().collect();
    let rows = ctx
        .db
        .triviaqa_ready(GENERATION_VERSION, &groups, None, 1)
        .await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| Some((row.id, to_question(row)?))))
}

#[cfg(test)]
mod triviaqa_tests {
    use super::*;

    // ── parse_dataset ────────────────────────────────────────────────────

    const SAMPLE_JSON: &str = r#"{
        "Data": [
            {
                "Question": "Who painted the Mona Lisa?",
                "Answer": { "Value": "Leonardo da Vinci", "Aliases": ["da Vinci", "Leonardo"] }
            },
            {
                "Question": "  ",
                "Answer": { "Value": "should be filtered", "Aliases": [] }
            },
            {
                "Question": "What is the capital of France?",
                "Answer": { "Value": "Paris", "Aliases": [] }
            }
        ]
    }"#;

    #[test]
    fn parse_dataset_filters_blank_questions_and_respects_the_cap() {
        let rows = parse_dataset(SAMPLE_JSON.as_bytes(), 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].question_text, "Who painted the Mona Lisa?");
        assert_eq!(rows[0].correct_answer, "Leonardo da Vinci");
        assert_eq!(rows[0].aliases, vec!["da Vinci", "Leonardo"]);

        let capped = parse_dataset(SAMPLE_JSON.as_bytes(), 1).unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn parse_dataset_transparently_decompresses_gzip() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(SAMPLE_JSON.as_bytes()).unwrap();
        let gz_bytes = encoder.finish().unwrap();

        let rows = parse_dataset(&gz_bytes, 100).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parse_dataset_rejects_json_that_does_not_match_the_schema() {
        let err = parse_dataset(b"{\"nope\": true}", 100).unwrap_err();
        assert!(format!("{err:#}").contains("schema"));
    }

    // ── tar.gz archive support (the official distribution's actual shape) ──

    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn parse_dataset_reads_a_plain_tar_archive() {
        let tar_bytes = build_tar(&[("triviaqa/unfiltered-web-dev.json", SAMPLE_JSON.as_bytes())]);
        let rows = parse_dataset(&tar_bytes, 100).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parse_dataset_reads_a_gzipped_tar_archive() {
        // The actual shape of the official https://nlp.cs.washington.edu/triviaqa/ download.
        let tar_bytes = build_tar(&[("triviaqa/unfiltered-web-dev.json", SAMPLE_JSON.as_bytes())]);
        let rows = parse_dataset(&gzip(&tar_bytes), 100).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parse_dataset_prefers_the_dev_split_over_other_archive_entries() {
        let other_split = br#"{"Data": [{"Question": "wrong split", "Answer": {"Value": "x"}}]}"#;
        let tar_bytes = build_tar(&[
            ("triviaqa/unfiltered-web-train.json", other_split.as_slice()),
            ("triviaqa/README", b"not json".as_slice()),
            ("triviaqa/unfiltered-web-dev.json", SAMPLE_JSON.as_bytes()),
        ]);
        let rows = parse_dataset(&tar_bytes, 100).unwrap();
        // The dev split (SAMPLE_JSON), not the train split, must win —
        // regardless of archive order.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].question_text, "Who painted the Mona Lisa?");
    }

    #[test]
    fn parse_dataset_falls_back_to_any_json_entry_that_is_not_without_answers() {
        let without_answers =
            br#"{"Data": [{"Question": "no answer here", "Answer": {"Value": ""}}]}"#;
        let tar_bytes = build_tar(&[
            (
                "triviaqa/unfiltered-web-test-without-answers.json",
                without_answers.as_slice(),
            ),
            ("triviaqa/custom-dataset.json", SAMPLE_JSON.as_bytes()),
        ]);
        let rows = parse_dataset(&tar_bytes, 100).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parse_dataset_errors_when_archive_has_no_usable_json_entry() {
        let without_answers = br#"{"Data": []}"#;
        let tar_bytes = build_tar(&[(
            "triviaqa/unfiltered-web-test-without-answers.json",
            without_answers.as_slice(),
        )]);
        let err = parse_dataset(&tar_bytes, 100).unwrap_err();
        assert!(format!("{err:#}").contains("no usable"));
    }

    // ── parse_distractor_lines ────────────────────────────────────────────

    #[test]
    fn parse_distractor_lines_accepts_three_clean_lines() {
        let d = parse_distractor_lines(
            "Michelangelo\nRaphael\nDonatello".lines(),
            "Leonardo da Vinci",
            &[],
        );
        assert_eq!(d, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    #[test]
    fn parse_distractor_lines_strips_numbering_and_quotes() {
        let d = parse_distractor_lines(
            "1. Michelangelo\n2) \"Raphael\"\n- Donatello".lines(),
            "Leonardo da Vinci",
            &[],
        );
        assert_eq!(d, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    #[test]
    fn parse_distractor_lines_never_lets_the_correct_answer_through() {
        // The correct answer and a known alias both appear in the raw
        // response (e.g. the LLM restating it) but must be filtered out;
        // enough other valid lines remain to still reach 3.
        let raw = "Leonardo da Vinci\nda Vinci\nMichelangelo\nRaphael\nDonatello";
        let d = parse_distractor_lines(raw.lines(), "Leonardo da Vinci", &["da Vinci".to_owned()]);
        assert_eq!(d, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    #[test]
    fn parse_distractor_lines_deduplicates_case_insensitively() {
        let raw = "Michelangelo\nMICHELANGELO\nRaphael\nDonatello";
        let d = parse_distractor_lines(raw.lines(), "Leonardo da Vinci", &[]);
        assert_eq!(d, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    #[test]
    fn parse_distractor_lines_stops_short_when_too_few_survive_filtering() {
        // Only 2 survive once the correct answer is excluded.
        let raw = "Leonardo da Vinci\nMichelangelo\nRaphael";
        let d = parse_distractor_lines(raw.lines(), "Leonardo da Vinci", &[]);
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn parse_distractor_lines_ignores_absurdly_long_lines() {
        let long = "x".repeat(500);
        let raw = format!("{long}\nMichelangelo\nRaphael\nDonatello");
        let d = parse_distractor_lines(raw.lines(), "Leonardo da Vinci", &[]);
        assert_eq!(d, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    // ── canonicalize_* ───────────────────────────────────────────────────

    #[test]
    fn canonicalize_category_group_matches_known_groups_case_and_amp_insensitively() {
        assert_eq!(canonicalize_category_group("History"), Some("History"));
        assert_eq!(
            canonicalize_category_group("science and technology"),
            Some("Science & Technology")
        );
        assert_eq!(canonicalize_category_group("Not A Real Category"), None);
    }

    #[test]
    fn canonicalize_difficulty_only_accepts_the_three_known_values() {
        assert_eq!(canonicalize_difficulty(" Easy \n"), Some("easy"));
        assert_eq!(canonicalize_difficulty("HARD"), Some("hard"));
        assert_eq!(canonicalize_difficulty("impossible"), None);
    }

    #[test]
    fn every_category_group_is_described_in_the_prompt() {
        let prompt = generation_system_prompt();
        for (name, _) in fetcher::CATEGORY_GROUPS {
            let description = GROUP_DESCRIPTIONS.iter().find(|(n, _)| n == name);
            assert!(description.is_some(), "{name} has no description");
            assert!(
                prompt.contains(&format!("\"{name}\": ")),
                "{name} missing from prompt"
            );
        }
    }

    // ── batch replies ────────────────────────────────────────────────────

    fn row(id: i64, question: &str, answer: &str) -> TriviaQaRow {
        TriviaQaRow {
            id,
            question_text: question.to_owned(),
            correct_answer: answer.to_owned(),
            aliases: vec!["Leonardo".to_owned()],
            category_group: None,
            difficulty: None,
            distractors: None,
        }
    }

    #[test]
    fn a_batch_reply_is_validated_item_by_item() {
        let rows = vec![
            row(1, "Who painted the Mona Lisa?", "Leonardo da Vinci"),
            row(
                2,
                "Which rugby team will play at Langtree Park in 2012?",
                "St Helens",
            ),
            row(3, "What is the capital of France?", "Paris"),
            row(4, "Who wrote Hamlet?", "Shakespeare"),
        ];
        let raw = r#"Sure! ```json
{"items": [
  {"id": 1, "suitable": true, "category": "art", "difficulty": "Easy",
   "wrong_answers": ["Michelangelo", "Leonardo", "Raphael", "Titian"]},
  {"id": 2, "suitable": false, "reason": "time-bound"},
  {"id": 3, "suitable": true, "category": "Cooking", "difficulty": "easy",
   "wrong_answers": ["Lyon", "Nice", "Lille"]},
  {"id": 99, "suitable": true, "category": "Art", "difficulty": "easy",
   "wrong_answers": ["a", "b", "c"]}
]}
```"#;
        let verdicts = parse_batch(raw, &rows);
        assert_eq!(
            verdicts.len(),
            3,
            "unknown ids are ignored, missing ones absent"
        );
        assert_eq!(
            verdicts[0],
            (
                1,
                Verdict::Ready {
                    category_group: "Art",
                    difficulty: "easy",
                    // The alias "Leonardo" is never a wrong answer.
                    distractors: vec!["Michelangelo".into(), "Raphael".into(), "Titian".into()],
                }
            )
        );
        assert_eq!(verdicts[1], (2, Verdict::Unsuitable("time-bound".into())));
        assert_eq!(verdicts[2], (3, Verdict::Invalid), "unknown category");
        assert!(parse_batch("no json here", &rows).is_empty());
    }

    #[test]
    fn too_few_distinct_wrong_answers_are_invalid() {
        let rows = vec![row(1, "Capital of France?", "PARIS")];
        let raw = r#"{"items": [{"id": 1, "suitable": true, "category": "Geography",
            "difficulty": "easy", "wrong_answers": ["Paris", "Lyon", "lyon"]}]}"#;
        assert_eq!(parse_batch(raw, &rows), vec![(1, Verdict::Invalid)]);
    }

    // ── text cleanup ─────────────────────────────────────────────────────

    #[test]
    fn questions_lose_csv_quotes_and_gain_a_question_mark() {
        assert_eq!(
            clean_question(r#""In what city did James Joyce's ""Ulysses"" take place?""#),
            r#"In what city did James Joyce's "Ulysses" take place?"#
        );
        assert_eq!(
            clean_question("What was Ian Fleming’s first Bond book"),
            "What was Ian Fleming’s first Bond book?"
        );
        assert_eq!(
            clean_question("Name the largest ocean"),
            "Name the largest ocean"
        );
        assert_eq!(clean_question("Who is it?"), "Who is it?");
    }

    #[test]
    fn all_caps_answers_are_title_cased_but_acronyms_kept() {
        assert_eq!(display_answer("SCOTTISH TERRIER"), "Scottish Terrier");
        assert_eq!(display_answer("CHILE"), "Chile");
        assert_eq!(display_answer("BAY OF BISCAY"), "Bay of Biscay");
        assert_eq!(display_answer("USA"), "USA");
        assert_eq!(display_answer("NATO"), "NATO");
        assert_eq!(display_answer("Leonardo da Vinci"), "Leonardo da Vinci");
        assert_eq!(display_answer("1066"), "1066");
    }

    // ── ingest key ───────────────────────────────────────────────────────

    #[test]
    fn ingest_key_changes_when_url_or_cap_changes() {
        let a = ingest_kv_key("https://example.org/a.json", 100);
        let b = ingest_kv_key("https://example.org/b.json", 100);
        let c = ingest_kv_key("https://example.org/a.json", 200);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
