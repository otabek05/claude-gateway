//! Admin panel (GET /admin, a static page) and admin API for client keys.
//! Every /admin/keys route needs `x-api-key: <GATEWAY_ADMIN_KEY>`.
//!
//!   POST   /admin/keys          {"name": "..."}  -> the new key (shown only here)
//!   GET    /admin/keys                           -> all keys, without secrets
//!   GET    /admin/keys/{id}
//!   PATCH  /admin/keys/{id}     {"name"?: "...", "status"?: "active" | "disabled"}
//!   DELETE /admin/keys/{id}

use axum::{
    body::Bytes,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::{api::ApiError, presented_key, AppState};

/// The admin panel page. It holds no secrets: it asks for the admin key and calls the API.
pub async fn page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        include_str!("admin.html"),
    )
        .into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/keys", get(list).post(create))
        .route("/admin/keys/{id}", get(get_one).patch(update).delete(remove))
}

/// Admin auth. Without GATEWAY_ADMIN_KEY the admin API doesn't exist.
pub async fn auth(State(st): State<AppState>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let Some(admin_key) = &st.cfg.admin_key else {
        return ApiError::new(StatusCode::NOT_FOUND, "not_found_error", "admin API is disabled (set GATEWAY_ADMIN_KEY)")
            .into_response();
    };
    let ok = presented_key(&headers).is_some_and(|p| bool::from(admin_key.as_bytes().ct_eq(p.as_bytes())));
    if !ok {
        return ApiError::new(StatusCode::UNAUTHORIZED, "authentication_error", "invalid admin key").into_response();
    }
    next.run(req).await
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found_error", format!("no key with id {id:?}"))
}

fn storage_error(e: String) -> ApiError {
    tracing::error!("key store: {e}");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "could not save the key store")
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::invalid(format!("invalid request body: {e}")))
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(ApiError::invalid("name: must be 1-100 characters"));
    }
    Ok(name.to_string())
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
}

async fn create(State(st): State<AppState>, body: Bytes) -> Result<(StatusCode, Json<Value>), ApiError> {
    let name = valid_name(&parse::<CreateBody>(&body)?.name)?;
    let (record, secret) = st.keys.create(name).map_err(storage_error)?;
    tracing::info!(id = %record.id, name = %record.name, "API key created");
    let mut view = record.view();
    view["key"] = json!(secret);
    Ok((StatusCode::CREATED, Json(view)))
}

async fn list(State(st): State<AppState>) -> Json<Value> {
    let data: Vec<Value> = st.keys.list().iter().map(|k| k.view()).collect();
    Json(json!({"data": data, "has_more": false}))
}

async fn get_one(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    st.keys.get(&id).map(|k| Json(k.view())).ok_or_else(|| not_found(&id))
}

#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    status: Option<String>,
}

async fn update(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let body: UpdateBody = parse(&body)?;
    let name = body.name.as_deref().map(valid_name).transpose()?;
    let disabled = match body.status.as_deref() {
        None => None,
        Some("active") => Some(false),
        Some("disabled") => Some(true),
        Some(other) => return Err(ApiError::invalid(format!("status: expected \"active\" or \"disabled\", got {other:?}"))),
    };
    let record = st.keys.update(&id, name, disabled).map_err(storage_error)?.ok_or_else(|| not_found(&id))?;
    tracing::info!(id = %record.id, name = %record.name, disabled = record.disabled, "API key updated");
    Ok(Json(record.view()))
}

async fn remove(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    if !st.keys.delete(&id).map_err(storage_error)? {
        return Err(not_found(&id));
    }
    tracing::info!(id = %id, "API key deleted");
    Ok(Json(json!({"id": id, "type": "api_key_deleted"})))
}
