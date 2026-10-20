//! Anthropic Messages API request types, error bodies, and conversion of a
//! request into the prompt / system prompt handed to the Claude CLI.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub messages: Vec<InputMessage>,
    pub system: Option<Content>,
    #[serde(default)]
    pub stream: bool,
    pub output_config: Option<OutputConfig>,
}

#[derive(Debug, Deserialize)]
pub struct OutputConfig {
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: Content,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

/// An Anthropic-style error: `{"type":"error","error":{"type":..,"message":..}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self { status, kind, message: message.into() }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }
    pub fn body(&self) -> Value {
        json!({"type": "error", "error": {"type": self.kind, "message": self.message}})
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

/// Most images sent to the CLI in one request (the newest ones are kept).
const MAX_IMAGES: usize = 20;

fn check_image(block: &Value, path: &str) -> Result<(), ApiError> {
    let source = &block["source"];
    let ok = match source["type"].as_str() {
        Some("base64") => {
            matches!(source["media_type"].as_str(), Some("image/png" | "image/jpeg" | "image/gif" | "image/webp"))
                && source["data"].is_string()
        }
        Some("url") => source["url"].is_string(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "{path}: image source must be base64 (png, jpeg, gif or webp) or url"
        )))
    }
}

/// Flatten a content value to text. Images are collected into `images` (when allowed)
/// and replaced by an `[image #N]` marker; `thinking` blocks in replayed turns are skipped.
fn content_text(content: &Content, path: &str, mut images: Option<&mut Vec<Value>>) -> Result<String, ApiError> {
    match content {
        Content::Text(s) => Ok(s.clone()),
        Content::Blocks(blocks) => {
            let mut parts = Vec::new();
            for (i, block) in blocks.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(
                        block.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                    ),
                    Some("thinking") | Some("redacted_thinking") => {}
                    Some("image") if images.is_some() => {
                        check_image(block, &format!("{path}.{i}"))?;
                        let list = images.as_deref_mut().unwrap();
                        list.push(block.clone());
                        parts.push(format!("[image #{}]", list.len()));
                    }
                    other => {
                        return Err(ApiError::invalid(format!(
                            "{path}.{i}: content block type {:?} is not supported by this gateway (text and image only)",
                            other.unwrap_or("<missing>")
                        )))
                    }
                }
            }
            Ok(parts.join("\n\n"))
        }
    }
}

pub fn system_text(req: &MessagesRequest) -> Result<Option<String>, ApiError> {
    req.system
        .as_ref()
        .map(|s| content_text(s, "system", None))
        .transpose()
        .map(|s| s.filter(|s| !s.trim().is_empty()))
}

/// What the CLI receives as its single user turn.
pub struct Prompt {
    pub text: String,
    /// Image blocks in the order the `[image #N]` markers appear.
    pub images: Vec<Value>,
    /// Marker number of `images[0]` (above 1 when the oldest images were dropped).
    pub first_image_number: usize,
}

/// Build the single user turn the CLI receives. The CLI only takes one turn, so
/// earlier turns are replayed as a transcript; images from every turn are attached
/// and referenced by number, so follow-up questions about them still work.
pub fn build_prompt(req: &MessagesRequest) -> Result<Prompt, ApiError> {
    let Some(last) = req.messages.last() else {
        return Err(ApiError::invalid("messages: at least one message is required"));
    };
    if last.role != "user" {
        return Err(ApiError::invalid(
            "messages: the final message must have role \"user\" (assistant prefill is not supported)",
        ));
    }
    let n = req.messages.len();
    let mut images = Vec::new();

    let mut transcript = String::new();
    for (i, m) in req.messages[..n - 1].iter().enumerate() {
        let tag = match m.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            other => {
                return Err(ApiError::invalid(format!("messages.{i}.role: unknown role {other:?}")))
            }
        };
        let text = content_text(&m.content, &format!("messages.{i}.content"), Some(&mut images))?;
        transcript.push_str(&format!("<{tag}>\n{text}\n</{tag}>\n"));
    }
    let final_text = content_text(&last.content, &format!("messages.{}.content", n - 1), Some(&mut images))?;

    let mut text = if n == 1 {
        final_text
    } else {
        format!(
            "Earlier turns of this conversation are below. Treat them as history you already took \
             part in, then reply to the final user message.\n\n<conversation>\n{transcript}</conversation>\n\n\
             Final user message:\n{final_text}"
        )
    };
    let mut dropped = 0;
    if images.len() > MAX_IMAGES {
        dropped = images.len() - MAX_IMAGES;
        images.drain(..dropped);
        text = format!("(Images #1 to #{dropped} are no longer attached.)\n\n{text}");
    }
    Ok(Prompt { text, images, first_image_number: dropped + 1 })
}

pub fn models_list(ids: &[String]) -> Value {
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"type": "model", "id": id, "display_name": id, "created_at": "2026-01-01T00:00:00Z"}))
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": ids.first(),
        "last_id": ids.last(),
    })
}
