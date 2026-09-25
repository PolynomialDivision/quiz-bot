//! Minimal Groq (OpenAI-compatible) chat completions client, shared by
//! `explainer` (post-question background info + image search keyword) and
//! `triviaqa` (classification + multiple-choice distractor generation).
//! Callers own their own system prompt and response parsing — this module is
//! only the HTTP call, retry, and error handling.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

const MAX_ATTEMPTS: u32 = 3;
const API_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

/// One HTTP client (connection pool) for every Groq call.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("building the Groq HTTP client")
});

/// Per-call options.
#[derive(Clone, Copy)]
pub struct Options {
    /// Upper bound for the reply — for reasoning models (gpt-oss) this
    /// includes the hidden reasoning, so leave headroom.
    pub max_tokens: u32,
    /// Ask for a single JSON object (`response_format: json_object`).
    pub json: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_tokens: 1024,
            json: false,
        }
    }
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    /// Only sent to reasoning models: keeps their (billed, slow) thinking
    /// short — classification and short answers need little of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
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
    #[serde(default)]
    content: Option<String>,
}

/// Models that think before answering and accept `reasoning_effort`.
fn is_reasoning_model(model: &str) -> bool {
    model.starts_with("openai/gpt-oss") || model.starts_with("qwen/qwen3")
}

/// POST a chat completion to Groq, retrying transient failures (network
/// errors, rate limits, 5xx, empty content) with exponential backoff.
/// Returns `None` — after logging why — on permanent errors (bad key,
/// unknown model, invalid request) or once `MAX_ATTEMPTS` is exhausted;
/// callers decide how to degrade (skip the explanation, fall back to another
/// question source, …).
pub async fn complete(
    api_key: &str,
    model: &str,
    system_prompt: &str,
    user_content: &str,
    options: Options,
) -> Option<String> {
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let delay = 2u64.pow(attempt - 2); // 1 s, 2 s
            warn!("Groq: retry {attempt}/{MAX_ATTEMPTS} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let body = ApiRequest {
            model,
            max_tokens: options.max_tokens,
            messages: vec![
                ApiMessage {
                    role: "system",
                    content: system_prompt,
                },
                ApiMessage {
                    role: "user",
                    content: user_content,
                },
            ],
            response_format: options.json.then_some(ResponseFormat {
                kind: "json_object",
            }),
            reasoning_effort: is_reasoning_model(model).then_some("low"),
        };

        let resp = match CLIENT
            .post(API_URL)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Groq: request failed (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            error!("Groq: auth error {status} — check explainer.api_key in config");
            return None;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // Unknown model / malformed request: retrying cannot help.
            if status.as_u16() == 404 || body.contains("model_not_found") {
                error!(
                    "Groq: model {model:?} is not available — fix explainer.model in config \
                     (see https://console.groq.com/docs/models): {body}"
                );
                return None;
            }
            if status.as_u16() == 400 && !body.contains("json_validate_failed") {
                error!("Groq: request rejected ({status}): {body}");
                return None;
            }
            warn!("Groq: API error {status} (attempt {attempt}/{MAX_ATTEMPTS}): {body}");
            continue;
        }

        let data: ApiResponse = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                warn!("Groq: response parse error (attempt {attempt}/{MAX_ATTEMPTS}): {e}");
                continue;
            }
        };

        let content = data
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        if content.trim().is_empty() {
            warn!("Groq: empty content (attempt {attempt}/{MAX_ATTEMPTS}) — raise max_tokens?");
            continue;
        }

        return Some(content);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_is_only_sent_to_reasoning_models() {
        let body = |model| {
            serde_json::to_value(ApiRequest {
                model,
                max_tokens: 10,
                messages: vec![],
                response_format: Some(ResponseFormat {
                    kind: "json_object",
                }),
                reasoning_effort: is_reasoning_model(model).then_some("low"),
            })
            .unwrap()
        };
        assert_eq!(body("openai/gpt-oss-120b")["reasoning_effort"], "low");
        assert!(body("llama-3.3-70b-versatile")
            .get("reasoning_effort")
            .is_none());
        assert_eq!(
            body("llama-3.3-70b-versatile")["response_format"]["type"],
            "json_object"
        );
    }
}
