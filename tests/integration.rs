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
            only_new: false,
        }],
        skip_if_contains: None,
        skip_if_no_new_items: false,
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
        skip_if_no_new_items: false,
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

#[tokio::test]
async fn passes_previous_entry_into_subsequent_prompt() {
    let seen_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_clone = Arc::clone(&seen_prompts);
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_clone = Arc::clone(&call_count);

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let prompts = Arc::clone(&prompts_clone);
            let counter = Arc::clone(&count_clone);
            async move {
                let user_msg = body["messages"]
                    .as_array()
                    .and_then(|arr| arr.iter().find(|m| m["role"] == "user"))
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                prompts.lock().unwrap().push(user_msg);
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = if n == 0 {
                    "# PS5 Watch\n\nLowest price: 2100 PLN at Media Expert."
                } else {
                    "# PS5 Watch\n\nLowest price: 1900 PLN (down 200 PLN since last run)."
                };
                Json(json!({
                    "model": "llama3.2",
                    "choices": [{ "message": { "content": reply } }]
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = tempdir().unwrap();
    let mut providers = BTreeMap::new();
    providers.insert(
        "local".to_string(),
        ProviderConfig {
            kind: ProviderKind::OpenaiCompatible,
            model: "llama3.2".to_string(),
            api_key: None,
            base_url: Some(format!("http://{addr}/v1")),
            web_search: false,
            temperature: None,
            max_tokens: None,
            extra_headers: BTreeMap::new(),
        },
    );

    let feed = FeedConfig {
        id: "price-delta".to_string(),
        title: "Price Delta".to_string(),
        description: None,
        category: None,
        enabled: true,
        schedule: "0 8 * * *".to_string(),
        timezone: Some("UTC".to_string()),
        provider: "local".to_string(),
        model: None,
        web_search: None,
        system_prompt: None,
        prompt: "Check price.\n\nPrevious:\n{{previous_entry}}".to_string(),
        entry_title_template: None,
        sources: vec![],
        skip_if_contains: None,
        skip_if_no_new_items: false,
        deduplicate: false,
        max_items: Some(10),
        run_on_startup: false,
    };

    let config = AppConfig {
        server: ServerConfig::default(),
        storage: StorageConfig {
            db_path: dir.path().join("delta.db"),
        },
        defaults: DefaultsConfig::default(),
        feeds_dir: None,
        providers,
        feeds: vec![feed.clone()],
    };

    let client = Client::builder().no_proxy().build().unwrap();
    let store = Store::open(&config.storage.db_path).unwrap();

    assert!(matches!(
        run_feed(&config, &feed, &client, &store, false)
            .await
            .unwrap(),
        RunOutcome::Published(_)
    ));
    assert!(matches!(
        run_feed(&config, &feed, &client, &store, false)
            .await
            .unwrap(),
        RunOutcome::Published(_)
    ));

    let prompts = seen_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("(No previous entry yet.)"));
    assert!(prompts[1].contains("Lowest price: 2100 PLN at Media Expert."));
}

#[tokio::test]
async fn tracks_seen_rss_items_across_runs() {
    let rss_items = Arc::new(std::sync::Mutex::new(vec![(
        "guid-a".to_string(),
        "Article A".to_string(),
    )]));
    let rss_items_clone = Arc::clone(&rss_items);

    let seen_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_clone = Arc::clone(&seen_prompts);

    let app = Router::new()
        .route(
            "/dynamic.rss",
            get(move || {
                let items = rss_items_clone.lock().unwrap().clone();
                async move {
                    let mut body = String::from(
                        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Dyn</title><link>https://example.com</link><description>d</description>"#,
                    );
                    for (guid, title) in items {
                        body.push_str(&format!(
                            "<item><guid>{guid}</guid><title>{title}</title><link>https://example.com/{guid}</link></item>"
                        ));
                    }
                    body.push_str("</channel></rss>");
                    body
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let prompts = Arc::clone(&prompts_clone);
                async move {
                    let user_msg = body["messages"]
                        .as_array()
                        .and_then(|arr| arr.iter().find(|m| m["role"] == "user"))
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or("")
                        .to_string();
                    let n = {
                        let mut p = prompts.lock().unwrap();
                        p.push(user_msg);
                        p.len()
                    };
                    Json(json!({
                        "model": "llama3.2",
                        "choices": [{ "message": { "content": format!("# Digest {n}\n\nSummary {n}") } }]
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = tempdir().unwrap();
    let mut providers = BTreeMap::new();
    providers.insert(
        "local".to_string(),
        ProviderConfig {
            kind: ProviderKind::OpenaiCompatible,
            model: "llama3.2".to_string(),
            api_key: None,
            base_url: Some(format!("http://{addr}/v1")),
            web_search: false,
            temperature: None,
            max_tokens: None,
            extra_headers: BTreeMap::new(),
        },
    );

    let feed = FeedConfig {
        id: "rss-seen".to_string(),
        title: "RSS Seen Test".to_string(),
        description: None,
        category: None,
        enabled: true,
        schedule: "0 8 * * *".to_string(),
        timezone: Some("UTC".to_string()),
        provider: "local".to_string(),
        model: None,
        web_search: None,
        system_prompt: None,
        prompt: "Summarize:\n{{sources}}".to_string(),
        entry_title_template: None,
        sources: vec![SourceConfig::Rss {
            name: "Dyn".to_string(),
            url: format!("http://{addr}/dynamic.rss"),
            limit: Some(10),
            only_new: true,
        }],
        skip_if_contains: None,
        skip_if_no_new_items: true,
        deduplicate: false,
        max_items: Some(10),
        run_on_startup: false,
    };

    let config = AppConfig {
        server: ServerConfig::default(),
        storage: StorageConfig {
            db_path: dir.path().join("seen.db"),
        },
        defaults: DefaultsConfig::default(),
        feeds_dir: None,
        providers,
        feeds: vec![feed.clone()],
    };

    let client = Client::builder().no_proxy().build().unwrap();
    let store = Store::open(&config.storage.db_path).unwrap();

    // Run 1: sees Article A and marks it seen
    assert!(matches!(
        run_feed(&config, &feed, &client, &store, false)
            .await
            .unwrap(),
        RunOutcome::Published(_)
    ));

    // Run 2: no new items -> skips without calling LLM
    assert!(matches!(
        run_feed(&config, &feed, &client, &store, false)
            .await
            .unwrap(),
        RunOutcome::Skipped(_)
    ));
    assert_eq!(seen_prompts.lock().unwrap().len(), 1);

    // Add Article B; dry-run sees Article B (not A) and does NOT mark B seen
    rss_items
        .lock()
        .unwrap()
        .push(("guid-b".to_string(), "Article B".to_string()));
    assert!(matches!(
        run_feed(&config, &feed, &client, &store, true)
            .await
            .unwrap(),
        RunOutcome::DryRun { .. }
    ));

    // Real run still sees Article B (and NOT Article A)
    assert!(matches!(
        run_feed(&config, &feed, &client, &store, false)
            .await
            .unwrap(),
        RunOutcome::Published(_)
    ));

    let prompts = seen_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].contains("Article A"));
    assert!(prompts[2].contains("Article B"));
    assert!(!prompts[2].contains("Article A"));
}
