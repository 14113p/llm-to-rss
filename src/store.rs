use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Citation {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEntry {
    pub id: String,
    pub feed_id: String,
    pub title: String,
    pub link: String,
    pub content_html: String,
    pub summary: String,
    pub published_at: DateTime<Utc>,
    pub content_hash: String,
    pub model: String,
    pub citations: Vec<Citation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedRun {
    pub id: i64,
    pub feed_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS entries (
                id TEXT PRIMARY KEY,
                feed_id TEXT NOT NULL,
                title TEXT NOT NULL,
                link TEXT NOT NULL,
                content_html TEXT NOT NULL,
                summary TEXT NOT NULL,
                published_at TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                model TEXT NOT NULL,
                citations_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_entries_feed_pub
                ON entries (feed_id, published_at DESC);

            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                feed_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                status TEXT NOT NULL,
                message TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_runs_feed_finished
                ON runs (feed_id, finished_at DESC);
            ",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn insert_entry(&self, entry: &FeedEntry, max_items: usize) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let citations_json = serde_json::to_string(&entry.citations)
            .map_err(|e| crate::error::AppError::Serde(e.to_string()))?;

        conn.execute(
            "INSERT INTO entries (id, feed_id, title, link, content_html, summary, published_at, content_hash, model, citations_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.id,
                entry.feed_id,
                entry.title,
                entry.link,
                entry.content_html,
                entry.summary,
                entry.published_at.to_rfc3339(),
                entry.content_hash,
                entry.model,
                citations_json,
            ],
        )?;

        conn.execute(
            "DELETE FROM entries
             WHERE feed_id = ?1
               AND id NOT IN (
                   SELECT id FROM entries
                   WHERE feed_id = ?1
                   ORDER BY published_at DESC
                   LIMIT ?2
               )",
            params![entry.feed_id, max_items as i64],
        )?;

        Ok(())
    }

    pub fn latest_content_hash(&self, feed_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let hash = conn
            .query_row(
                "SELECT content_hash FROM entries WHERE feed_id = ?1 ORDER BY published_at DESC LIMIT 1",
                params![feed_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(hash)
    }

    pub fn list_entries(&self, feed_id: &str, limit: usize) -> Result<Vec<FeedEntry>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, feed_id, title, link, content_html, summary, published_at, content_hash, model, citations_json
             FROM entries
             WHERE feed_id = ?1
             ORDER BY published_at DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![feed_id, limit as i64], |row| {
            let published_raw: String = row.get(6)?;
            let citations_raw: String = row.get(9)?;
            let published_at = DateTime::parse_from_rfc3339(&published_raw)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let citations: Vec<Citation> = serde_json::from_str(&citations_raw).unwrap_or_default();

            Ok(FeedEntry {
                id: row.get(0)?,
                feed_id: row.get(1)?,
                title: row.get(2)?,
                link: row.get(3)?,
                content_html: row.get(4)?,
                summary: row.get(5)?,
                published_at,
                content_hash: row.get(7)?,
                model: row.get(8)?,
                citations,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn count_entries(&self, feed_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            params![feed_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn record_run(
        &self,
        feed_id: &str,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        status: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO runs (feed_id, started_at, finished_at, status, message)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                feed_id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                message,
            ],
        )?;
        Ok(())
    }

    pub fn last_run(&self, feed_id: &str) -> Result<Option<FeedRun>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let run = conn
            .query_row(
                "SELECT id, feed_id, started_at, finished_at, status, message
                 FROM runs WHERE feed_id = ?1 ORDER BY id DESC LIMIT 1",
                params![feed_id],
                |row| {
                    let s: String = row.get(2)?;
                    let f: String = row.get(3)?;
                    Ok(FeedRun {
                        id: row.get(0)?,
                        feed_id: row.get(1)?,
                        started_at: DateTime::parse_from_rfc3339(&s)
                            .map(|d| d.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        finished_at: DateTime::parse_from_rfc3339(&f)
                            .map(|d| d.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        status: row.get(4)?,
                        message: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn enforces_retention_and_tracks_runs() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();

        for i in 0..5 {
            let entry = FeedEntry {
                id: format!("urn:uuid:{i}"),
                feed_id: "briefing".into(),
                title: format!("Item {i}"),
                link: "http://localhost:8080/feeds/briefing.xml".into(),
                content_html: format!("<p>{i}</p>"),
                summary: format!("summary {i}"),
                published_at: Utc::now() + chrono::Duration::seconds(i),
                content_hash: format!("hash-{i}"),
                model: "gpt-4o".into(),
                citations: vec![],
            };
            store.insert_entry(&entry, 3).unwrap();
        }

        let entries = store.list_entries("briefing", 10).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "Item 4");
        assert_eq!(
            store.latest_content_hash("briefing").unwrap().as_deref(),
            Some("hash-4")
        );

        let now = Utc::now();
        store
            .record_run("briefing", now, now, "ok", Some("published"))
            .unwrap();
        let last = store.last_run("briefing").unwrap().unwrap();
        assert_eq!(last.status, "ok");
    }
}
