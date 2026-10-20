//! Runs one `claude -p` process per request and turns its `stream-json`
//! output into Messages API stream events.
//!
//! With `--include-partial-messages` the CLI already emits raw Messages API
//! events (`message_start`, `content_block_delta`, ...). In agent mode one
//! request can span several model turns, so the translator merges them into a
//! single message: one `message_start`, text/thinking blocks re-indexed, the
//! CLI's own tool_use blocks hidden, and one final `message_delta`.

use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};

use axum::http::StatusCode;
use futures::Stream;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Semaphore,
};

use crate::{
    api::{self, ApiError, MessagesRequest},
    config::Config,
};

pub type Event = Value;

pub async fn run(
    cfg: Arc<Config>,
    limiter: Arc<Semaphore>,
    req: MessagesRequest,
) -> Result<impl Stream<Item = Result<Event, ApiError>>, ApiError> {
    // The CLI only tells the model today's date, so add the host's clock. It goes in the
    // prompt, not the system prompt, so the cached system prefix stays identical.
    let now = chrono::Local::now().format("%A, %Y-%m-%d %H:%M:%S (UTC%:z)");
    let prompt = api::build_prompt(&req)?;
    // One stream-json user message: each image labelled with its number, then the text.
    let mut blocks = Vec::new();
    for (i, image) in prompt.images.iter().enumerate() {
        blocks.push(json!({"type": "text", "text": format!("Image #{}:", prompt.first_image_number + i)}));
        blocks.push(image.clone());
    }
    blocks.push(json!({"type": "text", "text": format!("[Current date and time: {now}]\n\n{}", prompt.text)}));
    let input_line = json!({"type": "user", "message": {"role": "user", "content": blocks}}).to_string() + "\n";
    let system = api::system_text(&req)?;
    let model = req.model.clone().unwrap_or_else(|| cfg.default_model.clone());

    let permit = limiter.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            format!("gateway is busy ({} concurrent requests max)", cfg.max_concurrent),
        )
    })?;

    // The system prompt goes through a temp file: argv entries are capped at 128 KiB.
    let system_file = match &system {
        Some(text) => {
            let f = tempfile::NamedTempFile::new().map_err(internal)?;
            std::fs::write(f.path(), text).map_err(internal)?;
            Some(f)
        }
        None => None,
    };

    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--disable-slash-commands",
        // Ignore the host user's personal settings and hooks; only workspace/ config applies.
        "--setting-sources",
        "project",
        "--model",
        &model,
    ]);
    if cfg.tools.is_empty() {
        // Plain chat: no tools, and the caller's system prompt replaces Claude Code's.
        cmd.args(["--tools", ""]);
        let path = match &system_file {
            Some(f) => f.path().to_path_buf(),
            None => cfg.default_system_file.clone(),
        };
        cmd.arg("--system-prompt-file").arg(path);
    } else {
        // Agent mode: keep Claude Code's prompt and pre-approve the configured tools.
        let tools = cfg.tools.join(",");
        cmd.args(["--tools", &tools, "--allowedTools", &tools]);
        cmd.args(["--max-turns", &cfg.max_turns.to_string()]);
        if let Some(f) = &system_file {
            cmd.arg("--append-system-prompt-file").arg(f.path());
        }
    }
    if let Some(effort) = req.output_config.as_ref().and_then(|o| o.effort.as_deref()) {
        cmd.args(["--effort", effort]);
    }
    if let Some(max) = req.max_tokens {
        cmd.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max.to_string());
    }
    cmd.current_dir(&cfg.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", format!("failed to start claude CLI: {e}"))
    })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(input_line.as_bytes()).await.map_err(internal)?;
    drop(stdin);

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.timeout_secs);

    Ok(async_stream::stream! {
        // Held for the life of the stream; dropping the stream kills the CLI.
        let _permit = permit;
        let _system_file = system_file;
        let mut child = child;
        let mut lines = BufReader::new(stdout).lines();
        let mut tr = Translator::default();

        loop {
            let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
                Err(_) => {
                    yield Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "timeout_error", "claude CLI timed out"));
                    return;
                }
                Ok(Err(e)) => { yield Err(internal(e)); return; }
                Ok(Ok(None)) => break,
                Ok(Ok(Some(line))) => line,
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match tr.feed(&v) {
                Ok(events) => for ev in events { yield Ok(ev) },
                Err(e) => { yield Err(e); return; }
            }
        }

        let status = child.wait().await;
        if !tr.started {
            let stderr = stderr_task.await.unwrap_or_default();
            let tail: String = stderr.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
            tracing::warn!(?status, %tail, "claude CLI produced no message");
            yield Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("claude CLI exited ({status:?}) without a response: {}", tail.trim()),
            ));
            return;
        }
        for ev in tr.finish() { yield Ok(ev) }
    })
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", e.to_string())
}

#[derive(Default)]
struct Translator {
    started: bool,
    finished: bool,
    next_index: u64,
    /// CLI block index (within the current model turn) -> our output index.
    /// `None` marks a hidden block (tool_use and friends).
    index_map: HashMap<u64, Option<u64>>,
    stop_reason: Option<Value>,
    output_tokens: u64,
}

impl Translator {
    fn feed(&mut self, line: &Value) -> Result<Vec<Event>, ApiError> {
        match line.get("type").and_then(Value::as_str) {
            Some("stream_event") => {}
            Some("result") => return self.on_result(line),
            _ => return Ok(vec![]),
        }
        // Events from subagents carry a parent_tool_use_id; only the main thread is the reply.
        if !line.get("parent_tool_use_id").map_or(true, Value::is_null) {
            return Ok(vec![]);
        }
        let Some(ev) = line.get("event") else { return Ok(vec![]) };
        let idx = ev.get("index").and_then(Value::as_u64);

        Ok(match ev.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.index_map.clear();
                if self.started {
                    vec![]
                } else {
                    self.started = true;
                    let mut ev = ev.clone();
                    ev["message"]["content"] = json!([]);
                    vec![ev]
                }
            }
            Some("content_block_start") => {
                let kind = ev["content_block"]["type"].as_str().unwrap_or_default();
                let visible = matches!(kind, "text" | "thinking" | "redacted_thinking");
                let out = visible.then(|| {
                    self.next_index += 1;
                    self.next_index - 1
                });
                self.index_map.insert(idx.unwrap_or_default(), out);
                out.map(|i| reindex(ev, i)).into_iter().collect()
            }
            Some("content_block_delta") | Some("content_block_stop") => idx
                .and_then(|i| self.index_map.get(&i).copied().flatten())
                .map(|out| reindex(ev, out))
                .into_iter()
                .collect(),
            Some("message_delta") => {
                if let Some(sr) = ev["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                    self.stop_reason = Some(sr.clone());
                }
                self.output_tokens += ev["usage"]["output_tokens"].as_u64().unwrap_or(0);
                vec![]
            }
            _ => vec![],
        })
    }

    fn on_result(&mut self, line: &Value) -> Result<Vec<Event>, ApiError> {
        let is_error = line.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        if is_error && !self.started {
            let msg = line
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("claude CLI reported an error")
                .to_string();
            let lower = msg.to_lowercase();
            return Err(if lower.contains("rate limit") || lower.contains("usage limit") {
                ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
            } else {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg)
            });
        }
        if let Some(sub) = line.get("subtype").and_then(Value::as_str).filter(|s| *s != "success") {
            tracing::warn!(subtype = sub, "claude CLI finished with a non-success result");
        }
        Ok(if self.started { self.finish() } else { vec![] })
    }

    fn finish(&mut self) -> Vec<Event> {
        if self.finished || !self.started {
            return vec![];
        }
        self.finished = true;
        vec![
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": self.stop_reason.clone().unwrap_or(json!("end_turn")), "stop_sequence": null},
                "usage": {"output_tokens": self.output_tokens},
            }),
            json!({"type": "message_stop"}),
        ]
    }
}

fn reindex(ev: &Value, index: u64) -> Event {
    let mut ev = ev.clone();
    ev["index"] = json!(index);
    ev
}

/// Fold stream events into a complete non-streaming Message object.
pub fn accumulate(events: Vec<Event>) -> Value {
    let mut message = json!({});
    let mut blocks: Vec<Value> = Vec::new();
    for ev in events {
        match ev["type"].as_str() {
            Some("message_start") => message = ev["message"].clone(),
            Some("content_block_start") => blocks.push(ev["content_block"].clone()),
            Some("content_block_delta") => {
                let Some(block) = ev["index"].as_u64().and_then(|i| blocks.get_mut(i as usize)) else { continue };
                let d = &ev["delta"];
                let append = |block: &mut Value, field: &str, piece: &Value| {
                    let cur = block[field].as_str().unwrap_or_default().to_string();
                    block[field] = json!(cur + piece.as_str().unwrap_or_default());
                };
                match d["type"].as_str() {
                    Some("text_delta") => append(block, "text", &d["text"]),
                    Some("thinking_delta") => append(block, "thinking", &d["thinking"]),
                    Some("signature_delta") => block["signature"] = d["signature"].clone(),
                    _ => {}
                }
            }
            Some("message_delta") => {
                message["stop_reason"] = ev["delta"]["stop_reason"].clone();
                message["stop_sequence"] = ev["delta"]["stop_sequence"].clone();
                message["usage"]["output_tokens"] = ev["usage"]["output_tokens"].clone();
            }
            _ => {}
        }
    }
    message["content"] = Value::Array(blocks);
    message
}
