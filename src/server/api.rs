use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::error::AppError;
use crate::runner::RunOutcome;

use super::{format_last_run, AppState};

fn is_trigger_authorized(trigger_token: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = trigger_token.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };

    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    provided == Some(expected)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized (missing or invalid trigger token)" })),
    )
        .into_response()
}

pub(super) async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.current_config();
    let enabled = config.feeds.iter().filter(|f| f.enabled).count();
    Json(json!({
        "status": "ok",
        "feeds_total": config.feeds.len(),
        "feeds_enabled": enabled,
    }))
}

pub(super) async fn reload_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = state.current_config();
    if !is_trigger_authorized(config.server.trigger_token.as_deref(), &headers) {
        return unauthorized_response();
    }

    match state.config.reload_from_disk() {
        Ok(loaded) => {
            let enabled = loaded.feeds.iter().filter(|f| f.enabled).count();
            (
                StatusCode::OK,
                Json(json!({
                    "status": "reloaded",
                    "feeds_total": loaded.feeds.len(),
                    "feeds_enabled": enabled,
                })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "error": err.to_string(),
            })),
        )
            .into_response(),
    }
}

pub(super) async fn trigger_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let config = state.current_config();
    if !is_trigger_authorized(config.server.trigger_token.as_deref(), &headers) {
        return unauthorized_response();
    }

    let outcome = match state.run_feed_guarded(&id, false).await {
        Ok(Some(outcome)) => Ok(outcome),
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "running",
                    "error": format!("feed '{id}' is already running"),
                })),
            )
                .into_response();
        }
        Err(AppError::FeedNotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("feed '{id}' not found") })),
            )
                .into_response();
        }
        Err(err) => Err(err),
    };

    let items = state.store.count_entries(&id).unwrap_or(0);
    let (last_run, last_run_detail) = format_last_run(&state.store, &id);

    match outcome {
        Ok(RunOutcome::Published(entry)) => (
            StatusCode::OK,
            Json(json!({
                "status": "published",
                "entry_id": entry.id,
                "title": entry.title,
                "items": items,
                "last_run": last_run,
                "last_run_detail": last_run_detail,
            })),
        )
            .into_response(),
        Ok(RunOutcome::Skipped(reason)) => (
            StatusCode::OK,
            Json(json!({
                "status": "skipped",
                "reason": reason,
                "items": items,
                "last_run": last_run,
                "last_run_detail": last_run_detail,
            })),
        )
            .into_response(),
        Ok(RunOutcome::DryRun { .. }) => StatusCode::OK.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "error": err.to_string(),
                "items": items,
                "last_run": last_run,
                "last_run_detail": last_run_detail,
            })),
        )
            .into_response(),
    }
}
