//! claude-gateway: an HTTP server (run behind a TLS reverse proxy) that speaks the Anthropic Messages API
//! (`POST /v1/messages`, streaming and non-streaming) and answers each request
//! by running the local Claude CLI.

mod admin;
mod api;
mod cli;
mod config;
mod keys;

use std::{convert::Infallible, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Extension, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{sse::Event as SseEvent, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};

use futures::{stream, StreamExt};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use api::{ApiError, MessagesRequest};
use config::Config;

#[derive(Clone)]
pub struct AppState {
    cfg: Arc<Config>,
    limiter: Arc<Semaphore>,
    keys: Arc<keys::KeyStore>,
}

/// Name of the client key that authenticated the request (for logs).
#[derive(Clone)]
struct KeyName(String);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "claude_gateway=info".into()),
        )
        .init();

    let cfg = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };
    let keys = match keys::KeyStore::load(cfg.keys_file.clone()) {
        Ok(k) => Arc::new(k),
        Err(e) => {
            eprintln!("key store error: {e}");
            std::process::exit(2);
        }
    };
    // Usage counters are saved in the background rather than on every request.
    let flusher = keys.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            flusher.flush();
        }
    });
    let state = AppState { limiter: Arc::new(Semaphore::new(cfg.max_concurrent)), cfg: cfg.clone(), keys };

    let v1 = Router::new()
        // Images arrive base64-encoded inside the JSON body, so allow large requests.
        .route("/v1/messages", post(messages).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)))
        .route("/v1/models", get(models))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    let admin = admin::routes().route_layer(middleware::from_fn_with_state(state.clone(), admin::auth));
    let routes = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/admin", get(admin::page))
        .merge(v1)
        .merge(admin);
    // Also serve everything under GATEWAY_BASE_PATH (e.g. /claude/api), so a reverse proxy
    // can forward https://example.com/claude/api/... with or without stripping the prefix.
    let routes = match &cfg.base_path {
        Some(base) => routes.clone().nest(base, routes),
        None => routes,
    };
    let app = routes.layer(middleware::from_fn(request_id)).with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind).await.unwrap_or_else(|e| {
        eprintln!("bind {}: {e}", cfg.bind);
        std::process::exit(1);
    });
    let mode = if cfg.tools.is_empty() { "chat".to_string() } else { format!("agent (tools: {})", cfg.tools.join(",")) };
    // Plain HTTP: TLS is terminated by the reverse proxy (nginx) in front.
    tracing::info!("listening on http://{}{} — mode: {mode}", cfg.bind, cfg.base_path.as_deref().unwrap_or(""));
    axum::serve(listener, app).await.expect("server error");
}

async fn request_id(req: Request, next: Next) -> Response {
    let id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert("request-id", v);
    }
    resp
}

/// The key from `x-api-key: <key>` or `Authorization: Bearer <key>`, like the real API.
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")))
        .map(str::trim)
}

/// Client auth: a key created through the admin API, or one listed in GATEWAY_API_KEYS.
async fn auth(State(st): State<AppState>, headers: HeaderMap, mut req: Request, next: Next) -> Response {
    let Some(presented) = presented_key(&headers).filter(|p| !p.is_empty()) else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing API key: send it as an `x-api-key` header or `Authorization: Bearer <key>`",
        )
        .into_response();
    };
    let static_ok =
        st.cfg.api_keys.iter().fold(false, |acc, k| acc | bool::from(k.as_bytes().ct_eq(presented.as_bytes())));
    let name = if static_ok { Some(".env key".to_string()) } else { st.keys.authenticate(presented) };
    let Some(name) = name else {
        tracing::warn!("rejected request with an invalid or disabled API key");
        return ApiError::new(StatusCode::UNAUTHORIZED, "authentication_error", "invalid or disabled API key")
            .into_response();
    };
    req.extensions_mut().insert(KeyName(name));
    next.run(req).await
}

async fn models(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(api::models_list(&st.cfg.models))
}

async fn messages(State(st): State<AppState>, Extension(KeyName(key)): Extension<KeyName>, body: Bytes) -> Response {
    let req: MessagesRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return ApiError::invalid(format!("invalid request body: {e}")).into_response(),
    };
    let streaming = req.stream;
    tracing::info!(%key, model = ?req.model, streaming, turns = req.messages.len(), "messages request");

    let events = match cli::run(st.cfg.clone(), st.limiter.clone(), req).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut events = Box::pin(events);

    // Wait for the first event so startup failures still get a proper HTTP status.
    let first = match events.next().await {
        Some(Ok(ev)) => ev,
        Some(Err(e)) => return e.into_response(),
        None => {
            return ApiError::new(StatusCode::BAD_GATEWAY, "api_error", "empty response from claude CLI").into_response()
        }
    };

    if streaming {
        let sse = stream::once(async { Ok(first) }).chain(events).map(|item| {
            Ok::<_, Infallible>(match item {
                Ok(ev) => SseEvent::default().event(ev["type"].as_str().unwrap_or("message")).data(ev.to_string()),
                Err(e) => SseEvent::default().event("error").data(e.body().to_string()),
            })
        });
        let mut resp = Sse::new(sse).into_response();
        // Tell nginx not to buffer the event stream.
        resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
        return resp;
    }

    let mut all = vec![first];
    while let Some(item) = events.next().await {
        match item {
            Ok(ev) => all.push(ev),
            Err(e) => return e.into_response(),
        }
    }
    Json(cli::accumulate(all)).into_response()
}
