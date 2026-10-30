//! SQLite-backed persistence for quests (the auto-detected todo list).
//!
//! Two tables live in `<data dir>/plugin-data/@ryu+quests/quests.db` (the path is provided by the host so it
//! honours data-folder relocation):
//!   - `quests`     — the todo/quest definitions (title, completion condition,
//!     status, and the latest detection suggestion as embedded JSON).
//!   - `detections` — an append-only log of every time the judge thought a quest
//!     was done (confidence + reason + a short evidence snippet), so the history
//!     of *why* something was auto-completed survives.
//!
//! A broadcast channel fans freshly-changed quests out to SSE subscribers (the
//! desktop quests page + the island completion chip).
//!
//! At rest (see [`crate::seal_keys`]): `quests.json` (the whole quest, including
//! the judge's suggestion evidence and reason), `detections.reason`,
//! `detections.evidence` and `scratchpad.text` are sealed per value
//! (`encb:v1:`, store `quests.db`, bound to the row id). Ids, timestamps, the
//! `kind` enum, confidences and dispositions stay plaintext. Legacy plaintext
//! rows are sealed when the store opens (cursor-paginated, resumable), then
//! freed pages are scrubbed. Every connection runs with `secure_delete = ON`.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use ryu_crypto::content_sqlite::{
    enable_secure_delete, finish_store, migrate_columns, run_pending_vacuum, ColumnPlan, KeyKind,
};
use ryu_crypto::ContentContext;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use crate::seal_keys::{sqlite_store_seal, PluginKeys, StoreSeal};
use crate::{Quest, QuestEvent};

/// The registered content-store name (part of every key derivation; never rename).
pub const STORE: &str = "quests.db";

fn quest_ctx(id: &str) -> ContentContext {
    ContentContext::for_id("quests.json", id)
}

fn detection_ctx(column: &str, id: i64) -> ContentContext {
    ContentContext::for_id(format!("detections.{column}"), id)
}

fn scratchpad_ctx() -> ContentContext {
    ContentContext::for_id("scratchpad.text", 1)
}

/// The sealed columns, for the open-time migration.
pub fn column_plans() -> Result<Vec<ColumnPlan>> {
    Ok(vec![
        ColumnPlan::text("quests", "json", "id", KeyKind::Text)?,
        ColumnPlan::text("detections", "reason", "id", KeyKind::Integer)?,
        ColumnPlan::text("detections", "evidence", "id", KeyKind::Integer)?,
        ColumnPlan::text("scratchpad", "text", "id", KeyKind::Integer)?,
    ])
}

/// SQLite-backed quest store. Cheap to clone (wraps `Arc`s).
#[derive(Clone)]
pub struct QuestStore {
    conn: Arc<Mutex<Connection>>,
    tx: broadcast::Sender<QuestEvent>,
    seal: StoreSeal,
    unreadable: Arc<AtomicU64>,
}

impl QuestStore {
    /// Open (or create) the store at a specific path and run migrations. The
    /// caller (host) provides the path so data-folder relocation is honoured.
    /// Resolves this process's sealing key (see [`crate::seal_keys`]) with the
    /// store's directory as the plugin data dir.
    pub fn open(path: PathBuf) -> Result<Self> {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let keys = PluginKeys::resolve(crate::PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, &keys)
    }

    /// [`Self::open`] with explicit keys.
    pub fn open_with_keys(path: PathBuf, keys: &PluginKeys) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db dir {}", parent.display()))?;
        }
        let mut conn = Connection::open(&path)
            .with_context(|| format!("opening quests db {}", path.display()))?;
        enable_secure_delete(&conn)?;
        Self::init_schema(&conn)?;
        let seal = sqlite_store_seal(keys, &conn, STORE)?;
        if let StoreSeal::Sealing(cipher) = &seal {
            if let Err(error) = run_pending_vacuum(&conn, &path, STORE) {
                tracing::warn!("quests.db: deferred compaction failed: {error:#}");
            }
            let report = migrate_columns(&mut conn, cipher, &column_plans()?)
                .context("sealing legacy quests.db rows")?;
            if report.unreadable > 0 {
                tracing::warn!(
                    count = report.unreadable,
                    "quests.db: values that do not open were left untouched"
                );
            }
            if report.sealed > 0 {
                tracing::info!(count = report.sealed, "quests.db: sealed legacy values");
                if let Err(error) = finish_store(&conn, &path, STORE) {
                    tracing::warn!("quests.db: scrubbing freed pages failed: {error:#}");
                }
            }
        }
        let (tx, _rx) = broadcast::channel(128);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tx,
            seal,
            unreadable: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The sealing health item: `{state, detail?, unreadable_rows}`.
    pub fn seal_health(&self) -> serde_json::Value {
        let mut health = self.seal.health();
        health["unreadable_rows"] = self.unreadable.load(Ordering::Relaxed).into();
        health
    }

    fn open_quest(&self, id: &str, stored: &str) -> Result<Quest> {
        let json = self.seal.open_text(stored, &quest_ctx(id))?;
        serde_json::from_str(&json).context("deserializing quest")
    }

    fn seal_quest(&self, quest: &Quest) -> Result<String> {
        let json = serde_json::to_string(quest).context("serializing quest")?;
        self.seal.seal_text(&json, &quest_ctx(&quest.id))
    }

    fn skip_unreadable(&self, id: &str, error: &anyhow::Error) {
        let total = self.unreadable.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(quest = %id, total, "skipping a quest that does not open: {error:#}");
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS quests (
                 id          TEXT PRIMARY KEY,
                 json        TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS detections (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 quest_id    TEXT NOT NULL,
                 detected_at TEXT NOT NULL,
                 confidence  INTEGER NOT NULL,
                 reason      TEXT NOT NULL,
                 evidence    TEXT,
                 disposition TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_detections_quest
                 ON detections(quest_id, id DESC);
             CREATE TABLE IF NOT EXISTS scratchpad (
                 id          INTEGER PRIMARY KEY CHECK (id = 1),
                 text        TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );",
        )
        .context("initializing quests schema")?;

        // `kind` is denormalised out of the JSON blob purely so the list query can
        // filter in SQL. The JSON stays authoritative — this column is written
        // from it on every upsert, never read back into the struct.
        //
        // Added after the table shipped, so this is an idempotent migration: an
        // `ALTER TABLE ... ADD COLUMN` on an existing column errors with
        // "duplicate column name", which is the expected steady state and is
        // swallowed. Any other error is a real failure and is propagated.
        if let Err(err) = conn.execute(
            "ALTER TABLE quests ADD COLUMN kind TEXT NOT NULL DEFAULT 'task'",
            [],
        ) {
            let msg = err.to_string();
            if !msg.contains("duplicate column name") {
                return Err(err).context("adding quests.kind column");
            }
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_quests_kind ON quests(kind);")
            .context("indexing quests.kind")?;
        Ok(())
    }

    // ---- quests -----------------------------------------------------------

    /// Insert or replace a quest definition, then broadcast an `Updated` event.
    pub async fn upsert_quest(&self, quest: &Quest) -> Result<()> {
        let json = self.seal_quest(quest)?;
        {
            let conn = self.conn.lock().await;
            conn.execute(
                "INSERT INTO quests (id, json, created_at, updated_at, kind)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET json = ?2, updated_at = ?4, kind = ?5",
                params![
                    quest.id,
                    json,
                    quest.created_at,
                    quest.updated_at,
                    quest.kind.as_str()
                ],
            )
            .context("upserting quest")?;
        }
        self.broadcast(QuestEvent::Updated {
            quest: quest.clone(),
        });
        Ok(())
    }

    /// Apply `mutate` to the stored quest under the connection lock and write it
    /// back with `UPDATE` only, then broadcast an `Updated` event. Returns
    /// `Ok(None)` when the row is gone or `pred` rejects the current value, so a
    /// caller holding a stale snapshot (a judge pass that awaited a model call)
    /// can never resurrect a deleted quest or revert a change made meanwhile.
    pub async fn update_quest_if<P, M>(&self, id: &str, pred: P, mutate: M) -> Result<Option<Quest>>
    where
        P: FnOnce(&Quest) -> bool,
        M: FnOnce(&mut Quest),
    {
        let quest = {
            let conn = self.conn.lock().await;
            let Some(json) = conn
                .query_row(
                    "SELECT json FROM quests WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("reading quest")?
            else {
                return Ok(None);
            };
            let mut quest = self.open_quest(id, &json)?;
            if !pred(&quest) {
                return Ok(None);
            }
            mutate(&mut quest);
            let json = self.seal_quest(&quest)?;
            conn.execute(
                "UPDATE quests SET json = ?2, updated_at = ?3, kind = ?4 WHERE id = ?1",
                params![id, json, quest.updated_at, quest.kind.as_str()],
            )
            .context("updating quest")?;
            quest
        };
        self.broadcast(QuestEvent::Updated {
            quest: quest.clone(),
        });
        Ok(Some(quest))
    }

    /// Fetch a quest by id.
    pub async fn get_quest(&self, id: &str) -> Result<Option<Quest>> {
        let conn = self.conn.lock().await;
        let json = conn
            .query_row(
                "SELECT json FROM quests WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading quest")?;
        match json {
            Some(j) => Ok(Some(self.open_quest(id, &j)?)),
            None => Ok(None),
        }
    }

    /// List all quests, newest first.
    pub async fn list_quests(&self) -> Result<Vec<Quest>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT id, json FROM quests ORDER BY created_at DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        self.collect_quests(rows)
    }

    /// List quests of one kind, newest first.
    pub async fn list_quests_of_kind(&self, kind: &str) -> Result<Vec<Quest>> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT id, json FROM quests WHERE kind = ?1 ORDER BY created_at DESC")?;
        let rows = stmt.query_map(params![kind], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        self.collect_quests(rows)
    }

    /// Open listed rows; a row that does not open (or parse) is skipped and
    /// counted, never fatal for the whole list.
    fn collect_quests(
        &self,
        rows: impl Iterator<Item = rusqlite::Result<(String, String)>>,
    ) -> Result<Vec<Quest>> {
        let mut out = Vec::new();
        for row in rows {
            let (id, json) = row?;
            match self.open_quest(&id, &json) {
                Ok(quest) => out.push(quest),
                Err(error) => self.skip_unreadable(&id, &error),
            }
        }
        Ok(out)
    }

    // ---- scratchpad -------------------------------------------------------

    /// The freeform brain-dump buffer. Empty string when never written.
    pub async fn get_scratchpad(&self) -> Result<String> {
        let conn = self.conn.lock().await;
        let text = conn
            .query_row("SELECT text FROM scratchpad WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .context("reading scratchpad")?;
        match text {
            Some(text) => self.seal.open_text(&text, &scratchpad_ctx()),
            None => Ok(String::new()),
        }
    }

    /// Overwrite the brain-dump buffer.
    pub async fn set_scratchpad(&self, text: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let text = self.seal.seal_text(text, &scratchpad_ctx())?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO scratchpad (id, text, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET text = ?1, updated_at = ?2",
            params![text, now],
        )
        .context("writing scratchpad")?;
        Ok(())
    }

    /// Delete a quest and its detection history, then broadcast a `Deleted` event.
    /// Returns true when removed.
    pub async fn delete_quest(&self, id: &str) -> Result<bool> {
        let removed = {
            let conn = self.conn.lock().await;
            let n = conn.execute("DELETE FROM quests WHERE id = ?1", params![id])?;
            conn.execute("DELETE FROM detections WHERE quest_id = ?1", params![id])?;
            n > 0
        };
        if removed {
            self.broadcast(QuestEvent::Deleted { id: id.to_string() });
        }
        Ok(removed)
    }

    // ---- detections -------------------------------------------------------

    /// Append a detection record (the audit trail of why a quest was flagged done).
    pub async fn insert_detection(
        &self,
        quest_id: &str,
        confidence: u8,
        reason: &str,
        evidence: Option<&str>,
        disposition: &str,
    ) -> Result<()> {
        self.seal.ensure_writable()?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().await;
        // The AUTOINCREMENT id is only known after the insert, and the sealed
        // values bind it: insert empty placeholders, then write the sealed values
        // under the new id, in one transaction (no plaintext ever lands).
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO detections (quest_id, detected_at, confidence, reason, evidence, disposition)
             VALUES (?1, ?2, ?3, '', NULL, ?4)",
            params![quest_id, now, confidence as i64, disposition],
        )
        .context("inserting detection")?;
        let id = tx.last_insert_rowid();
        let reason = self.seal.seal_text(reason, &detection_ctx("reason", id))?;
        let evidence = self
            .seal
            .seal_opt(evidence, &detection_ctx("evidence", id))?;
        tx.execute(
            "UPDATE detections SET reason = ?2, evidence = ?3 WHERE id = ?1",
            params![id, reason, evidence],
        )
        .context("sealing detection")?;
        tx.commit()?;
        Ok(())
    }

    /// Broadcast a quest event (suggested / completed) to SSE subscribers.
    pub fn broadcast(&self, event: QuestEvent) {
        // A send error just means no live SSE subscribers — not a failure.
        let _ = self.tx.send(event);
    }

    /// Subscribe to live quest events (used by the SSE endpoint + island).
    pub fn subscribe(&self) -> broadcast::Receiver<QuestEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal_keys::KeySource;
    use base64::Engine as _;
    use ryu_crypto::{ContentCipher, PluginCipher};

    const SECRET: &str = "zebra-quest-marker";

    fn keys(n: u8) -> PluginKeys {
        let subkey = base64::engine::general_purpose::STANDARD.encode([n; 32]);
        PluginKeys::sealing(
            PluginCipher::from_subkey_b64("@ryu/quests", &subkey).unwrap(),
            KeySource::Delivered,
        )
    }

    fn db_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("ryu-quests-seal-{}", uuid::Uuid::new_v4().simple()))
            .join("quests.db")
    }

    fn quest(id: &str, title: &str) -> Quest {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": title,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn disk_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = std::fs::read(path).unwrap_or_default();
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        bytes.extend(std::fs::read(PathBuf::from(wal)).unwrap_or_default());
        bytes
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    #[tokio::test]
    async fn content_is_sealed_on_disk_and_reads_back() {
        let path = db_path();
        let store = QuestStore::open_with_keys(path.clone(), &keys(1)).unwrap();
        store.upsert_quest(&quest("q1", SECRET)).await.unwrap();
        store
            .insert_detection(
                "q1",
                90,
                &format!("reason {SECRET}"),
                Some(&format!("ev {SECRET}")),
                "suggested",
            )
            .await
            .unwrap();
        store
            .set_scratchpad(&format!("pad {SECRET}"))
            .await
            .unwrap();
        assert_eq!(store.get_quest("q1").await.unwrap().unwrap().title, SECRET);
        assert_eq!(store.list_quests().await.unwrap().len(), 1);
        assert_eq!(
            store.get_scratchpad().await.unwrap(),
            format!("pad {SECRET}")
        );
        assert!(!contains(&disk_bytes(&path), SECRET));
        let conn = store.conn.lock().await;
        let cipher = keys(1).content_cipher(STORE).unwrap();
        ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap())
            .unwrap();
    }

    #[tokio::test]
    async fn legacy_plaintext_is_migrated_once_and_still_reads() {
        let path = db_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            QuestStore::init_schema(&conn).unwrap();
            let json = serde_json::to_string(&quest("old", SECRET)).unwrap();
            conn.execute(
                "INSERT INTO quests (id, json, created_at, updated_at) VALUES ('old', ?1, 'a', 'a')",
                params![json],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO detections (quest_id, detected_at, confidence, reason, evidence, disposition)
                 VALUES ('old', 'a', 80, ?1, ?1, 'suggested')",
                params![SECRET],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO scratchpad (id, text, updated_at) VALUES (1, ?1, 'a')",
                params![SECRET],
            )
            .unwrap();
        }
        let store = QuestStore::open_with_keys(path.clone(), &keys(2)).unwrap();
        assert_eq!(store.get_quest("old").await.unwrap().unwrap().title, SECRET);
        assert_eq!(store.get_scratchpad().await.unwrap(), SECRET);
        let snapshot = |conn: &Connection| -> Vec<String> {
            let mut stmt = conn
                .prepare("SELECT json FROM quests UNION ALL SELECT reason FROM detections")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let first = {
            let conn = store.conn.lock().await;
            let cipher = keys(2).content_cipher(STORE).unwrap();
            ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap())
                .unwrap();
            snapshot(&conn)
        };
        drop(store);
        assert!(!contains(&disk_bytes(&path), SECRET));
        // A second open changes nothing.
        let store = QuestStore::open_with_keys(path.clone(), &keys(2)).unwrap();
        assert_eq!(snapshot(&*store.conn.lock().await), first);
    }

    #[tokio::test]
    async fn a_value_copied_to_another_row_does_not_open_and_lists_skip_it() {
        let path = db_path();
        let store = QuestStore::open_with_keys(path, &keys(3)).unwrap();
        store.upsert_quest(&quest("a", "first")).await.unwrap();
        store.upsert_quest(&quest("b", "second")).await.unwrap();
        {
            let conn = store.conn.lock().await;
            conn.execute(
                "UPDATE quests SET json = (SELECT json FROM quests WHERE id = 'a') WHERE id = 'b'",
                [],
            )
            .unwrap();
        }
        assert!(store.get_quest("b").await.is_err());
        let listed = store.list_quests().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(store.seal_health()["unreadable_rows"], 1);
    }

    #[tokio::test]
    async fn another_key_or_no_key_refuses_writes_and_keeps_data() {
        let path = db_path();
        let store = QuestStore::open_with_keys(path.clone(), &keys(4)).unwrap();
        store.upsert_quest(&quest("a", "kept")).await.unwrap();
        drop(store);
        let wrong = QuestStore::open_with_keys(path.clone(), &keys(5)).unwrap();
        assert_eq!(wrong.seal_health()["state"], "unhealthy");
        assert!(wrong.upsert_quest(&quest("c", "x")).await.is_err());
        assert!(wrong.set_scratchpad("x").await.is_err());
        drop(wrong);
        let none = QuestStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap();
        assert_eq!(none.seal_health()["state"], "unhealthy");
        assert!(none.upsert_quest(&quest("c", "x")).await.is_err());
        drop(none);
        let back = QuestStore::open_with_keys(path, &keys(4)).unwrap();
        assert_eq!(back.get_quest("a").await.unwrap().unwrap().title, "kept");
    }

    #[tokio::test]
    async fn a_never_sealed_store_without_a_key_keeps_working_in_compat_mode() {
        let path = db_path();
        let store = QuestStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap();
        assert_eq!(store.seal_health()["state"], "compat");
        store.upsert_quest(&quest("a", "plain")).await.unwrap();
        assert_eq!(store.get_quest("a").await.unwrap().unwrap().title, "plain");
        drop(store);
        // The next start with a key seals what compat mode wrote.
        let store = QuestStore::open_with_keys(path, &keys(6)).unwrap();
        let raw: String = store
            .conn
            .lock()
            .await
            .query_row("SELECT json FROM quests WHERE id = 'a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(ContentCipher::is_sealed_text(&raw));
    }
}
