use serde::{Deserialize, Serialize};
use tracing::warn;

const SYSTEM_PROMPT: &str = "\
You are a trivia explanation assistant for a Matrix chat quiz bot.

After each question you receive the question text and its correct answer (sourced from OpenTDB).
Your job is to help players understand and remember the answer.

Output two sections, each a short paragraph:

Explanation:
Why the answer is correct. Aim for 2–3 sentences, but use more if the topic genuinely needs it.

Background:
A memorable fact, historical context, or related concept that enriches the answer.

Behavior rules:
- Do not restate the question or repeat the answer word-for-word at the start.
- Treat the provided answer as correct unless it is clearly and unambiguously wrong — in that case, note the issue briefly at the end.
- No filler, no padding, no \"great question\" openers.

Style:
- Educational and clear, like a good textbook footnote.
- No emojis, no bullet points inside the sections, no markdown headers.";

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

// ── Public interface ──────────────────────────────────────────────────────────

/// Call the Groq chat-completions API to generate a quiz explanation.
/// Returns `None` if the request fails or the response contains no text.
pub async fn explain(
    question: &str,
    answer:   &str,
    api_key:  &str,
    model:    &str,
) -> Option<String> {
    let client       = reqwest::Client::new();
    let user_content = format!("Question: {question}\nProvided Answer: {answer}");

    let body = ApiRequest {
        model,
        max_tokens: 1024,
        messages: vec![
            ApiMessage { role: "system", content: SYSTEM_PROMPT },
            ApiMessage { role: "user",   content: &user_content },
        ],
    };

    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| warn!("Explainer request failed: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text   = resp.text().await.unwrap_or_default();
        warn!("Explainer API error {status}: {text}");
        return None;
    }

    let data: ApiResponse = resp.json()
        .await
        .map_err(|e| warn!("Explainer response parse error: {e}"))
        .ok()?;

    data.choices.into_iter().next().map(|c| c.message.content)
}
