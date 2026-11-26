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
use llm_to_rss::reload::{spawn_config_watcher, ConfigHandle};
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
        fail_on_source_error: None,
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
        fail_on_source_error: None,
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
            reload_interval_secs: 2,
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
        config: ConfigHandle::static_config(Arc::clone(&config)),
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
        fail_on_source_error: None,
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
        fail_on_source_error: None,
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

#[tokio::test]
async fn retries_transient_llm_and_source_errors() {
    use axum::http::header;
    use axum::response::IntoResponse;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let rss_attempts = Arc::new(AtomicUsize::new(0));
    let rss_attempts_clone = Arc::clone(&rss_attempts);
    let llm_attempts = Arc::new(AtomicUsize::new(0));
    let llm_attempts_clone = Arc::clone(&llm_attempts);

    let app = Router::new()
        .route(
            "/flaky.rss",
            get(move || {
                let attempts = Arc::clone(&rss_attempts_clone);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        return (StatusCode::SERVICE_UNAVAILABLE, "temporary outage")
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Flaky</title><link>https://example.com</link><description>d</description><item><guid>g1</guid><title>Recovered Item</title><link>https://example.com/1</link></item></channel></rss>"#,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move || {
                let attempts = Arc::clone(&llm_attempts_clone);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, "0")],
                            "rate limited",
                        )
                            .into_response();
                    }
                    if n == 1 {
                        return (StatusCode::BAD_GATEWAY, "<html>bad gateway</html>")
                            .into_response();
                    }
                    Json(json!({
                        "model": "llama3.2",
                        "choices": [{ "message": { "content": "# Recovered\n\nAll good." } }]
                    }))
                    .into_response()
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
        id: "retry-feed".to_string(),
        title: "Retry Test".to_string(),
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
            name: "Flaky".to_string(),
            url: format!("http://{addr}/flaky.rss"),
            limit: Some(5),
            only_new: true,
        }],
        fail_on_source_error: None,
        skip_if_contains: None,
        skip_if_no_new_items: false,
        deduplicate: false,
        max_items: Some(10),
        run_on_startup: false,
    };

    let config = AppConfig {
        server: ServerConfig::default(),
        storage: StorageConfig {
            db_path: dir.path().join("retry.db"),
        },
        defaults: DefaultsConfig {
            max_retries: 3,
            retry_base_delay_ms: 10,
            ..DefaultsConfig::default()
        },
        feeds_dir: None,
        providers,
        feeds: vec![feed.clone()],
    };

    let client = Client::builder().no_proxy().build().unwrap();
    let store = Store::open(&config.storage.db_path).unwrap();

    let outcome = run_feed(&config, &feed, &client, &store, false)
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Published(_)));
    assert_eq!(rss_attempts.load(Ordering::SeqCst), 3);
    assert_eq!(llm_attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn continues_when_fail_on_source_error_is_false() {
    let seen_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_clone = Arc::clone(&seen_prompts);

    let app = Router::new()
        .route(
            "/broken.rss",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        )
        .route(
            "/healthy.rss",
            get(|| async {
                r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Good</title><link>https://example.com</link><description>d</description><item><guid>ok-1</guid><title>Healthy Headline</title><link>https://example.com/ok</link></item></channel></rss>"#
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
                    prompts.lock().unwrap().push(user_msg);
                    Json(json!({
                        "model": "llama3.2",
                        "choices": [{ "message": { "content": "# Partial Briefing\n\nHealthy Headline." } }]
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

    let base_feed = FeedConfig {
        id: "partial-feed".to_string(),
        title: "Partial Test".to_string(),
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
        sources: vec![
            SourceConfig::Rss {
                name: "Broken Source".to_string(),
                url: format!("http://{addr}/broken.rss"),
                limit: Some(5),
                only_new: true,
            },
            SourceConfig::Rss {
                name: "Healthy Source".to_string(),
                url: format!("http://{addr}/healthy.rss"),
                limit: Some(5),
                only_new: true,
            },
        ],
        fail_on_source_error: Some(true),
        skip_if_contains: None,
        skip_if_no_new_items: false,
        deduplicate: false,
        max_items: Some(10),
        run_on_startup: false,
    };

    let config = AppConfig {
        server: ServerConfig::default(),
        storage: StorageConfig {
            db_path: dir.path().join("partial.db"),
        },
        defaults: DefaultsConfig {
            max_retries: 0,
            retry_base_delay_ms: 1,
            ..DefaultsConfig::default()
        },
        feeds_dir: None,
        providers,
        feeds: vec![base_feed.clone()],
    };

    let client = Client::builder().no_proxy().build().unwrap();
    let store = Store::open(&config.storage.db_path).unwrap();

    // Strict mode fails
    assert!(run_feed(&config, &base_feed, &client, &store, false)
        .await
        .is_err());

    // Tolerant mode succeeds and includes healthy source + unavailability note
    let mut tolerant = base_feed;
    tolerant.fail_on_source_error = Some(false);
    let outcome = run_feed(&config, &tolerant, &client, &store, false)
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Published(_)));

    let prompts = seen_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("Healthy Headline"));
    assert!(prompts[0].contains("[Source unavailable:"));
}

#[tokio::test]
async fn hot_reloads_new_feed_into_live_http_server() {
    use std::time::Duration;
    use tokio::sync::watch;

    let dir = tempdir().unwrap();
    let feeds_dir = dir.path().join("feeds");
    std::fs::create_dir_all(&feeds_dir).unwrap();
    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "storage:\n  db_path: '{}'\nfeeds_dir: feeds\nproviders:\n  local:\n    kind: openai_compatible\n    model: llama3.2\n",
            dir.path().join("hot.db").display()
        ),
    )
    .unwrap();
    std::fs::write(
        feeds_dir.join("initial.yaml"),
        "title: Initial Feed\nschedule: '0 7 * * *'\nprovider: local\nprompt: hello\n",
    )
    .unwrap();

    let loaded = Arc::new(AppConfig::load(&config_path).unwrap());
    let store = Store::open(&loaded.storage.db_path).unwrap();
    let config_handle = ConfigHandle::new(loaded, config_path);
    let mut cfg_rx = config_handle.subscribe();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watcher = spawn_config_watcher(
        config_handle.clone(),
        Duration::from_millis(40),
        shutdown_rx,
    );

    let state = AppState {
        config: config_handle,
        client: Client::builder().no_proxy().build().unwrap(),
        store,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let http = Client::builder().no_proxy().build().unwrap();
    let before: Value = http
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["feeds_total"], 1);

    // Drop a brand-new feed YAML into feeds/ without restarting the server
    std::fs::write(
        feeds_dir.join("added-live.yaml"),
        "title: Added Live\nschedule: '0 8 * * *'\nprovider: local\nprompt: live\n",
    )
    .unwrap();

    tokio::time::timeout(Duration::from_secs(2), cfg_rx.changed())
        .await
        .expect("server config should hot-reload")
        .unwrap();

    let after: Value = http
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["feeds_total"], 2);

    let feed_resp = http
        .get(format!("http://{addr}/feeds/added-live.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(feed_resp.status(), StatusCode::OK);
    assert!(feed_resp.text().await.unwrap().contains("Added Live"));

    let _ = shutdown_tx.send(true);
    watcher.await.unwrap();
}
