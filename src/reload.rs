use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::error::Result;

/// Shared, hot-reloadable handle to the active `AppConfig`.
#[derive(Clone)]
pub struct ConfigHandle {
    tx: Arc<watch::Sender<Arc<AppConfig>>>,
    rx: watch::Receiver<Arc<AppConfig>>,
    config_path: Option<PathBuf>,
}

impl ConfigHandle {
    pub fn new(config: Arc<AppConfig>, config_path: PathBuf) -> Self {
        let (tx, rx) = watch::channel(config);
        Self {
            tx: Arc::new(tx),
            rx,
            config_path: Some(config_path),
        }
    }

    /// Create a static handle not backed by a file on disk (primarily for tests).
    pub fn static_config(config: Arc<AppConfig>) -> Self {
        let (tx, rx) = watch::channel(config);
        Self {
            tx: Arc::new(tx),
            rx,
            config_path: None,
        }
    }

    pub fn current(&self) -> Arc<AppConfig> {
        self.rx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<AppConfig>> {
        self.tx.subscribe()
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    pub fn update(&self, new_config: Arc<AppConfig>) {
        self.tx.send_replace(new_config);
    }

    /// Reload configuration and `feeds_dir` from disk. On parse/validation failure, preserves the
    /// last known-good configuration and returns the error.
    pub fn reload_from_disk(&self) -> Result<Arc<AppConfig>> {
        let Some(path) = &self.config_path else {
            return Ok(self.current());
        };

        let loaded = Arc::new(AppConfig::load(path)?);
        let prev = self.current();

        if loaded.server.bind != prev.server.bind {
            warn!(
                old = %prev.server.bind,
                new = %loaded.server.bind,
                "server.bind changed on reload; restart required to re-bind listener"
            );
        }
        if loaded.storage.db_path != prev.storage.db_path {
            warn!(
                old = %prev.storage.db_path.display(),
                new = %loaded.storage.db_path.display(),
                "storage.db_path changed on reload; restart required to reopen database"
            );
        }

        let enabled = loaded.feeds.iter().filter(|f| f.enabled).count();
        self.tx.send_replace(Arc::clone(&loaded));
        info!(
            feeds_total = loaded.feeds.len(),
            feeds_enabled = enabled,
            providers = loaded.providers.len(),
            "reloaded configuration"
        );
        Ok(loaded)
    }
}

/// Compute a content fingerprint covering the config file and every `*.yaml`/`*.yml` file in `feeds_dir`.
pub fn config_fingerprint(config_path: &Path, feeds_dir: Option<&Path>) -> String {
    let mut hasher = Sha256::new();
    hash_file(&mut hasher, config_path);

    if let Some(dir) = feeds_dir {
        if let Ok(read_dir) = std::fs::read_dir(dir) {
            let mut files: Vec<PathBuf> = read_dir
                .filter_map(|e| e.ok().map(|entry| entry.path()))
                .filter(|p| {
                    p.is_file()
                        && matches!(
                            p.extension().and_then(|s| s.to_str()),
                            Some("yaml") | Some("yml")
                        )
                })
                .collect();
            files.sort();
            for path in files {
                hash_file(&mut hasher, &path);
            }
        }
    }

    format!("{:x}", hasher.finalize())
}

fn hash_file(hasher: &mut Sha256, path: &Path) {
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    match std::fs::read(path) {
        Ok(bytes) => {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Err(_) => {
            hasher.update(b"<missing>");
        }
    }
    hasher.update(b"\n");
}

/// Spawn a background task that watches `config.yaml` and `feeds_dir` for changes (via lightweight
/// content fingerprinting) and also listens for `SIGHUP` on Unix to trigger immediate reloads.
pub fn spawn_config_watcher(
    handle: ConfigHandle,
    poll_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let config_path = handle.config_path().map(Path::to_path_buf);
    let mut last_fp = config_path
        .as_deref()
        .map(|p| config_fingerprint(p, handle.current().feeds_dir.as_deref()))
        .unwrap_or_default();

    tokio::spawn(async move {
        let Some(config_path) = config_path else {
            return;
        };

        let polling_enabled = !poll_interval.is_zero();

        #[cfg(unix)]
        let mut sighup =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

        loop {
            if *shutdown.borrow() {
                break;
            }

            let poll_fut = async {
                if polling_enabled {
                    tokio::time::sleep(poll_interval).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };

            #[cfg(unix)]
            let sighup_fut = async {
                if let Some(sig) = sighup.as_mut() {
                    sig.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            #[cfg(not(unix))]
            let sighup_fut = std::future::pending::<()>();

            tokio::select! {
                _ = poll_fut => {
                    let fp = config_fingerprint(&config_path, handle.current().feeds_dir.as_deref());
                    if fp != last_fp {
                        last_fp = fp;
                        if let Err(err) = handle.reload_from_disk() {
                            error!(error = %err, "hot-reload failed; keeping previous valid configuration");
                        } else {
                            last_fp = config_fingerprint(&config_path, handle.current().feeds_dir.as_deref());
                        }
                    }
                }
                _ = sighup_fut => {
                    info!("SIGHUP received; reloading configuration");
                    match handle.reload_from_disk() {
                        Ok(_) => {
                            last_fp = config_fingerprint(&config_path, handle.current().feeds_dir.as_deref());
                        }
                        Err(err) => {
                            error!(error = %err, "SIGHUP reload failed; keeping previous valid configuration");
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_sample_config(dir: &Path) -> PathBuf {
        let feeds_dir = dir.join("feeds");
        std::fs::create_dir_all(&feeds_dir).unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(
            &config_path,
            "feeds_dir: feeds\nproviders:\n  local:\n    kind: openai_compatible\n    model: llama3.2\n",
        )
        .unwrap();
        std::fs::write(
            feeds_dir.join("first.yaml"),
            "title: First\nschedule: '0 7 * * *'\nprovider: local\nprompt: hi\n",
        )
        .unwrap();
        config_path
    }

    #[test]
    fn reload_preserves_previous_config_on_invalid_yaml() {
        let dir = tempdir().unwrap();
        let config_path = write_sample_config(dir.path());
        let initial = Arc::new(AppConfig::load(&config_path).unwrap());
        let handle = ConfigHandle::new(initial, config_path.clone());
        assert_eq!(handle.current().feeds.len(), 1);

        // Write invalid YAML into feeds/
        std::fs::write(dir.path().join("feeds/broken.yaml"), "title: [invalid").unwrap();
        assert!(handle.reload_from_disk().is_err());
        assert_eq!(handle.current().feeds.len(), 1);

        // Fix the file and reload again
        std::fs::write(
            dir.path().join("feeds/second.yaml"),
            "title: Second\nschedule: '0 8 * * *'\nprovider: local\nprompt: hello\n",
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("feeds/broken.yaml")).unwrap();
        let reloaded = handle.reload_from_disk().unwrap();
        assert_eq!(reloaded.feeds.len(), 2);
        assert_eq!(handle.current().feeds.len(), 2);
    }

    #[tokio::test]
    async fn watcher_detects_new_feed_file() {
        let dir = tempdir().unwrap();
        let config_path = write_sample_config(dir.path());
        let initial = Arc::new(AppConfig::load(&config_path).unwrap());
        let handle = ConfigHandle::new(initial, config_path);
        let mut rx = handle.subscribe();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let watcher = spawn_config_watcher(handle.clone(), Duration::from_millis(40), shutdown_rx);

        std::fs::write(
            dir.path().join("feeds/added.yaml"),
            "title: Added\nschedule: '0 9 * * *'\nprovider: local\nprompt: new\n",
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("watcher should detect new feed")
            .unwrap();
        assert_eq!(handle.current().feeds.len(), 2);

        let _ = shutdown_tx.send(true);
        watcher.await.unwrap();
    }
}
