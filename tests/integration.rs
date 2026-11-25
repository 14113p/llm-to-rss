use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Client;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;

use llm_to_rss::config::{
    AppConfig, DefaultsConfig, FeedConfig, ProviderConfig, ProviderKind, ServerConfig,
    SourceConfig, StorageConfig,
};
use llm_to_rss::runner::{run_feed, RunOutcome};
use llm_to_rss::server::{router, AppState};
use llm_to_rss::store::Store;

async fn start_mock_upstream() -> String {
    let app = Router::new()
        .route(
            "/source.rss",
            get(|| async {
                r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Mock Tech News</title>
    <link>https://example.com</link>
    <description>Mock feed</description>
    <item>
      <title>Rust 1.100 Released</title>
      <link>https://example.com/rust</link>
      <description>Big release today.</description>
    </item>
  </channel>
</rss>"#
            }),
        )
        .route(
            "/v1/responses",
            post(|Json(body): Json<Value>| async move {
                let prompt = body["input"].as_str().unwrap_or("");
                assert!(prompt.contains("Rust 1.100 Released"));
                Json(json!({
                    "id": "resp_1",
                    "model": "gpt-4o",
                    "output": [{
                        "type": "message",
                        "content": [{
                            "type": "output_text",
                            "text": "# Morning Tech Briefing\n\n- Rust 1.100 shipped today.",
                            "annotations": [{
                                "type": "url_citation",
                                "url": "https://example.com/rust",
                                "title": "Rust Release Notes"
                            }]
                        }]
                    }]
                }))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|| async {
                Json(json!({
                    "model": "llama3.2",
                    "choices": [{
                        "message": {
                            "content": "NO_UPDATE"
                        }
                    }]
                }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn end_to_end_fetch_llm_store_and_http_serve() {
    let mock_base = start_mock_upstream().await;
    let dir = tempdir().unwrap();

    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            kind: ProviderKind::OpenaiResponses,
            model: "gpt-4o".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: Some(format!("{mock_base}/v1")),
            web_search: true,
            temperature: None,
            max_tokens: None,
            extra_headers: BTreeMap::new(),
        },
    );
    providers.insert(
        "local".to_string(),
        ProviderConfig {
            kind: ProviderKind::OpenaiCompatible,
            model: "llama3.2".to_string(),
            api_key: None,
            base_url: Some(format!("{mock_base}/v1")),
            web_search: false,
            temperature: None,
            max_tokens: None,
            extra_headers: BTreeMap::new(),
        },
    );

    let briefing_feed = FeedConfig {
        id: "morning-briefing".to_string(),
        title: "Morning Briefing".to_string(),
        description: Some("Daily summary".to_string()),
        category: Some("Daily".to_string()),
        enabled: true,
        schedule: "0 7 * * *".to_string(),
        timezone: Some("UTC".to_string()),
        provider: "openai".to_string(),
        model: None,
        web_search: None,
        system_prompt: None,
        prompt: "Summarize today's highlights:\n\n{{sources}}".to_string(),
        entry_title_template: None,
        sources: vec![SourceConfig::Rss {
            name: "Mock Tech".to_string(),
            url: format!("{mock_base}/source.rss"),
            limit: Some(5),
        }],
        skip_if_contains: None,
        deduplicate: true,
        max_items: Some(10),
        run_on_startup: false,
    };

    let watcher_feed = FeedConfig {
        id: "ps5-watch".to_string(),
        title: "PS5 Watch".to_string(),
        description: None,
        category: Some("Deals".to_string()),
        enabled: true,
        schedule: "0 8 * * *".to_string(),
        timezone: None,
        provider: "local".to_string(),
        model: None,
        web_search: None,
        system_prompt: None,
        prompt: "Check PS5 price".to_string(),
        entry_title_template: None,
        sources: vec![],
        skip_if_contains: Some("NO_UPDATE".to_string()),
        deduplicate: false,
        max_items: None,
        run_on_startup: false,
    };

    let config = Arc::new(AppConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            base_url: "http://localhost:8080".to_string(),
            trigger_token: Some("secret-token".to_string()),
        },
        storage: StorageConfig {
            db_path: dir.path().join("feeds.db"),
        },
        defaults: DefaultsConfig::default(),
        feeds_dir: None,
        providers,
        feeds: vec![briefing_feed.clone(), watcher_feed.clone()],
    });
    config.validate().unwrap();

    let client = Client::builder().no_proxy().build().unwrap();
    let store = Store::open(&config.storage.db_path).unwrap();

    // 1. Run briefing feed -> should publish
    let outcome = run_feed(&config, &briefing_feed, &client, &store, false)
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Published(_)));

    // 2. Run again with deduplicate=true and same output -> should skip
    let outcome2 = run_feed(&config, &briefing_feed, &client, &store, false)
        .await
        .unwrap();
    assert!(matches!(outcome2, RunOutcome::Skipped(_)));

    // 3. Run watcher feed -> returns NO_UPDATE -> should skip
    let outcome3 = run_feed(&config, &watcher_feed, &client, &store, false)
        .await
        .unwrap();
    assert!(matches!(outcome3, RunOutcome::Skipped(_)));

    // 4. Verify HTTP endpoints
    let state = AppState {
        config: Arc::clone(&config),
        client,
        store,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let http = Client::builder().no_proxy().build().unwrap();
    let atom_resp = http
        .get(format!("http://{server_addr}/feeds/morning-briefing.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(atom_resp.status(), StatusCode::OK);
    let atom_body = atom_resp.text().await.unwrap();
    assert!(atom_body.contains("Morning Tech Briefing"));
    assert!(atom_body.contains("https://example.com/rust"));

    let rss_resp = http
        .get(format!("http://{server_addr}/feeds/morning-briefing.rss"))
        .send()
        .await
        .unwrap();
    assert_eq!(rss_resp.status(), StatusCode::OK);
    assert!(rss_resp
        .text()
        .await
        .unwrap()
        .contains("Morning Tech Briefing"));

    let opml_resp = http
        .get(format!("http://{server_addr}/opml.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(opml_resp.status(), StatusCode::OK);
    let opml_body = opml_resp.text().await.unwrap();
    assert!(opml_body.contains("morning-briefing.xml"));
    assert!(opml_body.contains("ps5-watch.xml"));

    // Auth check on trigger endpoint
    let unauth = http
        .post(format!("http://{server_addr}/feeds/morning-briefing/run"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let auth = http
        .post(format!("http://{server_addr}/feeds/morning-briefing/run"))
        .bearer_auth("secret-token")
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), StatusCode::OK);
}
