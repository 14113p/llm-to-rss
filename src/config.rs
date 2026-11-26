use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono_tz::Tz;
use cron::Schedule;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// Top-level application configuration loaded from YAML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    /// Optional directory containing one YAML file per virtual feed (`*.yaml` / `*.yml`).
    #[serde(default)]
    pub feeds_dir: Option<PathBuf>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub feeds: Vec<FeedConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Optional bearer token protecting `POST /feeds/{id}/run`. If unset, manual HTTP triggers are disabled.
    #[serde(default)]
    pub trigger_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            base_url: default_base_url(),
            trigger_token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_max_items")]
    pub max_items: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            max_items: default_max_items(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiCompatible,
    Gemini,
    Anthropic,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn effective_base_url(&self) -> &str {
        if let Some(url) = &self.base_url {
            url.trim_end_matches('/')
        } else {
            match self.kind {
                ProviderKind::OpenaiResponses | ProviderKind::OpenaiCompatible => {
                    "https://api.openai.com/v1"
                }
                ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
                ProviderKind::Anthropic => "https://api.anthropic.com/v1",
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedConfig {
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: String,
    #[serde(default)]
    pub timezone: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub web_search: Option<bool>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub entry_title_template: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub skip_if_contains: Option<String>,
    /// Skip calling the LLM when all `only_new: true` RSS sources have zero unseen items.
    #[serde(default)]
    pub skip_if_no_new_items: bool,
    #[serde(default)]
    pub deduplicate: bool,
    #[serde(default)]
    pub max_items: Option<usize>,
    #[serde(default)]
    pub run_on_startup: bool,
}

impl FeedConfig {
    pub fn effective_max_items(&self, defaults: &DefaultsConfig) -> usize {
        self.max_items.unwrap_or(defaults.max_items)
    }

    pub fn effective_timezone(&self, defaults: &DefaultsConfig) -> Result<Tz> {
        let tz_name = self.timezone.as_deref().unwrap_or(&defaults.timezone);
        Tz::from_str(tz_name).map_err(|_| {
            AppError::Config(format!(
                "invalid timezone '{tz_name}' on feed '{}'",
                self.id
            ))
        })
    }

    pub fn parsed_schedule(&self) -> Result<Schedule> {
        parse_cron_schedule(&self.schedule).map_err(|e| {
            AppError::Config(format!(
                "invalid cron '{}' on feed '{}': {e}",
                self.schedule, self.id
            ))
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Url {
        name: String,
        url: String,
        #[serde(default)]
        max_chars: Option<usize>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Rss {
        name: String,
        url: String,
        #[serde(default)]
        limit: Option<usize>,
        /// Filter out RSS/Atom items already seen in previous successful runs (default: true).
        #[serde(default = "default_true")]
        only_new: bool,
    },
}

fn default_bind() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_base_url() -> String {
    std::env::var("LLM_TO_RSS_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

fn default_db_path() -> PathBuf {
    PathBuf::from("data/llm-to-rss.db")
}

fn default_timezone() -> String {
    std::env::var("TZ").unwrap_or_else(|_| "UTC".to_string())
}

fn default_max_items() -> usize {
    50
}

fn default_timeout_secs() -> u64 {
    90
}

fn default_true() -> bool {
    true
}

impl AppConfig {
    /// Load and validate configuration from `path`, resolving relative `feeds_dir` and `db_path`
    /// against the config file's parent directory.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            AppError::Config(format!("failed to read config '{}': {e}", path.display()))
        })?;
        let expanded = expand_env_vars(&raw);
        let mut cfg: AppConfig = serde_yaml::from_str(&expanded)
            .map_err(|e| AppError::Config(format!("failed to parse '{}': {e}", path.display())))?;

        let config_dir = path.parent().unwrap_or_else(|| Path::new("."));

        if cfg.storage.db_path.is_relative() {
            cfg.storage.db_path = config_dir.join(&cfg.storage.db_path);
        }

        if let Some(feeds_dir) = &cfg.feeds_dir {
            let resolved_dir = if feeds_dir.is_relative() {
                config_dir.join(feeds_dir)
            } else {
                feeds_dir.clone()
            };
            cfg.feeds_dir = Some(resolved_dir.clone());
            load_feeds_from_dir(&resolved_dir, &mut cfg.feeds)?;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        Tz::from_str(&self.defaults.timezone).map_err(|_| {
            AppError::Config(format!(
                "invalid default timezone '{}'",
                self.defaults.timezone
            ))
        })?;

        if self.defaults.max_items == 0 {
            return Err(AppError::Config("defaults.max_items must be > 0".into()));
        }

        if url::Url::parse(&self.server.base_url).is_err() {
            return Err(AppError::Config(format!(
                "invalid server.base_url '{}'",
                self.server.base_url
            )));
        }

        let slug_re = Regex::new(r"^[a-z0-9][a-z0-9_-]*$").expect("valid regex");
        let mut seen_ids = HashSet::new();

        for feed in &self.feeds {
            if feed.id.is_empty() || !slug_re.is_match(&feed.id) {
                return Err(AppError::Config(format!(
                    "feed id '{}' must match ^[a-z0-9][a-z0-9_-]*$",
                    feed.id
                )));
            }
            if !seen_ids.insert(feed.id.clone()) {
                return Err(AppError::Config(format!("duplicate feed id '{}'", feed.id)));
            }
            if feed.title.trim().is_empty() {
                return Err(AppError::Config(format!(
                    "feed '{}' title cannot be empty",
                    feed.id
                )));
            }
            if feed.prompt.trim().is_empty() {
                return Err(AppError::Config(format!(
                    "feed '{}' prompt cannot be empty",
                    feed.id
                )));
            }
            if !self.providers.contains_key(&feed.provider) {
                return Err(AppError::Config(format!(
                    "feed '{}' references unknown provider '{}'",
                    feed.id, feed.provider
                )));
            }
            feed.parsed_schedule()?;
            feed.effective_timezone(&self.defaults)?;
            if feed.effective_max_items(&self.defaults) == 0 {
                return Err(AppError::Config(format!(
                    "feed '{}' max_items must be > 0",
                    feed.id
                )));
            }
            for src in &feed.sources {
                let src_url = match src {
                    SourceConfig::Url { url, .. } | SourceConfig::Rss { url, .. } => url,
                };
                if url::Url::parse(src_url).is_err() {
                    return Err(AppError::Config(format!(
                        "feed '{}' has invalid source url '{}'",
                        feed.id, src_url
                    )));
                }
            }
        }

        Ok(())
    }

    pub fn find_feed(&self, id: &str) -> Result<&FeedConfig> {
        self.feeds
            .iter()
            .find(|f| f.id == id)
            .ok_or_else(|| AppError::FeedNotFound(id.to_string()))
    }
}

fn load_feeds_from_dir(dir: &Path, feeds: &mut Vec<FeedConfig>) -> Result<()> {
    if !dir.exists() {
        return Err(AppError::Config(format!(
            "feeds_dir '{}' does not exist",
            dir.display()
        )));
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| {
            AppError::Config(format!("failed to read feeds_dir '{}': {e}", dir.display()))
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("yaml") | Some("yml")
                )
        })
        .collect();
    entries.sort();

    for path in entries {
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            AppError::Config(format!(
                "failed to read feed file '{}': {e}",
                path.display()
            ))
        })?;
        let expanded = expand_env_vars(&raw);
        let mut feed: FeedConfig = serde_yaml::from_str(&expanded).map_err(|e| {
            AppError::Config(format!(
                "failed to parse feed file '{}': {e}",
                path.display()
            ))
        })?;
        if feed.id.trim().is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                feed.id = stem.to_string();
            }
        }
        feeds.push(feed);
    }
    Ok(())
}

/// Normalize 5-field standard cron (`min hour dom month dow`) into 6-field (`0 min hour dom month dow`)
/// expected by the `cron` crate, while accepting 6- or 7-field schedules verbatim.
pub fn parse_cron_schedule(expr: &str) -> std::result::Result<Schedule, String> {
    let trimmed = expr.trim();
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => format!("0 {trimmed}"),
        6 | 7 => trimmed.to_string(),
        n => {
            return Err(format!(
                "expected 5, 6, or 7 cron fields, got {n} in '{trimmed}'"
            ))
        }
    };
    Schedule::from_str(&normalized).map_err(|e| e.to_string())
}

/// Expand `${VAR}` and `${VAR:-default}` references using the current process environment.
pub fn expand_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").expect("valid regex");
    re.replace_all(input, |caps: &regex::Captures<'_>| {
        let var = &caps[1];
        match std::env::var(var) {
            Ok(val) if !val.is_empty() => val,
            _ => caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
        }
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_env_vars_and_defaults() {
        std::env::set_var("LLM_TO_RSS_TEST_VAR", "secret123");
        let out = expand_env_vars("key: ${LLM_TO_RSS_TEST_VAR}\nhost: ${UNSET_VAR:-localhost}");
        assert_eq!(out, "key: secret123\nhost: localhost");
        std::env::remove_var("LLM_TO_RSS_TEST_VAR");
    }

    #[test]
    fn parses_five_and_six_field_cron() {
        assert!(parse_cron_schedule("30 7 * * *").is_ok());
        assert!(parse_cron_schedule("0 30 7 * * *").is_ok());
        assert!(parse_cron_schedule("bad cron").is_err());
    }
}
