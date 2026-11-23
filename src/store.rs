//! SQLite persistence: the doc cache and run accounting.
//!
//! One database per project, at
//! `$XDG_DATA_HOME/llm_lsp/<hash of project root>.db`. Queries are checked at
//! runtime (`sqlx::query`), so building never needs a live database. The
//! migrations in `migrations/` are embedded at compile time.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::lang::Lang;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Identifies one generated doc.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocKey {
    pub lang: Lang,
    /// `docs::body_hash` of the symbol's text.
    pub body_hash: String,
    pub prompt_version: i64,
}

pub struct NewDoc<'a> {
    pub key: &'a DocKey,
    pub qualified_name: &'a str,
    pub doc: &'a str,
    pub model: &'a str,
}

pub struct NewRun<'a> {
    pub kind: &'a str,
    pub subject: &'a str,
    pub model: &'a str,
    pub started_at: SystemTime,
    pub duration: Duration,
    pub cost_usd: Option<f64>,
    pub error: Option<&'a str>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // Several editors on one project share this file.
            .busy_timeout(Duration::from_secs(5));
        Self::connect(SqlitePoolOptions::new().max_connections(4), options).await
    }

    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        let options = <SqliteConnectOptions as std::str::FromStr>::from_str("sqlite::memory:")?;
        // Each in-memory connection is its own database: keep exactly one.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None);
        Self::connect(pool, options).await
    }

    async fn connect(pool: SqlitePoolOptions, options: SqliteConnectOptions) -> Result<Self> {
        let pool = pool.connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool })
    }

    pub async fn doc(&self, key: &DocKey) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT doc FROM docs WHERE lang = ? AND body_hash = ? AND prompt_version = ?",
        )
        .bind(key.lang.name())
        .bind(&key.body_hash)
        .bind(key.prompt_version)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("doc")))
    }

    pub async fn put_doc(&self, new: NewDoc<'_>) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO docs
                (lang, body_hash, prompt_version, qualified_name, doc, model, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new.key.lang.name())
        .bind(&new.key.body_hash)
        .bind(new.key.prompt_version)
        .bind(new.qualified_name)
        .bind(new.doc)
        .bind(new.model)
        .bind(unix(SystemTime::now()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_run(&self, run: NewRun<'_>) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs
                (kind, subject, model, started_at, duration_ms, cost_usd, ok, error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.kind)
        .bind(run.subject)
        .bind(run.model)
        .bind(unix(run.started_at))
        .bind(run.duration.as_millis() as i64)
        .bind(run.cost_usd)
        .bind(run.error.is_none())
        .bind(run.error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// `$XDG_DATA_HOME/llm_lsp/<16 hex chars of blake3(root)>.db`.
pub fn default_path(project_root: &Path) -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    let hash = blake3::hash(project_root.as_os_str().as_encoded_bytes()).to_hex();
    data.join("llm_lsp").join(format!("{}.db", &hash[..16]))
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(hash: &str, version: i64) -> DocKey {
        DocKey {
            lang: Lang::Rust,
            body_hash: hash.into(),
            prompt_version: version,
        }
    }

    #[tokio::test]
    async fn docs_round_trip_and_are_keyed_by_version() {
        let store = Store::open_in_memory().await.unwrap();
        let k = key("abc", 1);
        assert_eq!(store.doc(&k).await.unwrap(), None);

        store
            .put_doc(NewDoc { key: &k, qualified_name: "P::f", doc: "Does f.", model: "haiku" })
            .await
            .unwrap();
        assert_eq!(store.doc(&k).await.unwrap().as_deref(), Some("Does f."));
        assert_eq!(store.doc(&key("abc", 2)).await.unwrap(), None, "new prompt version");
        assert_eq!(store.doc(&key("abd", 1)).await.unwrap(), None, "different body");

        // Regenerating replaces.
        store
            .put_doc(NewDoc { key: &k, qualified_name: "P::f", doc: "Does f better.", model: "haiku" })
            .await
            .unwrap();
        assert_eq!(store.doc(&k).await.unwrap().as_deref(), Some("Does f better."));
    }

    #[tokio::test]
    async fn records_runs() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .record_run(NewRun {
                kind: "doc",
                subject: "P::f",
                model: "haiku",
                started_at: SystemTime::now(),
                duration: Duration::from_millis(2100),
                cost_usd: Some(0.0006),
                error: None,
            })
            .await
            .unwrap();
        let (n, ok): (i64, i64) = sqlx::query_as("SELECT COUNT(*), SUM(ok) FROM runs")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!((n, ok), (1, 1));
    }

    #[tokio::test]
    async fn opens_a_file_database_twice() {
        let dir = std::env::temp_dir().join(format!("llm_lsp_test_{}", std::process::id()));
        let path = dir.join("nested/test.db");
        let k = key("abc", 1);
        {
            let store = Store::open(&path).await.unwrap();
            store
                .put_doc(NewDoc { key: &k, qualified_name: "f", doc: "d", model: "m" })
                .await
                .unwrap();
        }
        let store = Store::open(&path).await.unwrap();
        assert_eq!(store.doc(&k).await.unwrap().as_deref(), Some("d"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
