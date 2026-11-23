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
use crate::notes::{self, Note, NoteScope};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Identifies one cached doc or review: the exact symbol text it was made
/// for, and the prompt version that made it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub lang: Lang,
    /// `docs::body_hash` of the symbol's text.
    pub body_hash: String,
    pub prompt_version: i64,
}

pub struct NewDoc<'a> {
    pub key: &'a CacheKey,
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

    pub async fn doc(&self, key: &CacheKey) -> Result<Option<String>> {
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

    /// The cached review's findings (a JSON array), if any.
    pub async fn review(&self, key: &CacheKey) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT findings FROM reviews WHERE lang = ? AND body_hash = ? AND prompt_version = ?",
        )
        .bind(key.lang.name())
        .bind(&key.body_hash)
        .bind(key.prompt_version)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("findings")))
    }

    pub async fn put_review(
        &self,
        key: &CacheKey,
        qualified_name: &str,
        findings_json: &str,
        model: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO reviews
                (lang, body_hash, prompt_version, qualified_name, findings, model, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key.lang.name())
        .bind(&key.body_hash)
        .bind(key.prompt_version)
        .bind(qualified_name)
        .bind(findings_json)
        .bind(model)
        .bind(unix(SystemTime::now()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn dismiss(&self, qualified_name: &str, fingerprint: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO dismissals (qualified_name, fingerprint, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(qualified_name)
        .bind(fingerprint)
        .bind(unix(SystemTime::now()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All `(qualified_name, fingerprint)` dismissals.
    pub async fn dismissals(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT qualified_name, fingerprint FROM dismissals")
            .fetch_all(&self.pool)
            .await?)
    }

    /// How many runs of `kind` started at or after `since`.
    pub async fn runs_since(&self, kind: &str, since: SystemTime) -> Result<i64> {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM runs WHERE kind = ? AND started_at >= ?")
                .bind(kind)
                .bind(unix(since))
                .fetch_one(&self.pool)
                .await?;
        Ok(n)
    }

    /// Adds a note, or refreshes an identical one (same place, same
    /// normalized text). `path`/`symbol` are ignored where the scope doesn't
    /// use them.
    pub async fn add_note(
        &self,
        scope: NoteScope,
        path: &str,
        symbol: &str,
        text: &str,
        source: &str,
    ) -> Result<()> {
        let path = if scope == NoteScope::Project { "" } else { path };
        let symbol = if scope == NoteScope::Symbol { symbol } else { "" };
        let now = unix(SystemTime::now());
        sqlx::query(
            "INSERT INTO notes (scope, path, symbol, text, norm, source, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (scope, path, symbol, norm) DO UPDATE SET updated_at = excluded.updated_at",
        )
        .bind(scope.as_str())
        .bind(path)
        .bind(symbol)
        .bind(text)
        .bind(notes::normalize(text))
        .bind(source)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Notes relevant to `symbol` in `path`: its own, its file's, and the
    /// most recent project-wide ones.
    pub async fn notes_for(&self, path: &str, symbol: &str) -> Result<Vec<Note>> {
        let rows = sqlx::query(
            "SELECT * FROM (
                 SELECT id, scope, path, symbol, text FROM notes WHERE scope = 'project'
                 ORDER BY updated_at DESC, id DESC LIMIT ?
             )
             UNION ALL
             SELECT id, scope, path, symbol, text FROM notes
             WHERE (scope = 'file' AND path = ?) OR (scope = 'symbol' AND path = ? AND symbol = ?)",
        )
        .bind(notes::MAX_PROJECT_IN_PROMPT as i64)
        .bind(path)
        .bind(path)
        .bind(symbol)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<Note> = rows.iter().filter_map(note_from_row).collect();
        out.sort_by_key(|n| (n.scope, n.id));
        Ok(out)
    }

    pub async fn all_notes(&self) -> Result<Vec<Note>> {
        let rows = sqlx::query("SELECT id, scope, path, symbol, text FROM notes ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().filter_map(note_from_row).collect())
    }

    /// Symbol-scoped notes in one file, as `(id, qualified name)`.
    pub async fn symbol_notes_in(&self, path: &str) -> Result<Vec<(i64, String)>> {
        Ok(
            sqlx::query_as("SELECT id, symbol FROM notes WHERE scope = 'symbol' AND path = ?")
                .bind(path)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Every distinct file path that file or symbol notes refer to.
    pub async fn note_paths(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT path FROM notes WHERE path != ''")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    pub async fn update_note_text(&self, id: i64, text: &str) -> Result<()> {
        sqlx::query("UPDATE notes SET text = ?, norm = ?, updated_at = ? WHERE id = ?")
            .bind(text)
            .bind(notes::normalize(text))
            .bind(unix(SystemTime::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_notes(&self, ids: &[i64]) -> Result<u64> {
        let mut n = 0;
        for id in ids {
            n += sqlx::query("DELETE FROM notes WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        Ok(n)
    }

    pub async fn delete_notes_for_path(&self, path: &str) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM notes WHERE path = ?")
            .bind(path)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    pub async fn clear_notes(&self) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM notes").execute(&self.pool).await?.rows_affected())
    }

    /// Keeps the most recently confirmed notes per place: 50 project-wide,
    /// 10 per file, 5 per symbol.
    pub async fn prune_notes(&self) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM notes WHERE id IN (
                 SELECT id FROM (
                     SELECT id, scope, ROW_NUMBER() OVER (
                         PARTITION BY scope, path, symbol ORDER BY updated_at DESC, id DESC
                     ) AS rank
                     FROM notes
                 )
                 WHERE (scope = 'project' AND rank > 50)
                    OR (scope = 'file' AND rank > 10)
                    OR (scope = 'symbol' AND rank > 5)
             )",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
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

fn note_from_row(row: &sqlx::sqlite::SqliteRow) -> Option<Note> {
    Some(Note {
        id: row.get("id"),
        scope: NoteScope::parse(row.get("scope"))?,
        path: row.get("path"),
        symbol: row.get("symbol"),
        text: row.get("text"),
    })
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

    fn key(hash: &str, version: i64) -> CacheKey {
        CacheKey {
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
    async fn reviews_round_trip() {
        let store = Store::open_in_memory().await.unwrap();
        let k = key("abc", 1);
        assert_eq!(store.review(&k).await.unwrap(), None);
        store.put_review(&k, "P::f", "[]", "sonnet").await.unwrap();
        assert_eq!(store.review(&k).await.unwrap().as_deref(), Some("[]"));
        assert_eq!(store.doc(&k).await.unwrap(), None, "separate from docs");
    }

    #[tokio::test]
    async fn dismissals_are_idempotent() {
        let store = Store::open_in_memory().await.unwrap();
        store.dismiss("P::f", "fp1").await.unwrap();
        store.dismiss("P::f", "fp1").await.unwrap();
        store.dismiss("g", "fp2").await.unwrap();
        let mut d = store.dismissals().await.unwrap();
        d.sort();
        assert_eq!(d, [("P::f".into(), "fp1".into()), ("g".into(), "fp2".into())]);
    }

    #[tokio::test]
    async fn counts_recent_runs_by_kind() {
        let store = Store::open_in_memory().await.unwrap();
        let now = SystemTime::now();
        for (kind, ago) in [("review", 10), ("review", 7200), ("doc", 10)] {
            store
                .record_run(NewRun {
                    kind,
                    subject: "f",
                    model: "m",
                    started_at: now - Duration::from_secs(ago),
                    duration: Duration::from_secs(1),
                    cost_usd: None,
                    error: None,
                })
                .await
                .unwrap();
        }
        let hour_ago = now - Duration::from_secs(3600);
        assert_eq!(store.runs_since("review", hour_ago).await.unwrap(), 1);
        assert_eq!(store.runs_since("doc", hour_ago).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn notes_dedupe_and_scope_their_place() {
        let store = Store::open_in_memory().await.unwrap();
        use NoteScope::*;
        store.add_note(Project, "src/a.rs", "f", "Uses anyhow.", "f").await.unwrap();
        store.add_note(Project, "src/b.rs", "g", "uses  ANYHOW", "g").await.unwrap(); // duplicate
        store.add_note(File, "src/a.rs", "f", "byte offsets", "f").await.unwrap();
        store.add_note(Symbol, "src/a.rs", "f", "never 0", "f").await.unwrap();
        store.add_note(Symbol, "src/a.rs", "g", "other symbol", "g").await.unwrap();
        store.add_note(File, "src/b.rs", "", "other file", "x").await.unwrap();

        let all = store.all_notes().await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!((all[0].path.as_str(), all[0].symbol.as_str()), ("", ""), "project notes drop their place");

        let texts = |ns: Vec<Note>| ns.into_iter().map(|n| n.text).collect::<Vec<_>>();
        assert_eq!(
            texts(store.notes_for("src/a.rs", "f").await.unwrap()),
            ["Uses anyhow.", "byte offsets", "never 0"]
        );
        assert_eq!(texts(store.notes_for("src/b.rs", "f").await.unwrap()), ["Uses anyhow.", "other file"]);
    }

    #[tokio::test]
    async fn notes_edit_delete_clear_and_paths() {
        let store = Store::open_in_memory().await.unwrap();
        use NoteScope::*;
        store.add_note(Symbol, "src/a.rs", "f", "one", "f").await.unwrap();
        store.add_note(Symbol, "src/a.rs", "g", "two", "g").await.unwrap();
        store.add_note(File, "src/b.rs", "", "three", "x").await.unwrap();
        let ids: Vec<i64> = store.all_notes().await.unwrap().iter().map(|n| n.id).collect();

        store.update_note_text(ids[0], "uno").await.unwrap();
        assert_eq!(store.all_notes().await.unwrap()[0].text, "uno");
        let mut syms = store.symbol_notes_in("src/a.rs").await.unwrap();
        syms.sort();
        assert_eq!(syms, [(ids[0], "f".into()), (ids[1], "g".into())]);
        let mut paths = store.note_paths().await.unwrap();
        paths.sort();
        assert_eq!(paths, ["src/a.rs", "src/b.rs"]);

        assert_eq!(store.delete_notes(&[ids[1], 999]).await.unwrap(), 1);
        assert_eq!(store.delete_notes_for_path("src/b.rs").await.unwrap(), 1);
        assert_eq!(store.clear_notes().await.unwrap(), 1);
        assert!(store.all_notes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_keeps_the_newest_per_place() {
        let store = Store::open_in_memory().await.unwrap();
        for i in 0..7 {
            store.add_note(NoteScope::Symbol, "a.rs", "f", &format!("n{i}"), "f").await.unwrap();
        }
        store.add_note(NoteScope::Symbol, "a.rs", "g", "g0", "g").await.unwrap();
        assert_eq!(store.prune_notes().await.unwrap(), 2);
        let left: Vec<_> = store.all_notes().await.unwrap().into_iter().map(|n| n.text).collect();
        assert_eq!(left, ["n2", "n3", "n4", "n5", "n6", "g0"]);
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
