use axum::extract::State;
use axum::response::Html;
use chrono::Utc;

use crate::render::escape_xml;

use super::{format_last_run, AppState};

const DASHBOARD_TEMPLATE: &str = include_str!("dashboard.html");

const TOKEN_BAR_HTML: &str = r#"<div class="token-bar">
  <label for="trigger-token">Trigger token</label>
  <input id="trigger-token" type="password" placeholder="Bearer token for manual actions" autocomplete="off"/>
  <button type="button" class="btn btn-secondary" onclick="saveToken()">Save</button>
  <button type="button" class="btn btn-secondary" onclick="clearToken()">Clear</button>
  <span id="token-status" class="muted"></span>
</div>"#;

pub(super) async fn dashboard(State(state): State<AppState>) -> Html<String> {
    let config = state.current_config();
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
            r#"<span class="badge running">running</span>"#
        } else if feed.enabled {
            r#"<span class="badge enabled">enabled</span>"#
        } else {
            r#"<span class="badge disabled">disabled</span>"#
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

    let html = DASHBOARD_TEMPLATE
        .replace("{{REQUIRES_TOKEN}}", &requires_token.to_string())
        .replace(
            "{{TOKEN_BAR}}",
            if requires_token { TOKEN_BAR_HTML } else { "" },
        )
        .replace("{{ROWS}}", &rows);

    Html(html)
}
