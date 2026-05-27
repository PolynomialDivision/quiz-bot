//! Groq-powered trivia explainer with Wikimedia Commons image search.
//!
//! After each question reveal, this module:
//!   • Asks Groq for 2–4 sentences of background on the answer, plus a
//!     search keyword to find an illustrating photo.
//!   • Searches Wikimedia Commons with that keyword and resolves the first
//!     matching image to a direct upload URL.
//!
//! The LLM never guesses filenames — it only suggests a search term.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── System prompt ─────────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "\
You are a trivia explanation assistant for a Matrix chat quiz bot.

After each question you receive the question text and its correct answer (sourced from OpenTDB).
Your job is to help players understand and remember the answer.

Write a short, engaging explanation (2–4 sentences) covering why the answer is correct and an \
interesting fact or context. No sections, no headers, no bullet points — just natural prose.

On the very last line write exactly:
IMAGE_SEARCH: <keyword>

Where <keyword> is a short Wikimedia Commons search term that would find a good illustration \
of the answer (e.g. \"Eiffel Tower Paris\" or \"Albert Einstein physicist\").
Nothing may follow the IMAGE_SEARCH line.";

const MAX_ATTEMPTS:    u32 = 3;
const REQUEST_TIMEOUT: u64 = 30; // seconds for Groq API call
const FETCH_TIMEOUT:   u64 = 15; // seconds for image download

const USER_AGENT: &str = "quiz-bot/1.0 (Matrix trivia quiz; https://matrix.org)";

// ── Result type ───────────────────────────────────────────────────────────────

pub struct ExplainerResult {
    pub text:      String,
    pub image_url: Option<String>,  // direct upload.wikimedia.org URL
}

// ── Groq / OpenAI-compatible chat completions ─────────────────────────────────

#[derive(Serialize)]
struct ApiRequest<'a> {
    model:      &'a str,
    max_tokens: u32,
    messages:   Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role:    &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiChoiceMessage,
}

#[derive(Deserialize)]
struct ApiChoiceMessage {
    content: String,
}

// ── Response parsing ──────────────────────────────────────────────────────────

/// Split the LLM response into `(prose_text, optional_search_keyword)`.
fn parse_response(raw: String) -> (String, Option<String>) {
    let mut text_lines:  Vec<&str> = Vec::new();
    let mut search_term: Option<String> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("IMAGE_SEARCH:") {
            let kw = rest.trim();
            if !kw.is_empty() {
                search_term = Some(kw.to_owned());
            }
        } else {
            text_lines.push(line);
        }
    }

    while text_lines.last().map(|l: &&str| l.trim().is_empty()).unwrap_or(false) {
        text_lines.pop();
    }

    (text_lines.join("\n"), search_term)
}

// ── Wikimedia Commons image search ───────────────────────────────────────────

/// Search Wikimedia Commons for files matching `keyword` and return the direct
/// upload URL of the first suitable image found (jpg/jpeg/png/webp).
async fn search_commons_image(client: &reqwest::Client, keyword: &str) -> Option<String> {
    // Step 1: search the File namespace for matching titles.
    let mut search_url = reqwest::Url::parse("https://commons.wikimedia.org/w/api.php").unwrap();
    search_url.query_pairs_mut()
        .append_pair("action",      "query")
        .append_pair("list",        "search")
        .append_pair("srsearch",    keyword)
        .append_pair("srnamespace", "6")       // File namespace
        .append_pair("srlimit",     "5")
        .append_pair("format",      "json");

    let search_resp: serde_json::Value = match client
        .get(search_url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
    {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Explainer: Commons search parse failed for {keyword:?}: {e}");
                return None;
            }
        },
        Err(e) => {
            warn!("Explainer: Commons search request failed for {keyword:?}: {e}");
            return None;
        }
    };

    // Pick the first result that looks like a photo (jpg/jpeg/png/webp).
    let hits = search_resp["query"]["search"].as_array()?;
    let file_title = hits.iter()
        .filter_map(|h| h["title"].as_str())
        .find(|t| {
            let lower = t.to_lowercase();
            lower.ends_with(".jpg")  || lower.ends_with(".jpeg") ||
            lower.ends_with(".png")  || lower.ends_with(".webp")
        })?;

    info!("Explainer: Commons search {keyword:?} → {file_title}");

    // Step 2: resolve the file title to a direct upload URL via imageinfo.
    let mut info_url = reqwest::Url::parse("https://commons.wikimedia.org/w/api.php").unwrap();
    info_url.query_pairs_mut()
        .append_pair("action",  "query")
        .append_pair("titles",  file_title)
        .append_pair("prop",    "imageinfo")
        .append_pair("iiprop",  "url")
        .append_pair("format",  "json");

    let info_resp: serde_json::Value = match client
        .get(info_url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
    {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Explainer: Commons imageinfo parse failed for {file_title:?}: {e}");
                return None;
            }
        },
        Err(e) => {
            warn!("Explainer: Commons imageinfo request failed for {file_title:?}: {e}");
            return None;
        }
    };

    let direct = info_resp["query"]["pages"]
        .as_object()?
        .values()
        .next()?
        .get("imageinfo")?
        .get(0)?
        .get("url")?
        .as_str()
        .map(|s| s.to_owned());

    if let Some(ref u) = direct {
        info!("Explainer: resolved image URL → {u}");
    } else {
        warn!("Explainer: Commons imageinfo returned no URL for {file_title:?}");
    }

    direct
}

// ── Image dimension parsing ───────────────────────────────────────────────────

/// Extract (width, height) from raw PNG or JPEG bytes without an external crate.
/// Returns `None` for unknown/unsupported formats.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte magic, then IHDR chunk: 4 len + 4 type + 4 w + 4 h
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }

    // JPEG: scan for SOF markers (0xFF 0xCn) that carry frame dimensions.
    if bytes.starts_with(b"\xFF\xD8") {
        let mut i = 2usize;
        while i + 4 < bytes.len() {
            if bytes[i] != 0xFF { break; }
            let marker = bytes[i + 1];
            let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            // SOF0–SOF3 and SOF5–SOF7 etc. carry height/width at offsets +5/+7.
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
                && i + 8 < bytes.len()
            {
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + seg_len;
        }
    }

    None
}

// ── Image download ────────────────────────────────────────────────────────────

/// Download a direct image URL. Returns `(bytes, mime_type)` or `None`.
pub async fn fetch_image_bytes(url: &str) -> Option<(Vec<u8>, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| warn!("Explainer: failed to build fetch client: {e}"))
        .ok()?;

    let resp = match client.get(url).send().await {
        Ok(r)  => r,
        Err(e) => {
            warn!("Explainer: image download failed for {url}: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        warn!("Explainer: image download returned {} for {url}", resp.status());
        return None;
    }

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .trim()
        .to_owned();

    match resp.bytes().await {
        Ok(b)  => Some((b.to_vec(), ct)),
        Err(e) => {
            warn!("Explainer: failed to read image body for {url}: {e}");
            None
        }
    }
}

// ── Groq call ─────────────────────────────────────────────────────────────────

async fn call_groq(
    client:       &reqwest::Client,
    api_key:      &str,
    model:        &str,
    user_content: &str,
) -> Option<String> {
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let delay = 2u64.pow(attempt - 2); // 1 s, 2 s
            warn!("Explainer: retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let body = ApiRequest {
            model,
            max_tokens: 512,
            messages: vec![
                ApiMessage { role: "system", content: SYSTEM_PROMPT },
                ApiMessage { role: "user",   content: user_content  },
            ],
        };

        let resp = match client
            .post("https://api.groq.com/openai/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r)  => r,
            Err(e) => {
                warn!("Explainer: request failed (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            warn!("Explainer: auth error {status} — check explainer.api_key in config");
            return None;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("Explainer: API error {status} (attempt {attempt}/{MAX_ATTEMPTS}): {body}");
            continue;
        }

        let data: ApiResponse = match resp.json().await {
            Ok(d)  => d,
            Err(e) => {
                warn!("Explainer: response parse error (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let content = data.choices.into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        if content.trim().is_empty() {
            warn!("Explainer: empty content (attempt {attempt}/{MAX_ATTEMPTS})");
            continue;
        }

        info!("Explainer: got response on attempt {attempt}/{MAX_ATTEMPTS}");
        return Some(content);
    }
    None
}

// ── Public interface ──────────────────────────────────────────────────────────

/// Ask Groq for background info about the quiz `question` and `answer`.
/// Groq also returns an IMAGE_SEARCH keyword which we use to find a real
/// photo on Wikimedia Commons — no filename guessing.
pub async fn explain(
    question: &str,
    answer:   &str,
    api_key:  &str,
    model:    &str,
) -> Option<ExplainerResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT))
        .build()
        .map_err(|e| warn!("Explainer: failed to build client: {e}"))
        .ok()?;

    let user_content = format!("Question: {question}\nAnswer: {answer}");

    let raw = call_groq(&client, api_key, model, &user_content).await?;
    let (text, search_term) = parse_response(raw);

    if text.trim().is_empty() {
        warn!("Explainer: empty text for question \"{question}\"");
        return None;
    }

    let image_url = match search_term {
        None => {
            info!("Explainer: LLM provided no IMAGE_SEARCH keyword for question \"{question}\"");
            None
        }
        Some(ref kw) => {
            info!("Explainer: searching Commons for {kw:?}");
            search_commons_image(&client, kw).await
        }
    };

    Some(ExplainerResult { text, image_url })
}
