use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{AppConfig, FeedConfig};
use crate::error::{AppError, Result};
use crate::llm::{self, LlmRequest};
use crate::reload::ConfigHandle;
use crate::render::{render_entry_html, render_prompt, PromptContext};
use crate::sources::{fetch_sources, FetchOptions};
use crate::store::{FeedEntry, Store};

const MAX_SEEN_ITEMS_PER_SOURCE: usize = 2_000;

/// Shared runtime context bundling hot-reloadable config, HTTP client, SQLite store, and run locks.
#[derive(Clone)]
pub struct AppContext {
    pub config: ConfigHandle,
    pub client: Client,
    pub store: Store,
    pub runs: RunTracker,
}

impl AppContext {
    pub fn new(config: ConfigHandle, client: Client, store: Store) -> Self {
        Self {
            config,
            client,
            store,
            runs: RunTracker::new(),
        }
    }

    pub fn current_config(&self) -> Arc<AppConfig> {
        self.config.current()
    }

    /// Look up `feed_id` in the active config and run it under the per-feed concurrency guard.
    /// Returns `Ok(None)` if a run for this feed is already in progress.
    pub async fn run_feed_guarded(
        &self,
        feed_id: &str,
        dry_run: bool,
    ) -> Result<Option<RunOutcome>> {
        let cfg = self.config.current();
        let feed = cfg.find_feed(feed_id)?.clone();
        let Some(_permit) = self.runs.try_acquire(&feed.id) else {
            return Ok(None);
        };
        run_feed(&cfg, &feed, &self.client, &self.store, dry_run)
            .await
            .map(Some)
    }
}

/// Tracks currently executing feeds so manual triggers and scheduled runs do not overlap.
#[derive(Clone, Default)]
pub struct RunTracker {
    in_flight: Arc<Mutex<HashSet<String>>>,
}

pub struct RunPermit {
    feed_id: String,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl Drop for RunPermit {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.in_flight.lock() {
            guard.remove(&self.feed_id);
        }
    }
}

impl RunTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_acquire(&self, feed_id: &str) -> Option<RunPermit> {
        let mut guard = self.in_flight.lock().expect("run tracker mutex poisoned");
        if guard.insert(feed_id.to_string()) {
            Some(RunPermit {
                feed_id: feed_id.to_string(),
                in_flight: Arc::clone(&self.in_flight),
            })
        } else {
            None
        }
    }

    pub fn is_running(&self, feed_id: &str) -> bool {
        self.in_flight
            .lock()
            .expect("run tracker mutex poisoned")
            .contains(feed_id)
    }
}

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Published(FeedEntry),
    DryRun {
        title: String,
        prompt: String,
        raw_markdown: String,
        model: String,
    },
    Skipped(String),
}

/// Execute a single virtual feed end-to-end: pre-fetch sources, call the LLM, and store the entry.
pub async fn run_feed(
    config: &AppConfig,
    feed: &FeedConfig,
    client: &Client,
    store: &Store,
    dry_run: bool,
) -> Result<RunOutcome> {
    let started_at = Utc::now();
    let res = run_feed_inner(config, feed, client, store, dry_run).await;

    if !dry_run {
        let finished_at = Utc::now();
        match &res {
            Ok(RunOutcome::Published(entry)) => {
                let _ = store.record_run(
                    &feed.id,
                    started_at,
                    finished_at,
                    "ok",
                    Some(&format!("published '{}'", entry.title)),
                );
            }
            Ok(RunOutcome::Skipped(reason)) => {
                let _ =
                    store.record_run(&feed.id, started_at, finished_at, "skipped", Some(reason));
            }
            Ok(RunOutcome::DryRun { .. }) => {}
            Err(err) => {
                let _ = store.record_run(
                    &feed.id,
                    started_at,
                    finished_at,
                    "error",
                    Some(&err.to_string()),
                );
            }
        }
    }

    res
}

async fn run_feed_inner(
    config: &AppConfig,
    feed: &FeedConfig,
    client: &Client,
    store: &Store,
    dry_run: bool,
) -> Result<RunOutcome> {
    let provider = config.providers.get(&feed.provider).ok_or_else(|| {
        AppError::Config(format!(
            "feed '{}' references unknown provider '{}'",
            feed.id, feed.provider
        ))
    })?;

    let tz = feed.effective_timezone(&config.defaults)?;
    let now_local = Utc::now().with_timezone(&tz);

    info!(feed_id = %feed.id, provider = %feed.provider, "running virtual feed");

    let timeout = Duration::from_secs(config.defaults.timeout_secs);
    let retry = config.defaults.retry_policy();
    let fetch_opts = FetchOptions {
        timeout: timeout.min(Duration::from_secs(60)),
        retry,
        fail_on_source_error: feed.effective_fail_on_source_error(&config.defaults),
    };
    let fetched = fetch_sources(client, &feed.id, &feed.sources, store, fetch_opts).await?;

    if feed.skip_if_no_new_items
        && fetched.tracked_rss_sources > 0
        && fetched.new_rss_items.is_empty()
    {
        info!(
            feed_id = %feed.id,
            "skipping feed run because tracked RSS sources have no new items"
        );
        return Ok(RunOutcome::Skipped("no new RSS source items".to_string()));
    }

    let previous_entry = if feed.prompt.contains("{{previous_entry") {
        store.latest_entry(&feed.id)?
    } else {
        None
    };
    let user_prompt = render_prompt(
        feed,
        &PromptContext {
            now_local,
            sources: &fetched.sources,
            previous_entry: previous_entry.as_ref(),
        },
    );

    let default_system = "You are generating an item for a personal RSS/Atom feed reader. \
        Write crisp, scannable Markdown. Start with a concise `# Headline` on the first line, \
        followed by well-structured sections, bullet points, or tables where helpful.";

    let system_prompt = feed
        .system_prompt
        .as_deref()
        .map(str::to_string)
        .or_else(|| Some(default_system.to_string()));

    let llm_req = LlmRequest {
        model: feed.model.clone().unwrap_or_else(|| provider.model.clone()),
        system_prompt,
        user_prompt: user_prompt.clone(),
        web_search: feed.web_search.unwrap_or(provider.web_search),
        temperature: provider.temperature,
        max_tokens: provider.max_tokens,
    };

    let llm_resp =
        llm::generate(client, &feed.provider, provider, &llm_req, timeout, retry).await?;

    if let Some(marker) = &feed.skip_if_contains {
        if !marker.is_empty() && llm_resp.text.contains(marker) {
            if !dry_run {
                store.mark_source_items_seen(
                    &feed.id,
                    &fetched.new_rss_items,
                    MAX_SEEN_ITEMS_PER_SOURCE,
                )?;
            }
            info!(feed_id = %feed.id, marker = %marker, "skipping feed entry due to skip_if_contains match");
            return Ok(RunOutcome::Skipped(format!(
                "matched skip marker '{marker}'"
            )));
        }
    }

    let normalized = llm_resp.text.trim();
    let content_hash = sha256_hex(normalized);

    if !dry_run && feed.deduplicate {
        if let Some(prev_hash) = store.latest_content_hash(&feed.id)? {
            if prev_hash == content_hash {
                store.mark_source_items_seen(
                    &feed.id,
                    &fetched.new_rss_items,
                    MAX_SEEN_ITEMS_PER_SOURCE,
                )?;
                info!(feed_id = %feed.id, "skipping duplicate feed output");
                return Ok(RunOutcome::Skipped("duplicate content hash".to_string()));
            }
        }
    }

    let (title, content_html, summary) =
        render_entry_html(feed, now_local, normalized, &llm_resp.citations);

    if dry_run {
        return Ok(RunOutcome::DryRun {
            title,
            prompt: user_prompt,
            raw_markdown: llm_resp.text,
            model: llm_resp.model,
        });
    }

    let base = config.server.base_url.trim_end_matches('/');
    let id = format!("urn:uuid:{}", Uuid::new_v4());
    let link = format!("{base}/feeds/{}.xml#{}", feed.id, id);

    let entry = FeedEntry {
        id,
        feed_id: feed.id.clone(),
        title,
        link,
        content_html,
        raw_markdown: Some(normalized.to_string()),
        summary,
        published_at: Utc::now(),
        content_hash,
        model: llm_resp.model,
        citations: llm_resp.citations,
    };

    let max_items = feed.effective_max_items(&config.defaults);
    store.insert_entry(&entry, max_items)?;
    store.mark_source_items_seen(&feed.id, &fetched.new_rss_items, MAX_SEEN_ITEMS_PER_SOURCE)?;
    info!(feed_id = %feed.id, entry_title = %entry.title, "published feed entry");

    Ok(RunOutcome::Published(entry))
}

/// Run all enabled feeds concurrently (or sequentially? sequentially avoids hammering rate limits,
/// or bounded concurrency; sequential per CLI invocation with clear logging is predictable and safe).
pub async fn run_all_feeds(
    config: &AppConfig,
    client: &Client,
    store: &Store,
    dry_run: bool,
) -> Vec<(String, Result<RunOutcome>)> {
    let mut results = Vec::new();
    for feed in config.feeds.iter().filter(|f| f.enabled) {
        let res = run_feed(config, feed, client, store, dry_run).await;
        if let Err(err) = &res {
            warn!(feed_id = %feed.id, error = %err, "feed run failed");
        }
        results.push((feed.id.clone(), res));
    }
    results
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_tracker_prevents_overlapping_runs_and_releases_on_drop() {
        let tracker = RunTracker::new();
        assert!(!tracker.is_running("briefing"));

        {
            let permit = tracker.try_acquire("briefing");
            assert!(permit.is_some());
            assert!(tracker.is_running("briefing"));
            assert!(tracker.try_acquire("briefing").is_none());
            assert!(tracker.try_acquire("other").is_some());
        }

        assert!(!tracker.is_running("briefing"));
        assert!(tracker.try_acquire("briefing").is_some());
    }
}
