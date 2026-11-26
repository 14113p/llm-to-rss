use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use clap::Parser;
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use llm_to_rss::cli::{Cli, Command};
use llm_to_rss::config::AppConfig;
use llm_to_rss::reload::{spawn_config_watcher, ConfigHandle};
use llm_to_rss::render::build_opml;
use llm_to_rss::runner::{run_all_feeds, run_feed, RunOutcome};
use llm_to_rss::scheduler::spawn_scheduler;
use llm_to_rss::server::{router, AppState};
use llm_to_rss::store::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = Arc::new(
        AppConfig::load(&cli.config)
            .with_context(|| format!("failed to load config from {}", cli.config.display()))?,
    );

    match cli.command {
        Command::Validate => {
            println!(
                "Configuration OK: {} provider(s), {} feed(s) ({} enabled).",
                config.providers.len(),
                config.feeds.len(),
                config.feeds.iter().filter(|f| f.enabled).count()
            );
        }
        Command::List => {
            let store = Store::open(&config.storage.db_path)?;
            for feed in &config.feeds {
                let count = store.count_entries(&feed.id)?;
                let tz = feed.effective_timezone(&config.defaults)?;
                let next = feed
                    .parsed_schedule()?
                    .after(&Utc::now().with_timezone(&tz))
                    .next()
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "none".into());
                println!(
                    "{:<24} enabled={:<5} provider={:<14} cron={:<15} items={:<3} next={}",
                    feed.id, feed.enabled, feed.provider, feed.schedule, count, next
                );
            }
        }
        Command::Opml { base_url } => {
            let effective = base_url.as_deref().unwrap_or(&config.server.base_url);
            print!("{}", build_opml(&config.feeds, effective));
        }
        Command::Run { feed, dry_run } => {
            let client = Client::builder().build()?;
            let store = Store::open(&config.storage.db_path)?;

            if let Some(feed_id) = feed {
                let target = config.find_feed(&feed_id)?;
                let outcome = run_feed(&config, target, &client, &store, dry_run).await?;
                print_outcome(&feed_id, outcome);
            } else {
                let results = run_all_feeds(&config, &client, &store, dry_run).await;
                let mut had_error = false;
                for (id, res) in results {
                    match res {
                        Ok(outcome) => print_outcome(&id, outcome),
                        Err(e) => {
                            had_error = true;
                            eprintln!("[{id}] ERROR: {e}");
                        }
                    }
                }
                if had_error {
                    std::process::exit(1);
                }
            }
        }
        Command::Serve => {
            let client = Client::builder().build()?;
            let store = Store::open(&config.storage.db_path)?;

            let config_handle = ConfigHandle::new(Arc::clone(&config), cli.config.clone());
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let scheduler_handle = spawn_scheduler(
                config_handle.clone(),
                client.clone(),
                store.clone(),
                shutdown_rx.clone(),
            );
            let watcher_handle = spawn_config_watcher(
                config_handle.clone(),
                Duration::from_secs(config.server.reload_interval_secs),
                shutdown_rx,
            );

            let state = AppState {
                config: config_handle,
                client,
                store,
            };

            let app = router(state);
            let listener = TcpListener::bind(&config.server.bind)
                .await
                .with_context(|| format!("failed to bind {}", config.server.bind))?;
            info!(
                bind = %config.server.bind,
                base_url = %config.server.base_url,
                reload_interval_secs = config.server.reload_interval_secs,
                "listening"
            );

            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal(shutdown_tx))
                .await?;

            if let Err(e) = watcher_handle.await {
                error!(error = %e, "config watcher task join error");
            }
            if let Err(e) = scheduler_handle.await {
                error!(error = %e, "scheduler task join error");
            }
        }
    }

    Ok(())
}

fn print_outcome(feed_id: &str, outcome: RunOutcome) {
    match outcome {
        RunOutcome::Published(entry) => {
            println!("[{feed_id}] published: {} ({})", entry.title, entry.id);
        }
        RunOutcome::Skipped(reason) => {
            println!("[{feed_id}] skipped: {reason}");
        }
        RunOutcome::DryRun {
            title,
            prompt,
            raw_markdown,
            model,
        } => {
            println!("=== [{feed_id}] DRY RUN (model: {model}) ===");
            println!("--- Prompt ---\n{prompt}\n");
            println!("--- Title ---\n{title}\n");
            println!("--- Output ---\n{raw_markdown}\n");
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal(tx: watch::Sender<bool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received");
    let _ = tx.send(true);
}
