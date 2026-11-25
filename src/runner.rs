use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{AppConfig, FeedConfig};
use crate::error::{AppError, Result};
use crate::fetcher::fetch_sources;
use crate::llm::{self, LlmRequest};
use crate::render::{render_entry_html, render_prompt};
use crate::store::{FeedEntry, Store};

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

    let sources = fetch_sources(client, &feed.sources).await?;
    let user_prompt = render_prompt(feed, now_local, &sources);

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

    let timeout = Duration::from_secs(config.defaults.timeout_secs);
    let llm_resp = llm::generate(client, &feed.provider, provider, &llm_req, timeout).await?;

    if let Some(marker) = &feed.skip_if_contains {
        if !marker.is_empty() && llm_resp.text.contains(marker) {
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
        summary,
        published_at: Utc::now(),
        content_hash,
        model: llm_resp.model,
        citations: llm_resp.citations,
    };

    let max_items = feed.effective_max_items(&config.defaults);
    store.insert_entry(&entry, max_items)?;
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
