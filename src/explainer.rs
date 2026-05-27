use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Prompt that asks for explanation + image.
const SYSTEM_PROMPT_WITH_IMAGE: &str = "\
You are a trivia explanation assistant for a Matrix chat quiz bot.

After each question you receive the question text and its correct answer (sourced from OpenTDB).
Your job is to help players understand and remember the answer.

Write a short, engaging explanation (2–4 sentences) covering why the answer is correct and an \
interesting fact or context. No sections, no headers, no bullet points — just natural prose.

On the very last line, include one image that illustrates the answer:
IMAGE: <url>

Rules for the IMAGE line:
- The most important thing: the image must be the best possible illustration of the answer.
- Only include a URL you are confident actually exists and is publicly accessible. \
If you are not certain the URL is real, omit the IMAGE line — a missing image is better than a broken one.
- Any source is fine: Wikimedia Commons, Wikipedia, news archives, museums, etc.
- Nothing may appear after the IMAGE line.";

/// Fallback prompt used when image retrieval failed — text only, no IMAGE line.
const SYSTEM_PROMPT_TEXT_ONLY: &str = "\
You are a trivia explanation assistant for a Matrix chat quiz bot.

After each question you receive the question text and its correct answer (sourced from OpenTDB).
Your job is to help players understand and remember the answer.

Write a short, engaging explanation (2–4 sentences) covering why the answer is correct and an \
interesting fact or context. No sections, no headers, no bullet points — just natural prose.";

const MAX_ATTEMPTS:    u32 = 3;
const REQUEST_TIMEOUT: u64 = 30; // seconds for Groq API call
const IMAGE_TIMEOUT:   u64 = 8;  // seconds for image validation / Commons API
const FETCH_TIMEOUT:   u64 = 15; // seconds for image download

// ── Result type ───────────────────────────────────────────────────────────────

pub struct ExplainerResult {
    pub text:      String,
    pub image_url: Option<String>,  // validated, resolved direct image URL
}

// ── Groq / OpenAI-compatible chat completions types ───────────────────────────

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

/// Split raw LLM output into (explanation_text, optional_raw_image_url).
/// The IMAGE: line is stripped from the text regardless of outcome.
fn parse_response(raw: String) -> (String, Option<String>) {
    let mut text_lines: Vec<&str> = Vec::new();
    let mut image_url: Option<String> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("IMAGE:") {
            let url = rest.trim();
            if url.starts_with("http") {
                image_url = Some(url.to_owned());
            }
        } else {
            text_lines.push(line);
        }
    }

    // Drop trailing blank lines.
    while text_lines.last().map(|l: &&str| l.trim().is_empty()).unwrap_or(false) {
        text_lines.pop();
    }

    (text_lines.join("\n"), image_url)
}

// ── Image URL resolution + validation ────────────────────────────────────────

/// Resolve a Wikimedia Commons file page URL to a direct upload URL via the API.
/// "https://commons.wikimedia.org/wiki/File:Foo.jpg"
///   → "https://upload.wikimedia.org/wikipedia/commons/…/Foo.jpg"
async fn resolve_commons_url(client: &reqwest::Client, url: &str) -> Option<String> {
    let title = url.split("/wiki/").nth(1)?;
    let api_url = format!(
        "https://commons.wikimedia.org/w/api.php\
         ?action=query&titles={title}&prop=imageinfo&iiprop=url&format=json"
    );

    let resp: serde_json::Value = client
        .get(&api_url)
        .header("User-Agent", "quiz-bot/1.0")
        .send()
        .await
        .map_err(|e| warn!("Explainer: Commons API request failed: {e}"))
        .ok()?
        .json()
        .await
        .map_err(|e| warn!("Explainer: Commons API parse failed: {e}"))
        .ok()?;

    let direct = resp["query"]["pages"]
        .as_object()?
        .values()
        .next()?
        .get("imageinfo")?
        .get(0)?
        .get("url")?
        .as_str()
        .map(|s| s.to_owned());

    if direct.is_none() {
        warn!("Explainer: Commons API returned no imageinfo URL for {url}");
    }
    direct
}

/// Validate a raw image URL from the LLM and return a resolved direct URL, or None.
///
/// - Wikimedia Commons page URLs  → resolved via the Commons API
/// - Everything else              → verified with a HEAD request (must be image/*)
///
/// Returns None (with a warning) for broken, hallucinated, or non-image URLs.
async fn resolve_image_url(raw_url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(IMAGE_TIMEOUT))
        .user_agent("quiz-bot/1.0")
        .build()
        .map_err(|e| warn!("Explainer: failed to build image client: {e}"))
        .ok()?;

    if raw_url.contains("commons.wikimedia.org/wiki/") {
        return resolve_commons_url(&client, raw_url).await;
    }

    // HEAD request — fast check without downloading the full image.
    let resp = match client.head(raw_url).send().await {
        Ok(r)  => r,
        Err(e) => {
            warn!("Explainer: image HEAD request failed for {raw_url}: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        warn!("Explainer: image URL returned {} for {raw_url}", resp.status());
        return None;
    }

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if ct.starts_with("image/") {
        Some(raw_url.to_owned())
    } else {
        warn!("Explainer: image URL has non-image content-type {ct:?}: {raw_url}");
        None
    }
}

// ── Image download ────────────────────────────────────────────────────────────

/// Download a validated image URL.  Returns (bytes, mime_type) or None on any failure.
pub async fn fetch_image_bytes(url: &str) -> Option<(Vec<u8>, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT))
        .user_agent("quiz-bot/1.0")
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

// ── Public interface ──────────────────────────────────────────────────────────

/// Send one chat-completions request to Groq and return the raw response text.
/// Retries up to MAX_ATTEMPTS on network / server errors.
/// Returns None on auth errors or if all attempts fail.
async fn call_groq(
    client:        &reqwest::Client,
    api_key:       &str,
    model:         &str,
    system_prompt: &str,
    user_content:  &str,
) -> Option<String> {
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let delay = 2u64.pow(attempt - 2); // 1 s, 2 s
            warn!("Explainer: retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let body = ApiRequest {
            model,
            max_tokens: 1024,
            messages: vec![
                ApiMessage { role: "system", content: system_prompt },
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
            warn!("Explainer: auth error {status} — check api_key in config");
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

/// Call the Groq API to generate a quiz explanation with an optional image.
///
/// Flow:
/// 1. Ask Groq for explanation + image URL.
/// 2. Validate the image URL.
/// 3. If the image is unusable, ask Groq again with a text-only prompt so the
///    explanation isn't shaped around a failed image suggestion.
/// 4. Always returns Some as long as we got explanation text — image failure
///    never prevents the explanation from being posted.
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

    let user_content = format!("Question: {question}\nProvided Answer: {answer}");

    // ── First pass: ask for explanation + image ───────────────────────────────
    let raw = call_groq(&client, api_key, model, SYSTEM_PROMPT_WITH_IMAGE, &user_content).await?;
    let (text, raw_url) = parse_response(raw);

    if text.trim().is_empty() {
        warn!("Explainer: explanation text empty after parsing — giving up");
        return None;
    }

    // ── Validate image URL ────────────────────────────────────────────────────
    let image_url = match raw_url {
        None => None,
        Some(ref url) => match resolve_image_url(url).await {
            Some(resolved) => {
                info!("Explainer: image resolved to {resolved}");
                Some(resolved)
            }
            None => {
                warn!("Explainer: image unusable — retrying with text-only prompt");
                None
            }
        },
    };

    // ── Second pass (text-only) if image failed ───────────────────────────────
    let final_text = if image_url.is_none() && raw_url.is_some() {
        // The model suggested an image but it didn't work — ask again without
        // the image instruction so the explanation stands cleanly on its own.
        match call_groq(&client, api_key, model, SYSTEM_PROMPT_TEXT_ONLY, &user_content).await {
            Some(raw2) => {
                let (t, _) = parse_response(raw2);
                if t.trim().is_empty() { text } else { t }
            }
            None => {
                warn!("Explainer: text-only retry failed — using text from first pass");
                text
            }
        }
    } else {
        text
    };

    Some(ExplainerResult { text: final_text, image_url })
}
