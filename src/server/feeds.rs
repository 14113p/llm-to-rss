use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::render::{build_atom_feed, build_opml, build_rss_feed};

use super::AppState;

pub(super) async fn opml(State(state): State<AppState>) -> Response {
    let config = state.current_config();
    let xml = build_opml(&config.feeds, &config.server.base_url);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/x-opml; charset=utf-8")],
        xml,
    )
        .into_response()
}

pub(super) async fn serve_feed(
    State(state): State<AppState>,
    Path(id_ext): Path<String>,
) -> Response {
    let config = state.current_config();
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
