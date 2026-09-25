//! TriviaQA as a second question source, alongside OpenTDB (`fetcher.rs`).
//!
//! TriviaQA supplies the question and the single verified correct answer,
//! but has no multiple-choice distractors, category, or difficulty of its
//! own — so this module asks the same Groq/LLM integration `explainer.rs`
//! already uses (see `crate::groq`) to generate 3 plausible wrong answers
//! *and* classify the question into one of the bot's existing category
//! groups (`fetcher::CATEGORY_GROUPS`) and difficulties (easy/medium/hard),
//! reusing that vocabulary rather than inventing a TriviaQA-specific one.
//! The LLM is never given the ability to change the question or correct
//! answer — it only ever sees them as read-only context in the prompt, and
//! its reply is parsed *exclusively* into a category/difficulty/distractors
//! triple (see `validate_generation`). The `FetchedQuestion` returned to the
//! rest of the bot always carries the question/answer exactly as read from
//! the TriviaQA dataset.
//!
//! ## How it fits the existing architecture
//! - Sourced questions become an ordinary `state::FetchedQuestion` — the
//!   same type OpenTDB produces, with a real category group and a real
//!   easy/medium/hard difficulty — so quiz.rs, format.rs, and the DB layer
//!   need no source-specific handling downstream, and existing category
//!   exclusion / diversity / difficulty-filter config applies unchanged
//!   (see `allowed_category_groups`).
//! - Dedup/history is the *shared* `questions` table (see
//!   `db::Db::question_recently_asked`): a TriviaQA question that was
//!   asked yesterday is exactly as "recently asked" as an OpenTDB one.
//! - The downloaded dataset itself is ingested once into a dedicated
//!   `triviaqa_pool` table (not re-parsed into memory on every startup);
//!   `bot_kv` (already used for other one-shot state) records whether
//!   ingestion for the current `dataset_url`/`max_pool_size` has run.
//!   Classification + distractors are likewise generated once per row, on
//!   first selection, and cached on that row (`Db::save_triviaqa_generation`)
//!   rather than during ingestion — classifying the full pool up front would
//!   mean an LLM call per row before the bot could offer anything.
//! - `fetcher::fetch_round_questions`/`fetcher::next_question` decide,
//!   per question slot, whether to try this module first — see
//!   `should_offer`. Any failure (pool empty/exhausted for the active
//!   filters, every candidate within its reuse cooldown, LLM
//!   classification/distractor generation failed validation) simply falls
//!   back to OpenTDB, so TriviaQA is additive, never a hard dependency.

use std::collections::HashSet;
use std::io::Read;
use std::sync::LazyLock;

use anyhow::Context;
use rand::Rng;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    db::{Db, TriviaQaImportRow, TriviaQaRow},
    fetcher::{self, normalise, QUESTION_REUSE_COOLDOWN_DAYS},
    groq,
    state::FetchedQuestion,
    BotContext,
};

/// How many distinct pool candidates to try before giving up on TriviaQA
/// for this question slot and letting the caller fall back to OpenTDB.
const MAX_SAMPLE_ATTEMPTS: usize = 8;
/// How many times to ask the LLM for classification + distractors
/// (including re-prompts after a validation failure) before giving up on
/// this specific question.
const MAX_GENERATION_ATTEMPTS: u32 = 2;

/// Built once per call rather than as a `const` so the category list stays
/// in sync with `fetcher::CATEGORY_GROUPS` (the bot's single source of
/// truth for category names) instead of duplicating it here.
fn generation_system_prompt() -> String {
    let categories: Vec<&str> = fetcher::CATEGORY_GROUPS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    format!(
        "You classify trivia questions and generate multiple-choice distractors \
         for a trivia quiz bot.

You will be given a QUESTION and its single CORRECT ANSWER. Reply with
exactly 5 lines, and nothing else:

CATEGORY: <exactly one of: {}>
DIFFICULTY: <exactly one of: easy, medium, hard>
<incorrect but plausible answer 1>
<incorrect but plausible answer 2>
<incorrect but plausible answer 3>

Rules:
- CATEGORY must be copied exactly from the list above — no other wording.
- DIFFICULTY must be exactly one of: easy, medium, hard.
- Each distractor line is a short answer in the same style as the correct \
  answer, with no numbering, bullets, or explanations.
- All 3 distractors must be wrong, and clearly different from each other.
- Never output the correct answer or a rephrasing of it.
- Never output or alter the question itself.",
        categories.join(", ")
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
    if !cfg.enabled {
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

// ── Classification + distractor generation/validation ─────────────────────────

/// Find the first line starting with `prefix` (case-insensitive) and return
/// the trimmed text after it.
fn extract_field<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    raw.lines().find_map(|line| {
        let t = line.trim();
        (t.len() >= prefix.len() && t[..prefix.len()].eq_ignore_ascii_case(prefix))
            .then(|| t[prefix.len()..].trim())
    })
}

/// Match free-form LLM output against the bot's actual category groups
/// (`fetcher::CATEGORY_GROUPS`) — the single source of truth — rather than a
/// separate TriviaQA-specific list, so a match is guaranteed to plug
/// straight into the bot's existing category filtering/diversity logic.
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

/// Extract up to 3 usable distractor lines from an iterator of raw lines
/// (already excluding any CATEGORY:/DIFFICULTY: header lines) — cleans
/// numbering/quotes, and drops anything blank, too long, a duplicate, or
/// matching the correct answer/an alias.
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

/// One successful classification + distractor generation.
struct Generation {
    category_group: &'static str,
    difficulty: &'static str,
    distractors: Vec<String>,
}

/// Parse and validate an LLM response into a category group (one of
/// `fetcher::CATEGORY_GROUPS`), a difficulty (easy/medium/hard), and exactly
/// 3 usable distractors — or `None` if any part is missing/invalid. Pure and
/// network-free — see `triviaqa_tests::validate_generation_*`.
fn validate_generation(raw: &str, correct_answer: &str, aliases: &[String]) -> Option<Generation> {
    let category_group = extract_field(raw, "CATEGORY:").and_then(canonicalize_category_group)?;
    let difficulty = extract_field(raw, "DIFFICULTY:").and_then(canonicalize_difficulty)?;

    let distractor_lines = raw.lines().filter(|line| {
        let t = line.trim();
        !t.to_uppercase().starts_with("CATEGORY:") && !t.to_uppercase().starts_with("DIFFICULTY:")
    });
    let distractors = parse_distractor_lines(distractor_lines, correct_answer, aliases);

    (distractors.len() == 3).then_some(Generation {
        category_group,
        difficulty,
        distractors,
    })
}

/// Ask the LLM to classify `candidate` (category group + difficulty, reusing
/// the bot's existing OpenTDB-derived vocabulary) and generate 3 wrong
/// answers, retrying a couple of times if the response doesn't validate.
/// Returns `None` (after logging) if the LLM is unavailable/misconfigured or
/// never produces a usable response — callers must fall back to another
/// question rather than serve an under-filled or unclassified one.
async fn generate(ctx: &BotContext, candidate: &TriviaQaRow) -> Option<Generation> {
    let api_key = ctx.config.explainer.api_key.as_deref()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| warn!("TriviaQA: failed to build Groq client: {e}"))
        .ok()?;

    let system_prompt = generation_system_prompt();
    let user_content = format!(
        "QUESTION: {}\nCORRECT ANSWER: {}",
        candidate.question_text, candidate.correct_answer
    );

    for attempt in 1..=MAX_GENERATION_ATTEMPTS {
        let raw = groq::complete(
            &client,
            api_key,
            &ctx.config.explainer.model,
            &system_prompt,
            &user_content,
        )
        .await?;

        if let Some(g) = validate_generation(&raw, &candidate.correct_answer, &candidate.aliases) {
            return Some(g);
        }
        warn!(
            "TriviaQA: LLM classification/distractors failed validation for {:?} \
             (attempt {attempt}/{MAX_GENERATION_ATTEMPTS})",
            candidate.question_text
        );
    }
    None
}

// ── Public: offer a question for a round slot ────────────────────────────────

/// Whether this question slot should be attempted from TriviaQA at all —
/// enabled in config, a Groq key is configured (needed for classification
/// and distractors), and a random roll against `mix_ratio`. Doesn't check
/// the pool itself; `next_question` naturally falls through to an error
/// (and the caller to OpenTDB) if the pool turns out to be empty or
/// exhausted.
pub fn should_offer(ctx: &BotContext) -> bool {
    let cfg = &ctx.config.trivia.triviaqa;
    cfg.enabled
        && ctx.config.explainer.api_key.is_some()
        && rand::thread_rng().gen_bool(cfg.mix_ratio.clamp(0.0, 1.0))
}

/// Which category groups a TriviaQA candidate is allowed to belong to,
/// mirroring how OpenTDB itself is restricted by `trivia.category` /
/// `trivia.excluded_categories` — a fixed category maps to that category's
/// group (`None` if it's outside our known groups, meaning TriviaQA simply
/// can't contribute in this config), otherwise every non-excluded group is
/// allowed, same as OpenTDB's own balanced selection.
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

/// Produce one TriviaQA-sourced question, or an error the caller should
/// treat as "fall back to OpenTDB for this slot".
///
/// `exclude_normalized` is the set of normalized question texts already
/// picked for the *current* round (from either source) — history in the DB
/// only covers questions from *past* rounds (they're recorded once posted,
/// which happens after the whole round is assembled), so this catches an
/// in-progress round accidentally sampling the same TriviaQA row twice.
///
/// Respects the same `trivia.category` / `trivia.excluded_categories` /
/// `trivia.difficulty` config OpenTDB honors, via `fetcher`'s own group
/// vocabulary — see `allowed_category_groups`.
pub async fn next_question(
    ctx: &BotContext,
    exclude_normalized: &HashSet<String>,
) -> anyhow::Result<FetchedQuestion> {
    let Some(allowed_groups) = allowed_category_groups(ctx) else {
        anyhow::bail!(
            "fixed OpenTDB category {:?} has no TriviaQA-mapped group",
            ctx.config.trivia.category
        );
    };
    let allowed_groups: Vec<&str> = allowed_groups;
    let filter_difficulty = ctx.config.trivia.difficulty.as_deref();

    for attempt in 1..=MAX_SAMPLE_ATTEMPTS {
        let Some(candidate) = ctx
            .db
            .sample_triviaqa_candidate(&allowed_groups, filter_difficulty)
            .await?
        else {
            anyhow::bail!("TriviaQA pool has no matching candidate");
        };

        let normalized = crate::db::normalize_question_text(&candidate.question_text);
        if exclude_normalized.contains(&normalized) {
            continue;
        }
        let recently_asked = ctx
            .db
            .question_recently_asked(&candidate.question_text, QUESTION_REUSE_COOLDOWN_DAYS)
            .await
            .unwrap_or(false);
        if recently_asked {
            continue;
        }

        let (category_group, difficulty, distractors) = match (
            candidate.category_group.clone(),
            candidate.difficulty.clone(),
            candidate.distractors.clone(),
        ) {
            (Some(cg), Some(diff), Some(d)) if d.len() == 3 => (cg, diff, d),
            _ => {
                let Some(g) = generate(ctx, &candidate).await else {
                    // This specific question didn't pan out — try another
                    // candidate rather than giving up on TriviaQA entirely.
                    continue;
                };
                ctx.db
                    .save_triviaqa_generation(
                        candidate.id,
                        g.category_group,
                        g.difficulty,
                        &g.distractors,
                    )
                    .await?;
                // Freshly classified but not what this slot needs (e.g. the
                // fixed category/difficulty didn't match) — it's cached now
                // for a future slot that does want it, but not this one.
                let group_matches = allowed_groups
                    .iter()
                    .any(|allowed| normalise(allowed) == normalise(g.category_group));
                let difficulty_matches = filter_difficulty.is_none_or(|d| d == g.difficulty);
                if !group_matches || !difficulty_matches {
                    continue;
                }
                (
                    g.category_group.to_owned(),
                    g.difficulty.to_owned(),
                    g.distractors,
                )
            }
        };

        info!(
            attempt,
            question = %candidate.question_text,
            category_group,
            difficulty,
            "TriviaQA: question ready"
        );
        return Ok(FetchedQuestion {
            category: category_group,
            difficulty,
            question: candidate.question_text,
            correct_answer: candidate.correct_answer,
            incorrect_answers: distractors,
        });
    }

    anyhow::bail!("no usable TriviaQA candidate found after {MAX_SAMPLE_ATTEMPTS} attempts")
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

    // ── extract_field / canonicalize_* ───────────────────────────────────

    #[test]
    fn extract_field_is_case_insensitive_and_trims() {
        assert_eq!(
            extract_field("category:  History  ", "CATEGORY:"),
            Some("History")
        );
        assert_eq!(
            extract_field("Category: History", "CATEGORY:"),
            Some("History")
        );
        assert_eq!(extract_field("Something else", "CATEGORY:"), None);
    }

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

    // ── validate_generation ───────────────────────────────────────────────

    const GOOD_GENERATION_RESPONSE: &str = "\
CATEGORY: Art
DIFFICULTY: medium
Michelangelo
Raphael
Donatello";

    #[test]
    fn validate_generation_accepts_a_well_formed_response() {
        let g = validate_generation(GOOD_GENERATION_RESPONSE, "Leonardo da Vinci", &[]).unwrap();
        assert_eq!(g.category_group, "Art");
        assert_eq!(g.difficulty, "medium");
        assert_eq!(g.distractors, vec!["Michelangelo", "Raphael", "Donatello"]);
    }

    #[test]
    fn validate_generation_rejects_an_unrecognized_category() {
        let raw =
            "CATEGORY: Renaissance Trivia\nDIFFICULTY: medium\nMichelangelo\nRaphael\nDonatello";
        assert!(validate_generation(raw, "Leonardo da Vinci", &[]).is_none());
    }

    #[test]
    fn validate_generation_rejects_an_unrecognized_difficulty() {
        let raw = "CATEGORY: Art\nDIFFICULTY: extreme\nMichelangelo\nRaphael\nDonatello";
        assert!(validate_generation(raw, "Leonardo da Vinci", &[]).is_none());
    }

    #[test]
    fn validate_generation_rejects_a_missing_header() {
        assert!(validate_generation(
            "DIFFICULTY: medium\nMichelangelo\nRaphael\nDonatello",
            "Leonardo da Vinci",
            &[]
        )
        .is_none());
    }

    #[test]
    fn validate_generation_does_not_mistake_a_header_line_for_a_distractor() {
        // 5 lines total, but only 3 are real distractor candidates once the
        // two header lines are excluded from that pass.
        let g = validate_generation(GOOD_GENERATION_RESPONSE, "Leonardo da Vinci", &[]).unwrap();
        assert_eq!(g.distractors.len(), 3);
        assert!(!g
            .distractors
            .iter()
            .any(|d| d.to_lowercase().contains("category")));
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
