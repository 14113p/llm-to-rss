use std::collections::HashMap;

use chrono::Utc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::runner::AppContext;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScheduleKey {
    schedule: String,
    timezone: String,
}

struct ActiveFeedTask {
    schedule_key: ScheduleKey,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

/// Spawn a supervisor task that maintains per-feed cron scheduler loops and dynamically
/// reconciles them whenever the configuration is hot-reloaded.
pub fn spawn_scheduler(ctx: AppContext, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut config_rx = ctx.config.subscribe();
        let mut active: HashMap<String, ActiveFeedTask> = HashMap::new();

        reconcile_feeds(&mut active, &ctx, true).await;

        loop {
            if *shutdown.borrow() {
                break;
            }

            tokio::select! {
                changed = config_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    reconcile_feeds(&mut active, &ctx, false).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }

        for (feed_id, task) in active {
            let _ = task.stop_tx.send(true);
            if let Err(e) = task.handle.await {
                error!(feed_id = %feed_id, error = %e, "scheduler task join error");
            }
        }
    })
}

async fn reconcile_feeds(
    active: &mut HashMap<String, ActiveFeedTask>,
    ctx: &AppContext,
    is_initial_startup: bool,
) {
    let cfg = ctx.current_config();
    let mut desired: HashMap<String, (ScheduleKey, bool)> = HashMap::new();

    for feed in cfg.feeds.iter().filter(|f| f.enabled) {
        let tz = feed
            .timezone
            .clone()
            .unwrap_or_else(|| cfg.defaults.timezone.clone());
        desired.insert(
            feed.id.clone(),
            (
                ScheduleKey {
                    schedule: feed.schedule.clone(),
                    timezone: tz,
                },
                feed.run_on_startup,
            ),
        );
    }

    // Stop tasks that were removed, disabled, or had their cron/timezone changed.
    let to_stop: Vec<String> = active
        .iter()
        .filter(|(id, task)| {
            desired
                .get(*id)
                .is_none_or(|(key, _)| *key != task.schedule_key)
        })
        .map(|(id, _)| id.clone())
        .collect();

    for id in to_stop {
        if let Some(task) = active.remove(&id) {
            info!(feed_id = %id, "stopping scheduler loop");
            let _ = task.stop_tx.send(true);
            if let Err(e) = task.handle.await {
                error!(feed_id = %id, error = %e, "scheduler task join error");
            }
        }
    }

    // Spawn tasks for newly enabled or rescheduled feeds.
    for (id, (schedule_key, run_on_startup)) in desired {
        if active.contains_key(&id) {
            continue;
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle = tokio::spawn(run_feed_loop(
            id.clone(),
            ctx.clone(),
            stop_rx,
            is_initial_startup && run_on_startup,
        ));
        active.insert(
            id,
            ActiveFeedTask {
                schedule_key,
                stop_tx,
                handle,
            },
        );
    }
}

async fn run_feed_loop(
    feed_id: String,
    ctx: AppContext,
    mut stop: watch::Receiver<bool>,
    run_on_startup: bool,
) {
    let initial_cfg = ctx.current_config();
    let Ok(initial_feed) = initial_cfg.find_feed(&feed_id).cloned() else {
        return;
    };
    let schedule = match initial_feed.parsed_schedule() {
        Ok(s) => s,
        Err(e) => {
            error!(feed_id = %feed_id, error = %e, "failed to parse cron schedule");
            return;
        }
    };
    let tz = match initial_feed.effective_timezone(&initial_cfg.defaults) {
        Ok(t) => t,
        Err(e) => {
            error!(feed_id = %feed_id, error = %e, "invalid timezone");
            return;
        }
    };

    if run_on_startup {
        info!(feed_id = %feed_id, "running feed on startup");
        execute_if_idle(&ctx, &feed_id, "startup").await;
    }

    loop {
        if *stop.borrow() {
            break;
        }

        let now_local = Utc::now().with_timezone(&tz);
        let Some(next) = schedule.after(&now_local).next() else {
            info!(feed_id = %feed_id, "cron schedule has no upcoming occurrences");
            break;
        };

        let wait_std = match (next - now_local).to_std() {
            Ok(d) => d,
            Err(_) => std::time::Duration::from_secs(1),
        };

        info!(feed_id = %feed_id, next_run = %next.to_rfc3339(), "scheduled next run");

        tokio::select! {
            _ = tokio::time::sleep(wait_std) => {
                execute_if_idle(&ctx, &feed_id, "scheduled").await;
            }
            _ = stop.changed() => {
                if *stop.borrow() {
                    break;
                }
            }
        }
    }
}

async fn execute_if_idle(ctx: &AppContext, feed_id: &str, kind: &str) {
    let cfg = ctx.current_config();
    if cfg.find_feed(feed_id).is_ok_and(|f| !f.enabled) {
        return;
    }

    match ctx.run_feed_guarded(feed_id, false).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            info!(feed_id = %feed_id, trigger = kind, "skipping run because feed is already running");
        }
        Err(e) => {
            error!(feed_id = %feed_id, trigger = kind, error = %e, "feed run failed");
        }
    }
}
