//! Integration tests for the `ryu-quests` capability crate, reaching only its
//! PUBLIC surface (`QuestStore`, `QuestEngine`, the `QuestsHost` trait, and the
//! `api::*` handlers). No production source is edited by these tests.
//!
//! Hermetic by construction:
//!   - persistence uses a unique temp SQLite file per test (no shared data dir);
//!   - the cross-cutting host is a `FakeHost` (in-memory prefs, recorded jobs,
//!     scriptable Shadow responses) — no Core, no scheduler, no real preferences;
//!   - the Gateway judge call is served by a loopback mock (`spawn_gateway`) that
//!     returns a canned chat-completion — no network, no model.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use ryu_quests::api::{self, DetectionConfigBody, JudgeBody, QuestBody, QuestsCtx};
use ryu_quests::{
    board_columns, CompletionSource, DetectionMode, Quest, QuestEngine, QuestEvent, QuestStatus,
    QuestStore, QuestsHost, DETECTION_INTERVAL_PREF, DETECTION_MODE_PREF, JUDGE_EFFORT_PREF,
    JUDGE_MODEL_PREF,
};

// ── temp store ──────────────────────────────────────────────────────────────

/// A unique temp dir + db path for one test (cleaned up by `cleanup`).
fn temp_db() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ryu-quests-it-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    dir.join("quests.db")
}

fn cleanup(db: &PathBuf) {
    if let Some(parent) = db.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

async fn open_store() -> (QuestStore, PathBuf) {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).expect("open store");
    (store, db)
}

// ── fake host ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct HostState {
    prefs: HashMap<String, String>,
    shadow: HashMap<String, Value>,
    jobs: Vec<(String, bool)>,
    deleted_jobs: Vec<String>,
}

struct FakeHost {
    state: Mutex<HostState>,
    gateway_url: String,
    sync_fails: bool,
}

impl FakeHost {
    fn new(gateway_url: String) -> Self {
        Self {
            state: Mutex::new(HostState::default()),
            gateway_url,
            sync_fails: false,
        }
    }

    fn failing() -> Self {
        Self {
            state: Mutex::new(HostState::default()),
            gateway_url: "http://127.0.0.1:1".to_string(),
            sync_fails: true,
        }
    }

    fn set_pref(&self, k: &str, v: &str) {
        self.state
            .lock()
            .unwrap()
            .prefs
            .insert(k.to_string(), v.to_string());
    }

    fn set_shadow(&self, tool: &str, v: Value) {
        self.state
            .lock()
            .unwrap()
            .shadow
            .insert(tool.to_string(), v);
    }

    fn jobs(&self) -> Vec<(String, bool)> {
        self.state.lock().unwrap().jobs.clone()
    }

    fn deleted_jobs(&self) -> Vec<String> {
        self.state.lock().unwrap().deleted_jobs.clone()
    }
}

#[async_trait]
impl QuestsHost for FakeHost {
    async fn pref_get(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().prefs.get(key).cloned()
    }

    async fn pref_set(&self, key: &str, value: &str) -> Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .prefs
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn shadow_call(&self, tool: &str, _args: Value) -> Option<Value> {
        self.state.lock().unwrap().shadow.get(tool).cloned()
    }

    fn gateway_url(&self) -> String {
        self.gateway_url.clone()
    }

    fn gateway_token(&self) -> Option<String> {
        None
    }

    fn default_judge_model(&self) -> String {
        "fake-default-model".to_string()
    }

    fn sync_backing_job(
        &self,
        quest_id: &str,
        _title: &str,
        _interval: &str,
        open: bool,
    ) -> Result<(), String> {
        if self.sync_fails {
            return Err("sync failed".to_string());
        }
        self.state
            .lock()
            .unwrap()
            .jobs
            .push((quest_id.to_string(), open));
        Ok(())
    }

    fn delete_backing_job(&self, quest_id: &str) {
        self.state
            .lock()
            .unwrap()
            .deleted_jobs
            .push(quest_id.to_string());
    }
}

// ── mock gateway ────────────────────────────────────────────────────────────

/// Spawn a loopback chat-completions endpoint returning `reply` as the assistant
/// message content. Returns the base URL (`http://127.0.0.1:<port>`).
async fn spawn_gateway(reply: String) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let reply = reply.clone();
            async move { Json(json!({ "choices": [{ "message": { "content": reply } }] })) }
        }),
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind mock gateway");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Build an engine over a fresh temp store and the given host.
fn engine_with(store: QuestStore, host: Arc<dyn QuestsHost>) -> QuestEngine {
    QuestEngine::new(store, host, reqwest::Client::new())
}

/// Drain all currently-available broadcast events.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<QuestEvent>) -> Vec<QuestEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

// ── store: persistence ──────────────────────────────────────────────────────

fn mk_quest(id: &str, created_at: &str) -> Quest {
    Quest {
        id: id.into(),
        title: id.into(),
        detail: None,
        completion_condition: String::new(),
        status: QuestStatus::Open,
        created_at: created_at.into(),
        updated_at: created_at.into(),
        completed_at: None,
        completion_source: None,
        last_judged_at: None,
        snoozed_until: None,
        suggestion: None,
    }
}

#[tokio::test]
async fn store_upsert_get_roundtrip_and_missing() {
    let (store, db) = open_store().await;
    assert!(store.list_quests().await.unwrap().is_empty());
    assert!(store.get_quest("nope").await.unwrap().is_none());

    let q = mk_quest("q1", "2021-01-01T00:00:00Z");
    store.upsert_quest(&q).await.unwrap();
    let got = store.get_quest("q1").await.unwrap().expect("present");
    assert_eq!(got.id, "q1");
    assert_eq!(got.status, QuestStatus::Open);

    // Upsert again with a changed field replaces (ON CONFLICT update).
    let mut q2 = q.clone();
    q2.title = "renamed".into();
    q2.updated_at = "2021-02-01T00:00:00Z".into();
    store.upsert_quest(&q2).await.unwrap();
    assert_eq!(store.get_quest("q1").await.unwrap().unwrap().title, "renamed");
    // Still a single row.
    assert_eq!(store.list_quests().await.unwrap().len(), 1);
    cleanup(&db);
}

#[tokio::test]
async fn store_list_is_newest_created_first() {
    let (store, db) = open_store().await;
    store
        .upsert_quest(&mk_quest("old", "2020-01-01T00:00:00Z"))
        .await
        .unwrap();
    store
        .upsert_quest(&mk_quest("new", "2023-01-01T00:00:00Z"))
        .await
        .unwrap();
    store
        .upsert_quest(&mk_quest("mid", "2021-06-01T00:00:00Z"))
        .await
        .unwrap();
    let ids: Vec<String> = store
        .list_quests()
        .await
        .unwrap()
        .into_iter()
        .map(|q| q.id)
        .collect();
    assert_eq!(ids, vec!["new", "mid", "old"]);
    cleanup(&db);
}

#[tokio::test]
async fn store_delete_removes_and_reports() {
    let (store, db) = open_store().await;
    store
        .upsert_quest(&mk_quest("q1", "2021-01-01T00:00:00Z"))
        .await
        .unwrap();
    // A detection row exists for the audit trail; delete must cascade it.
    store
        .insert_detection("q1", 90, "done", Some("evidence"), "auto_completed")
        .await
        .unwrap();

    assert!(store.delete_quest("q1").await.unwrap());
    assert!(store.get_quest("q1").await.unwrap().is_none());
    // Deleting a missing quest reports false (and broadcasts nothing).
    assert!(!store.delete_quest("q1").await.unwrap());
    cleanup(&db);
}

#[tokio::test]
async fn store_broadcasts_updated_and_deleted() {
    let (store, db) = open_store().await;
    let mut rx = store.subscribe();
    store
        .upsert_quest(&mk_quest("q1", "2021-01-01T00:00:00Z"))
        .await
        .unwrap();
    store.delete_quest("q1").await.unwrap();
    let events = drain(&mut rx);
    assert!(matches!(events[0], QuestEvent::Updated { .. }));
    assert!(matches!(events[1], QuestEvent::Deleted { .. }));
    cleanup(&db);
}

// ── engine: CRUD ────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_quest_trims_and_syncs_open_job() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());

    let q = engine
        .create_quest(
            "  Ship the release  ".into(),
            Some("   ".into()), // whitespace-only detail is dropped
            "  PR merged  ".into(),
        )
        .await
        .expect("created");
    assert_eq!(q.title, "Ship the release");
    assert_eq!(q.completion_condition, "PR merged");
    assert!(q.detail.is_none());
    assert_eq!(q.status, QuestStatus::Open);
    assert!(q.id.starts_with("quest_"));
    // Backing job created, enabled (open).
    assert_eq!(host.jobs(), vec![(q.id.clone(), true)]);
    cleanup(&db);
}

#[tokio::test]
async fn create_quest_surfaces_backing_job_failure() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::failing());
    let engine = engine_with(store, host);
    let err = engine
        .create_quest("t".into(), None, String::new())
        .await
        .expect_err("job sync failure propagates");
    assert!(err.contains("sync failed"));
    cleanup(&db);
}

#[tokio::test]
async fn update_quest_edits_or_reports_missing() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);

    assert!(engine
        .update_quest("ghost", "x".into(), None, String::new())
        .await
        .unwrap()
        .is_none());

    let q = engine
        .create_quest("orig".into(), None, String::new())
        .await
        .unwrap();
    let updated = engine
        .update_quest(&q.id, " new title ".into(), Some("d".into()), " cond ".into())
        .await
        .unwrap()
        .expect("present");
    assert_eq!(updated.title, "new title");
    assert_eq!(updated.detail.as_deref(), Some("d"));
    assert_eq!(updated.completion_condition, "cond");
    cleanup(&db);
}

#[tokio::test]
async fn delete_quest_removes_job_and_row() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    assert!(engine.delete_quest(&q.id).await.unwrap());
    assert_eq!(host.deleted_jobs(), vec![q.id.clone()]);
    assert!(!engine.delete_quest(&q.id).await.unwrap());
    cleanup(&db);
}

// ── engine: manual state transitions ────────────────────────────────────────

#[tokio::test]
async fn complete_quest_manual_sets_source_and_disables_job() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store.clone(), host.clone());
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    let mut rx = store.subscribe();

    let done = engine.complete_quest(&q.id, false).await.unwrap().unwrap();
    assert_eq!(done.status, QuestStatus::Done);
    assert_eq!(done.completion_source, Some(CompletionSource::Manual));
    assert!(done.completed_at.is_some());
    // Job re-synced with open=false.
    assert!(host.jobs().iter().any(|(id, open)| id == &q.id && !*open));
    // Completed(auto:false) broadcast.
    assert!(drain(&mut rx)
        .iter()
        .any(|e| matches!(e, QuestEvent::Completed { auto: false, .. })));

    // Missing quest → None.
    assert!(engine.complete_quest("ghost", false).await.unwrap().is_none());
    cleanup(&db);
}

#[tokio::test]
async fn accept_suggestion_marks_detected() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    // accept == complete_quest(detected = true)
    let done = engine.complete_quest(&q.id, true).await.unwrap().unwrap();
    assert_eq!(done.completion_source, Some(CompletionSource::Detected));
    cleanup(&db);
}

#[tokio::test]
async fn dismiss_quest_and_reopen_cycle() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();

    let dismissed = engine.dismiss_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(dismissed.status, QuestStatus::Dismissed);
    assert!(dismissed.suggestion.is_none());

    let reopened = engine.reopen_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(reopened.status, QuestStatus::Open);
    assert!(reopened.completed_at.is_none());
    assert!(reopened.completion_source.is_none());

    assert!(engine.dismiss_quest("ghost").await.unwrap().is_none());
    assert!(engine.reopen_quest("ghost").await.unwrap().is_none());
    cleanup(&db);
}

#[tokio::test]
async fn dismiss_suggestion_snoozes_and_clears() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    let out = engine.dismiss_suggestion(&q.id).await.unwrap().unwrap();
    assert!(out.suggestion.is_none());
    let until = out.snoozed_until.expect("snoozed");
    let parsed = chrono::DateTime::parse_from_rfc3339(&until).unwrap();
    // Snooze is ~1h in the future.
    assert!(parsed > chrono::Utc::now() + chrono::Duration::minutes(50));

    assert!(engine.dismiss_suggestion("ghost").await.unwrap().is_none());
    cleanup(&db);
}

// ── engine: judge — early-return no-ops ──────────────────────────────────────

#[tokio::test]
async fn judge_skips_non_open_quest() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    engine.complete_quest(&q.id, false).await.unwrap();
    // Done quest is never judged.
    assert!(engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap()
        .is_none());
    cleanup(&db);
}

#[tokio::test]
async fn judge_skips_when_detection_off() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    host.set_pref(DETECTION_MODE_PREF, "off");
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    assert!(engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap()
        .is_none());
    cleanup(&db);
}

#[tokio::test]
async fn judge_skips_while_snoozed() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    // Default mode (auto_high) — snooze branch is reached only past the mode gate.
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    // Snooze it ~1h forward.
    engine.dismiss_suggestion(&q.id).await.unwrap();
    assert!(engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap()
        .is_none());
    cleanup(&db);
}

#[tokio::test]
async fn judge_skips_when_no_context_available() {
    let (store, db) = open_store().await;
    // No external context and Shadow returns nothing → gather_context is None.
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    assert!(engine
        .judge_quest_with_context(&q.id, None)
        .await
        .unwrap()
        .is_none());
    // Unknown quest id → Err (not a skip).
    assert!(engine
        .judge_quest_with_context("ghost", Some("e".into()))
        .await
        .is_err());
    cleanup(&db);
}

// ── engine: judge — verdict → action matrix ─────────────────────────────────

async fn judged_engine(reply: &str, mode: &str) -> (QuestEngine, Quest, PathBuf) {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let base = spawn_gateway(reply.to_string()).await;
    let host = Arc::new(FakeHost::new(base));
    host.set_pref(DETECTION_MODE_PREF, mode);
    // Drive the model through a pref so env is irrelevant (avoids env races).
    host.set_pref(JUDGE_MODEL_PREF, "test-judge");
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("write the report".into(), None, String::new())
        .await
        .unwrap();
    (engine, q, db)
}

#[tokio::test]
async fn judge_suggest_mode_creates_suggestion_not_completion() {
    let (engine, q, db) = judged_engine("MET: yes\nCONFIDENCE: 90\nREASON: report filed", "suggest").await;
    let mut rx = engine.store.subscribe();
    let v = engine
        .judge_quest_with_context(&q.id, Some("the report was filed".into()))
        .await
        .unwrap()
        .expect("verdict");
    assert!(v.met);
    assert_eq!(v.confidence, 90);

    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(after.status, QuestStatus::Open, "suggest never auto-completes");
    let s = after.suggestion.expect("suggestion present");
    assert_eq!(s.confidence, 90);
    assert_eq!(s.reason, "report filed");
    assert!(s.evidence.is_some());
    assert!(after.last_judged_at.is_some());
    // Suggested event fired.
    assert!(drain(&mut rx)
        .iter()
        .any(|e| matches!(e, QuestEvent::Suggested { confidence: 90, .. })));
    cleanup(&db);
}

#[tokio::test]
async fn judge_suggest_mode_does_not_respam_identical_suggestion() {
    let (engine, q, db) = judged_engine("MET: yes\nCONFIDENCE: 77\nREASON: same reason", "suggest").await;
    engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap();
    // Second identical pass: `already` short-circuits the re-broadcast.
    let mut rx = engine.store.subscribe();
    engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap();
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QuestEvent::Suggested { .. })),
        "no repeat Suggested for an unchanged verdict"
    );
    cleanup(&db);
}

#[tokio::test]
async fn judge_auto_high_completes_above_threshold() {
    let (engine, q, db) =
        judged_engine("MET: yes\nCONFIDENCE: 90\nREASON: shipped", "auto_high").await;
    let mut rx = engine.store.subscribe();
    engine
        .judge_quest_with_context(&q.id, Some("shipped it".into()))
        .await
        .unwrap();
    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(after.status, QuestStatus::Done);
    assert_eq!(after.completion_source, Some(CompletionSource::Detected));
    assert!(after.suggestion.is_none());
    assert!(drain(&mut rx)
        .iter()
        .any(|e| matches!(e, QuestEvent::Completed { auto: true, .. })));
    cleanup(&db);
}

#[tokio::test]
async fn judge_auto_high_below_threshold_only_suggests() {
    let (engine, q, db) =
        judged_engine("MET: yes\nCONFIDENCE: 70\nREASON: maybe", "auto_high").await;
    engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap();
    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    // 70 ≥ floor (50) but < HIGH (85): suggest, don't auto-complete.
    assert_eq!(after.status, QuestStatus::Open);
    assert!(after.suggestion.is_some());
    cleanup(&db);
}

#[tokio::test]
async fn judge_auto_all_completes_any_above_floor() {
    let (engine, q, db) =
        judged_engine("MET: yes\nCONFIDENCE: 60\nREASON: good enough", "auto_all").await;
    engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap();
    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(after.status, QuestStatus::Done);
    cleanup(&db);
}

#[tokio::test]
async fn judge_below_confidence_floor_is_ignored() {
    let (engine, q, db) =
        judged_engine("MET: yes\nCONFIDENCE: 40\nREASON: weak", "auto_all").await;
    let v = engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap()
        .unwrap();
    assert!(v.met);
    assert_eq!(v.confidence, 40);
    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    // Below floor (50): no completion, no suggestion, but judged.
    assert_eq!(after.status, QuestStatus::Open);
    assert!(after.suggestion.is_none());
    assert!(after.last_judged_at.is_some());
    cleanup(&db);
}

#[tokio::test]
async fn judge_not_met_takes_no_action() {
    let (engine, q, db) =
        judged_engine("MET: no\nCONFIDENCE: 95\nREASON: not started", "auto_all").await;
    engine
        .judge_quest_with_context(&q.id, Some("evidence".into()))
        .await
        .unwrap();
    let after = engine.store.get_quest(&q.id).await.unwrap().unwrap();
    assert_eq!(after.status, QuestStatus::Open);
    assert!(after.suggestion.is_none());
    cleanup(&db);
}

#[tokio::test]
async fn judge_gathers_context_from_shadow_when_no_external() {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let base = spawn_gateway("MET: yes\nCONFIDENCE: 88\nREASON: done".into()).await;
    let host = Arc::new(FakeHost::new(base));
    host.set_pref(DETECTION_MODE_PREF, "suggest");
    host.set_pref(JUDGE_MODEL_PREF, "test-judge");
    host.set_shadow(
        "shadow__recent_context",
        json!({ "summary": "user finished writing and saved the report" }),
    );
    host.set_shadow(
        "shadow__semantic_search",
        json!({ "text": "earlier draft edits" }),
    );
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("write the report".into(), None, String::new())
        .await
        .unwrap();
    // No external context: gather_context pulls the Shadow evidence.
    let v = engine
        .judge_quest_with_context(&q.id, None)
        .await
        .unwrap()
        .expect("gathered context and judged");
    assert!(v.met);
    cleanup(&db);
}

#[tokio::test]
async fn judge_survives_multibyte_evidence_over_cap() {
    // Regression: gather_context truncated at a raw byte offset, which panics when
    // it lands mid-multibyte-char. Feed >4000 bytes of 3-byte chars.
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let base = spawn_gateway("MET: no\nCONFIDENCE: 10\nREASON: nope".into()).await;
    let host = Arc::new(FakeHost::new(base));
    host.set_pref(DETECTION_MODE_PREF, "suggest");
    host.set_pref(JUDGE_MODEL_PREF, "test-judge");
    let big = "参".repeat(2000); // 6000 bytes, cap at 4000 lands mid-char
    host.set_shadow("shadow__recent_context", json!({ "summary": big }));
    let engine = engine_with(store, host);
    let q = engine
        .create_quest("t".into(), None, String::new())
        .await
        .unwrap();
    // Must not panic.
    let v = engine.judge_quest_with_context(&q.id, None).await.unwrap();
    assert!(v.is_some());
    cleanup(&db);
}

// ── engine: config resolvers ────────────────────────────────────────────────

#[tokio::test]
async fn detection_mode_reads_pref_then_default() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());
    // Unset → default AutoHigh.
    assert_eq!(engine.detection_mode().await, DetectionMode::AutoHigh);
    host.set_pref(DETECTION_MODE_PREF, "auto_all");
    assert_eq!(engine.detection_mode().await, DetectionMode::AutoAll);
    cleanup(&db);
}

#[tokio::test]
async fn resolve_interval_prefers_valid_pref_else_default() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());
    // Valid pref wins.
    host.set_pref(DETECTION_INTERVAL_PREF, "5m");
    assert_eq!(engine.resolve_interval().await, "5m");
    // Invalid pref falls through (env unset in test) to the default.
    host.set_pref(DETECTION_INTERVAL_PREF, "not-a-duration");
    assert_eq!(engine.resolve_interval().await, "2m");
    cleanup(&db);
}

#[tokio::test]
async fn resolve_judge_model_prefers_pref() {
    let (store, db) = open_store().await;
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());
    host.set_pref(JUDGE_MODEL_PREF, "my-model");
    // pref_get/pref_set surface used by config endpoints.
    assert_eq!(engine.pref_get(JUDGE_MODEL_PREF).await.as_deref(), Some("my-model"));
    engine.pref_set(JUDGE_EFFORT_PREF, "high").await.unwrap();
    assert_eq!(engine.pref_get(JUDGE_EFFORT_PREF).await.as_deref(), Some("high"));
    cleanup(&db);
}

// ── board columns ────────────────────────────────────────────────────────────

#[tokio::test]
async fn board_columns_render_source_labels() {
    let mut open = mk_quest("o", "t");
    open.status = QuestStatus::Open;
    let mut done = mk_quest("d", "t");
    done.status = QuestStatus::Done;
    done.completion_source = Some(CompletionSource::Detected);
    let mut dismissed = mk_quest("x", "t");
    dismissed.status = QuestStatus::Dismissed;
    let board = board_columns(&[open, done, dismissed]);
    let cols = board["columns"].as_array().unwrap();
    assert_eq!(cols[1]["quests"][0]["source"], json!("detected"));
    assert_eq!(cols[0]["count"], json!(1));
    assert_eq!(cols[2]["count"], json!(1));
}

// ── api handlers (invoked directly through the extractors) ───────────────────

async fn api_engine() -> (QuestsCtx, PathBuf, Arc<FakeHost>) {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let host = Arc::new(FakeHost::new("http://127.0.0.1:1".into()));
    let engine = engine_with(store, host.clone());
    (QuestsCtx::new(engine), db, host)
}

#[tokio::test]
async fn api_create_validates_and_creates() {
    let (ctx, db, _host) = api_engine().await;
    // Empty title → 400.
    let (code, _) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "   ".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);

    // Valid → 200 with a quest.
    let (code, Json(body)) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "Do it".into(),
            detail: Some("x".into()),
            completion_condition: "done".into(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(body["quest"]["title"], json!("Do it"));

    // List reflects it.
    let Json(list) = api::list_quests(State(ctx)).await;
    assert_eq!(list["quests"].as_array().unwrap().len(), 1);
    cleanup(&db);
}

#[tokio::test]
async fn api_create_maps_backing_job_failure_to_500() {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let engine = engine_with(store, Arc::new(FakeHost::failing()));
    let ctx = QuestsCtx::new(engine);
    let (code, _) = api::create_quest(
        State(ctx),
        Json(QuestBody {
            title: "t".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    cleanup(&db);
}

#[tokio::test]
async fn api_get_update_delete_lifecycle() {
    let (ctx, db, _host) = api_engine().await;
    let (_c, Json(created)) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "t".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    let id = created["quest"]["id"].as_str().unwrap().to_string();

    // GET found / missing.
    let (code, _) = api::get_quest(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let (code, _) = api::get_quest(State(ctx.clone()), Path("ghost".into())).await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);

    // PUT empty title → 400, missing → 404, valid → 200.
    let (code, _) = api::update_quest(
        State(ctx.clone()),
        Path(id.clone()),
        Json(QuestBody {
            title: "  ".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
    let (code, _) = api::update_quest(
        State(ctx.clone()),
        Path("ghost".into()),
        Json(QuestBody {
            title: "x".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);
    let (code, Json(upd)) = api::update_quest(
        State(ctx.clone()),
        Path(id.clone()),
        Json(QuestBody {
            title: "renamed".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(upd["quest"]["title"], json!("renamed"));

    // DELETE found → 200, again → 404.
    let (code, _) = api::delete_quest(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let (code, _) = api::delete_quest(State(ctx), Path(id)).await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);
    cleanup(&db);
}

#[tokio::test]
async fn api_status_change_handlers() {
    let (ctx, db, _host) = api_engine().await;
    let (_c, Json(created)) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "t".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    let id = created["quest"]["id"].as_str().unwrap().to_string();

    // dismiss_suggestion (snooze) → 200; missing → 404.
    let (code, _) = api::dismiss_suggestion(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let (code, _) = api::dismiss_suggestion(State(ctx.clone()), Path("ghost".into())).await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);

    // reopen → 200 (from open, still fine).
    let (code, _) = api::reopen_quest(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);

    // complete → 200; missing → 404.
    let (code, Json(done)) = api::complete_quest(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(done["quest"]["status"], json!("done"));
    let (code, _) = api::complete_quest(State(ctx.clone()), Path("ghost".into())).await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);

    // accept_suggestion path (detected completion) on a reopened quest.
    api::reopen_quest(State(ctx.clone()), Path(id.clone())).await;
    let (code, _) = api::accept_suggestion(State(ctx.clone()), Path(id.clone())).await;
    assert_eq!(code, axum::http::StatusCode::OK);

    // dismiss_quest whole → 200.
    api::reopen_quest(State(ctx.clone()), Path(id.clone())).await;
    let (code, _) = api::dismiss_quest(State(ctx), Path(id)).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    cleanup(&db);
}

#[tokio::test]
async fn api_judge_handler_skip_and_verdict() {
    // No context, no shadow → skipped (200 with skipped=true).
    let (ctx, db, _host) = api_engine().await;
    let (_c, Json(created)) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "t".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    let id = created["quest"]["id"].as_str().unwrap().to_string();
    let (code, Json(body)) = api::judge_quest(State(ctx.clone()), Path(id.clone()), None).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(body["skipped"], json!(true));

    // Unknown id with a body context → BAD_REQUEST (engine errors).
    let (code, _) = api::judge_quest(
        State(ctx),
        Path("ghost".into()),
        Some(Json(JudgeBody {
            context: Some("e".into()),
        })),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
    cleanup(&db);
}

#[tokio::test]
async fn api_judge_handler_returns_verdict_with_gateway() {
    let db = temp_db();
    let store = QuestStore::open(db.clone()).unwrap();
    let base = spawn_gateway("MET: yes\nCONFIDENCE: 92\nREASON: done".into()).await;
    let host = Arc::new(FakeHost::new(base));
    host.set_pref(DETECTION_MODE_PREF, "suggest");
    host.set_pref(JUDGE_MODEL_PREF, "test-judge");
    let engine = engine_with(store, host);
    let ctx = QuestsCtx::new(engine);
    let (_c, Json(created)) = api::create_quest(
        State(ctx.clone()),
        Json(QuestBody {
            title: "t".into(),
            detail: None,
            completion_condition: String::new(),
        }),
    )
    .await;
    let id = created["quest"]["id"].as_str().unwrap().to_string();
    let (code, Json(body)) = api::judge_quest(
        State(ctx),
        Path(id),
        Some(Json(JudgeBody {
            context: Some("the thing is done".into()),
        })),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(body["met"], json!(true));
    assert_eq!(body["confidence"], json!(92));
    cleanup(&db);
}

#[tokio::test]
async fn api_detection_config_get_and_set() {
    let (ctx, db, _host) = api_engine().await;
    // Default GET.
    let Json(cfg) = api::get_detection_config(State(ctx.clone())).await;
    assert_eq!(cfg["mode"], json!("auto_high"));
    assert_eq!(cfg["interval"], json!("2m"));

    // Set mode (normalized), model, effort, valid interval.
    let (code, _) = api::set_detection_config(
        State(ctx.clone()),
        Json(DetectionConfigBody {
            mode: Some("auto-all".into()),
            model: Some(" gpt ".into()),
            effort: Some("low".into()),
            interval: Some("10m".into()),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);

    let Json(cfg) = api::get_detection_config(State(ctx.clone())).await;
    assert_eq!(cfg["mode"], json!("auto_all"));
    assert_eq!(cfg["model"], json!("gpt"));
    assert_eq!(cfg["effort"], json!("low"));
    assert_eq!(cfg["interval"], json!("10m"));

    // Invalid interval → 400.
    let (code, _) = api::set_detection_config(
        State(ctx),
        Json(DetectionConfigBody {
            mode: None,
            model: None,
            effort: None,
            interval: Some("banana".into()),
        }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
    cleanup(&db);
}

#[tokio::test]
async fn api_events_handler_constructs() {
    let (ctx, db, _host) = api_engine().await;
    // Just constructing the SSE response covers the priming-seed setup; dropping it
    // is fine (no subscriber leak beyond the store's channel).
    let _sse = api::quest_events(State(ctx)).await;
    cleanup(&db);
}
