use std::sync::Arc;

use chrono::Utc;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::config::{AppConfig, FeedConfig};
use crate::runner::run_feed;
use crate::store::Store;

/// Spawn background cron scheduler tasks for every enabled feed.
pub fn spawn_scheduler(
    config: Arc<AppConfig>,
    client: Client,
    store: Store,
    shutdown: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    for feed in config.feeds.iter().filter(|f| f.enabled).cloned() {
        let cfg = Arc::clone(&config);
        let cli = client.clone();
        let st = store.clone();
        let mut rx = shutdown.clone();

        let handle = tokio::spawn(async move {
            run_feed_loop(cfg, feed, cli, st, &mut rx).await;
        });
        handles.push(handle);
    }

    handles
}

async fn run_feed_loop(
    config: Arc<AppConfig>,
    feed: FeedConfig,
    client: Client,
    store: Store,
    shutdown: &mut watch::Receiver<bool>,
) {
    let schedule = match feed.parsed_schedule() {
        Ok(s) => s,
        Err(e) => {
            error!(feed_id = %feed.id, error = %e, "failed to parse cron schedule");
            return;
        }
    };
    let tz = match feed.effective_timezone(&config.defaults) {
        Ok(t) => t,
        Err(e) => {
            error!(feed_id = %feed.id, error = %e, "invalid timezone");
            return;
        }
    };

    if feed.run_on_startup {
        info!(feed_id = %feed.id, "running feed on startup");
        if let Err(e) = run_feed(&config, &feed, &client, &store, false).await {
            error!(feed_id = %feed.id, error = %e, "startup feed run failed");
        }
    }

    loop {
        if *shutdown.borrow() {
            break;
        }

        let now_local = Utc::now().with_timezone(&tz);
        let Some(next) = schedule.after(&now_local).next() else {
            info!(feed_id = %feed.id, "cron schedule has no upcoming occurrences");
            break;
        };

        let wait_std = match (next - now_local).to_std() {
            Ok(d) => d,
            Err(_) => std::time::Duration::from_secs(1),
        };

        info!(feed_id = %feed.id, next_run = %next.to_rfc3339(), "scheduled next run");

        tokio::select! {
            _ = tokio::time::sleep(wait_std) => {
                if let Err(e) = run_feed(&config, &feed, &client, &store, false).await {
                    error!(feed_id = %feed.id, error = %e, "scheduled feed run failed");
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}
