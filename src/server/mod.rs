mod api;
mod dashboard;
mod feeds;

use axum::routing::{get, post};
use axum::Router;

use crate::runner::AppContext;
use crate::store::Store;

pub type AppState = AppContext;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard::dashboard))
        .route("/healthz", get(api::healthz))
        .route("/opml.xml", get(feeds::opml))
        .route("/reload", post(api::reload_config))
        .route("/feeds/{id_ext}", get(feeds::serve_feed))
        .route("/feeds/{id}/run", post(api::trigger_feed))
        .with_state(state)
}

pub(crate) fn format_last_run(store: &Store, feed_id: &str) -> (String, String) {
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
