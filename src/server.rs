use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use reqwest::Client;
use serde_json::json;

use crate::reload::ConfigHandle;
use crate::render::{build_atom_feed, build_opml, build_rss_feed, escape_xml};
use crate::runner::{run_feed, RunOutcome};
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub config: ConfigHandle,
    pub client: Client,
    pub store: Store,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/opml.xml", get(opml))
        .route("/feeds/{id_ext}", get(serve_feed))
        .route("/feeds/{id}/run", post(trigger_feed))
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.current();
    let enabled = config.feeds.iter().filter(|f| f.enabled).count();
    Json(json!({
        "status": "ok",
        "feeds_total": config.feeds.len(),
        "feeds_enabled": enabled,
    }))
}

async fn opml(State(state): State<AppState>) -> Response {
    let config = state.config.current();
    let xml = build_opml(&config.feeds, &config.server.base_url);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/x-opml; charset=utf-8")],
        xml,
    )
        .into_response()
}

async fn serve_feed(State(state): State<AppState>, Path(id_ext): Path<String>) -> Response {
    let config = state.config.current();
    let (id, is_rss) = if let Some(stem) = id_ext.strip_suffix(".xml") {
        (stem, false)
    } else if let Some(stem) = id_ext.strip_suffix(".atom") {
        (stem, false)
    } else if let Some(stem) = id_ext.strip_suffix(".rss") {
        (stem, true)
    } else {
        (id_ext.as_str(), false)
    };

    let Ok(feed) = config.find_feed(id) else {
        return (StatusCode::NOT_FOUND, format!("feed '{id}' not found")).into_response();
    };

    let limit = feed.effective_max_items(&config.defaults);
    let entries = match state.store.list_entries(&feed.id, limit) {
        Ok(e) => e,
        Err(err) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response();
        }
    };

    let base_url = &config.server.base_url;
    if is_rss {
        match build_rss_feed(feed, base_url, &entries) {
            Ok(xml) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    } else {
        match build_atom_feed(feed, base_url, &entries) {
            Ok(xml) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
                xml,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
}

async fn trigger_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let config = state.config.current();
    let Some(expected) = config.server.trigger_token.as_deref() else {
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({ "error": "manual HTTP triggers are disabled (set server.trigger_token)" }),
            ),
        )
            .into_response();
    };

    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if provided != Some(expected) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    }

    let Ok(feed) = config.find_feed(&id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("feed '{id}' not found") })),
        )
            .into_response();
    };

    match run_feed(&config, &feed, &state.client, &state.store, false).await {
        Ok(RunOutcome::Published(entry)) => (
            StatusCode::OK,
            Json(json!({ "status": "published", "entry_id": entry.id, "title": entry.title })),
        )
            .into_response(),
        Ok(RunOutcome::Skipped(reason)) => (
            StatusCode::OK,
            Json(json!({ "status": "skipped", "reason": reason })),
        )
            .into_response(),
        Ok(RunOutcome::DryRun { .. }) => StatusCode::OK.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error", "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn dashboard(State(state): State<AppState>) -> Html<String> {
    let config = state.config.current();
    let mut rows = String::new();
    for feed in &config.feeds {
        let count = state.store.count_entries(&feed.id).unwrap_or(0);
        let last_run = state
            .store
            .last_run(&feed.id)
            .ok()
            .flatten()
            .map(|r| {
                format!(
                    "{} ({})",
                    r.status,
                    r.finished_at.format("%Y-%m-%d %H:%M UTC")
                )
            })
            .unwrap_or_else(|| "never".to_string());

        let next_run = feed
            .parsed_schedule()
            .ok()
            .zip(feed.effective_timezone(&config.defaults).ok())
            .and_then(|(sched, tz)| {
                let now = Utc::now().with_timezone(&tz);
                sched
                    .after(&now)
                    .next()
                    .map(|n| n.format("%Y-%m-%d %H:%M %Z").to_string())
            })
            .unwrap_or_else(|| "-".to_string());

        let status_badge = if feed.enabled { "enabled" } else { "disabled" };
        let category = feed.category.as_deref().unwrap_or("Virtual Feeds");

        rows.push_str(&format!(
            "<tr>
                <td><strong>{}</strong><br/><code>{}</code></td>
                <td>{}</td>
                <td><code>{}</code></td>
                <td>{}</td>
                <td>{}</td>
                <td>{}</td>
                <td>{}</td>
                <td>
                    <a href=\"/feeds/{}.xml\">Atom</a> ·
                    <a href=\"/feeds/{}.rss\">RSS</a>
                </td>
            </tr>",
            escape_xml(&feed.title),
            escape_xml(&feed.id),
            escape_xml(category),
            escape_xml(&feed.schedule),
            status_badge,
            count,
            escape_xml(&last_run),
            escape_xml(&next_run),
            escape_xml(&feed.id),
            escape_xml(&feed.id),
        ));
    }

    let html = format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\"/>
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"/>
<title>llm-to-rss</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 1080px; margin: 2rem auto; padding: 0 1rem; color: #1f2328; }}
  h1 {{ margin-bottom: .25rem; }}
  .sub {{ color: #57606a; margin-bottom: 1.5rem; }}
  table {{ width: 100%; border-collapse: collapse; font-size: 0.92rem; }}
  th, td {{ border-bottom: 1px solid #d0d7de; padding: 0.65rem 0.5rem; text-align: left; vertical-align: top; }}
  th {{ background: #f6f8fa; font-weight: 600; }}
  code {{ background: #f6f8fa; padding: 0.12rem 0.3rem; border-radius: 4px; }}
  a {{ color: #0969da; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
</style>
</head>
<body>
  <h1>llm-to-rss</h1>
  <p class=\"sub\">Virtual LLM-powered Atom &amp; RSS feeds · <a href=\"/opml.xml\">Download OPML</a> · <a href=\"/healthz\">Health JSON</a></p>
  <table>
    <thead>
      <tr>
        <th>Feed</th>
        <th>Category</th>
        <th>Cron</th>
        <th>State</th>
        <th>Items</th>
        <th>Last Run</th>
        <th>Next Run</th>
        <th>Subscribe</th>
      </tr>
    </thead>
    <tbody>{rows}</tbody>
  </table>
</body>
</html>"
    );

    Html(html)
}
