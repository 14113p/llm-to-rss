use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::json;

use crate::error::AppError;
use crate::render::{build_atom_feed, build_opml, build_rss_feed, escape_xml};
use crate::runner::{AppContext, RunOutcome};
use crate::store::Store;

pub type AppState = AppContext;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/opml.xml", get(opml))
        .route("/reload", post(reload_config))
        .route("/feeds/{id_ext}", get(serve_feed))
        .route("/feeds/{id}/run", post(trigger_feed))
        .with_state(state)
}

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

fn format_last_run(store: &Store, feed_id: &str) -> (String, String) {
    store
        .last_run(feed_id)
        .ok()
        .flatten()
        .map(|r| {
            let label = format!(
                "{} ({})",
                r.status,
                r.finished_at.format("%Y-%m-%d %H:%M UTC")
            );
            let detail = r.message.unwrap_or_default();
            (label, detail)
        })
        .unwrap_or_else(|| ("never".to_string(), String::new()))
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

async fn reload_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = state.config.current();
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
    let config = state.current_config();
    if !is_trigger_authorized(config.server.trigger_token.as_deref(), &headers) {
        return unauthorized_response();
    }

    let guarded = state.run_feed_guarded(&id, false).await;
    let outcome = match guarded {
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

async fn dashboard(State(state): State<AppState>) -> Html<String> {
    let config = state.config.current();
    let requires_token = config
        .server
        .trigger_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());

    let mut rows = String::new();
    for feed in &config.feeds {
        let count = state.store.count_entries(&feed.id).unwrap_or(0);
        let (last_run, last_run_detail) = format_last_run(&state.store, &feed.id);
        let is_running = state.runs.is_running(&feed.id);

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

        let status_badge = if is_running {
            "<span class=\"badge running\">running</span>"
        } else if feed.enabled {
            "<span class=\"badge enabled\">enabled</span>"
        } else {
            "<span class=\"badge disabled\">disabled</span>"
        };
        let category = feed.category.as_deref().unwrap_or("Virtual Feeds");
        let disabled_attr = if is_running { " disabled" } else { "" };
        let btn_label = if is_running { "Running..." } else { "Run now" };
        let escaped_id = escape_xml(&feed.id);

        rows.push_str(&format!(
            "<tr data-feed-row=\"{escaped_id}\">
                <td><strong>{}</strong><br/><code>{escaped_id}</code></td>
                <td>{}</td>
                <td><code>{}</code></td>
                <td data-state-cell=\"{escaped_id}\">{status_badge}</td>
                <td data-items-cell=\"{escaped_id}\">{count}</td>
                <td data-last-run-cell=\"{escaped_id}\" title=\"{}\">{}</td>
                <td>{}</td>
                <td>
                    <a href=\"/feeds/{escaped_id}.xml\">Atom</a> ·
                    <a href=\"/feeds/{escaped_id}.rss\">RSS</a>
                </td>
                <td class=\"actions\">
                    <button type=\"button\" class=\"btn\" data-feed-id=\"{escaped_id}\" onclick=\"runFeed(this)\"{disabled_attr}>{btn_label}</button>
                    <div class=\"run-status\" data-status-cell=\"{escaped_id}\"></div>
                </td>
            </tr>",
            escape_xml(&feed.title),
            escape_xml(category),
            escape_xml(&feed.schedule),
            escape_xml(&last_run_detail),
            escape_xml(&last_run),
            escape_xml(&next_run),
        ));
    }

    let token_bar = if requires_token {
        "<div class=\"token-bar\">
           <label for=\"trigger-token\">Trigger token</label>
           <input id=\"trigger-token\" type=\"password\" placeholder=\"Bearer token for manual actions\" autocomplete=\"off\"/>
           <button type=\"button\" class=\"btn btn-secondary\" onclick=\"saveToken()\">Save</button>
           <button type=\"button\" class=\"btn btn-secondary\" onclick=\"clearToken()\">Clear</button>
           <span id=\"token-status\" class=\"muted\"></span>
         </div>"
    } else {
        ""
    };

    let html = format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\"/>
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"/>
<title>llm-to-rss</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 1180px; margin: 2rem auto; padding: 0 1rem; color: #1f2328; }}
  h1 {{ margin-bottom: .25rem; }}
  .sub {{ color: #57606a; margin-bottom: 1rem; display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; }}
  .token-bar {{ display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; margin-bottom: 1rem; padding: 0.65rem 0.85rem; background: #f6f8fa; border: 1px solid #d0d7de; border-radius: 6px; font-size: 0.88rem; }}
  .token-bar input {{ padding: 0.3rem 0.5rem; border: 1px solid #d0d7de; border-radius: 4px; min-width: 240px; font-size: 0.88rem; }}
  .muted {{ color: #57606a; font-size: 0.85rem; }}
  #global-status {{ margin-bottom: 0.75rem; font-size: 0.88rem; min-height: 1.2em; }}
  table {{ width: 100%; border-collapse: collapse; font-size: 0.92rem; }}
  th, td {{ border-bottom: 1px solid #d0d7de; padding: 0.65rem 0.5rem; text-align: left; vertical-align: top; }}
  th {{ background: #f6f8fa; font-weight: 600; }}
  code {{ background: #f6f8fa; padding: 0.12rem 0.3rem; border-radius: 4px; }}
  a {{ color: #0969da; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  .badge {{ display: inline-block; padding: 0.12rem 0.45rem; border-radius: 999px; font-size: 0.78rem; font-weight: 600; }}
  .badge.enabled {{ background: #dafbe1; color: #116329; }}
  .badge.disabled {{ background: #eaeef2; color: #57606a; }}
  .badge.running {{ background: #ddf4ff; color: #0969da; }}
  .btn {{ background: #1f883d; color: #fff; border: 1px solid rgba(31,35,40,.15); border-radius: 6px; padding: 0.32rem 0.7rem; font-size: 0.84rem; font-weight: 500; cursor: pointer; white-space: nowrap; }}
  .btn:hover:not(:disabled) {{ background: #1a7f37; }}
  .btn:disabled {{ opacity: 0.6; cursor: wait; }}
  .btn-secondary {{ background: #f6f8fa; color: #24292f; border-color: #d0d7de; }}
  .btn-secondary:hover:not(:disabled) {{ background: #eaeef2; }}
  .actions {{ min-width: 150px; }}
  .run-status {{ margin-top: 0.35rem; font-size: 0.8rem; line-height: 1.3; max-width: 240px; word-break: break-word; }}
  .status-ok {{ color: #116329; }}
  .status-skip {{ color: #9a6700; }}
  .status-err {{ color: #cf222e; }}
</style>
</head>
<body data-requires-token=\"{requires_token}\">
  <h1>llm-to-rss</h1>
  <div class=\"sub\">
    <span>Virtual LLM-powered Atom &amp; RSS feeds</span>
    <span>·</span>
    <a href=\"/opml.xml\">Download OPML</a>
    <span>·</span>
    <a href=\"/healthz\">Health JSON</a>
    <span>·</span>
    <button type=\"button\" class=\"btn btn-secondary\" id=\"reload-btn\" onclick=\"reloadConfig(this)\">Reload config</button>
  </div>
  {token_bar}
  <div id=\"global-status\"></div>
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
        <th>Actions</th>
      </tr>
    </thead>
    <tbody>{rows}</tbody>
  </table>
<script>
const TOKEN_KEY = 'llm_to_rss_trigger_token';
const requiresToken = document.body.dataset.requiresToken === 'true';

function getToken() {{
  const input = document.getElementById('trigger-token');
  if (input && input.value.trim()) {{
    return input.value.trim();
  }}
  return sessionStorage.getItem(TOKEN_KEY) || '';
}}

function saveToken() {{
  const input = document.getElementById('trigger-token');
  const status = document.getElementById('token-status');
  if (!input) return;
  const val = input.value.trim();
  if (val) {{
    sessionStorage.setItem(TOKEN_KEY, val);
    if (status) status.textContent = 'Saved for this browser session.';
  }}
}}

function clearToken() {{
  sessionStorage.removeItem(TOKEN_KEY);
  const input = document.getElementById('trigger-token');
  const status = document.getElementById('token-status');
  if (input) input.value = '';
  if (status) status.textContent = 'Cleared.';
}}

if (requiresToken) {{
  const saved = sessionStorage.getItem(TOKEN_KEY);
  const input = document.getElementById('trigger-token');
  const status = document.getElementById('token-status');
  if (saved && input) {{
    input.value = saved;
    if (status) status.textContent = 'Loaded from session.';
  }}
}}

function authHeaders() {{
  const headers = {{}};
  if (requiresToken) {{
    const token = getToken();
    if (token) {{
      sessionStorage.setItem(TOKEN_KEY, token);
      headers['Authorization'] = 'Bearer ' + token;
    }}
  }}
  return headers;
}}

async function runFeed(btn) {{
  const feedId = btn.dataset.feedId;
  const statusEl = document.querySelector('[data-status-cell=\"' + CSS.escape(feedId) + '\"]');
  const itemsEl = document.querySelector('[data-items-cell=\"' + CSS.escape(feedId) + '\"]');
  const lastRunEl = document.querySelector('[data-last-run-cell=\"' + CSS.escape(feedId) + '\"]');

  btn.disabled = true;
  btn.textContent = 'Running...';
  if (statusEl) {{
    statusEl.className = 'run-status muted';
    statusEl.textContent = 'Executing feed...';
  }}

  try {{
    const resp = await fetch('/feeds/' + encodeURIComponent(feedId) + '/run', {{
      method: 'POST',
      headers: authHeaders(),
    }});
    const data = await resp.json().catch(() => ({{}}));

    if (typeof data.items === 'number' && itemsEl) {{
      itemsEl.textContent = String(data.items);
    }}
    if (typeof data.last_run === 'string' && lastRunEl) {{
      lastRunEl.textContent = data.last_run;
      if (typeof data.last_run_detail === 'string') {{
        lastRunEl.title = data.last_run_detail;
      }}
    }}

    if (!statusEl) return;
    if (resp.ok && data.status === 'published') {{
      statusEl.className = 'run-status status-ok';
      statusEl.textContent = 'Published: ' + (data.title || 'ok');
    }} else if (resp.ok && data.status === 'skipped') {{
      statusEl.className = 'run-status status-skip';
      statusEl.textContent = 'Skipped: ' + (data.reason || 'no change');
    }} else {{
      statusEl.className = 'run-status status-err';
      statusEl.textContent = 'Error (' + resp.status + '): ' + (data.error || resp.statusText);
    }}
  }} catch (err) {{
    if (statusEl) {{
      statusEl.className = 'run-status status-err';
      statusEl.textContent = 'Request failed: ' + err.message;
    }}
  }} finally {{
    btn.disabled = false;
    btn.textContent = 'Run now';
  }}
}}

async function reloadConfig(btn) {{
  const globalStatus = document.getElementById('global-status');
  btn.disabled = true;
  btn.textContent = 'Reloading...';
  if (globalStatus) {{
    globalStatus.className = 'muted';
    globalStatus.textContent = 'Reloading configuration from disk...';
  }}
  try {{
    const resp = await fetch('/reload', {{
      method: 'POST',
      headers: authHeaders(),
    }});
    const data = await resp.json().catch(() => ({{}}));
    if (resp.ok) {{
      if (globalStatus) {{
        globalStatus.className = 'status-ok';
        globalStatus.textContent = 'Reloaded (' + data.feeds_total + ' feeds, ' + data.feeds_enabled + ' enabled). Refreshing...';
      }}
      window.location.reload();
    }} else if (globalStatus) {{
      globalStatus.className = 'status-err';
      globalStatus.textContent = 'Reload failed (' + resp.status + '): ' + (data.error || resp.statusText);
    }}
  }} catch (err) {{
    if (globalStatus) {{
      globalStatus.className = 'status-err';
      globalStatus.textContent = 'Reload request failed: ' + err.message;
    }}
  }} finally {{
    btn.disabled = false;
    btn.textContent = 'Reload config';
  }}
}}
</script>
</body>
</html>"
    );

    Html(html)
}
