//! Minimal Groq (OpenAI-compatible) chat completions client, shared by
//! `explainer` (post-question background info + image search keyword) and
//! `triviaqa` (multiple-choice distractor generation). Callers own their
//! own system prompt and response parsing — this module is only the HTTP
//! call, retry, and error handling.

use serde::{Deserialize, Serialize};
use tracing::warn;

const MAX_ATTEMPTS: u32 = 3;

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

/// POST a chat completion to Groq, retrying transient failures (network
/// errors, non-2xx responses other than auth, empty content) with
/// exponential backoff. Returns `None` — after logging why — on auth
/// errors or once `MAX_ATTEMPTS` is exhausted; callers decide how to
/// degrade (skip the explanation, fall back to another question source, …).
pub async fn complete(
    client:        &reqwest::Client,
    api_key:       &str,
    model:         &str,
    system_prompt: &str,
    user_content:  &str,
) -> Option<String> {
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let delay = 2u64.pow(attempt - 2); // 1 s, 2 s
            warn!("Groq: retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let body = ApiRequest {
            model,
            max_tokens: 512,
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
                warn!("Groq: request failed (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            warn!("Groq: auth error {status} — check explainer.api_key in config");
            return None;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("Groq: API error {status} (attempt {attempt}/{MAX_ATTEMPTS}): {body}");
            continue;
        }

        let data: ApiResponse = match resp.json().await {
            Ok(d)  => d,
            Err(e) => {
                warn!("Groq: response parse error (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let content = data.choices.into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        if content.trim().is_empty() {
            warn!("Groq: empty content (attempt {attempt}/{MAX_ATTEMPTS})");
            continue;
        }

        return Some(content);
    }
    None
}
