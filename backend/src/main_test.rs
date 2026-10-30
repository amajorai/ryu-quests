use super::{load_prefs, save_prefs, SidecarQuestsHost, DEFAULT_JUDGE_MODEL};
use ryu_quests::QuestsHost;
use std::collections::HashMap;

fn tmp_prefs_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!(
            "ryu-quests-host-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ))
        .join("prefs.json")
}

#[test]
fn load_prefs_on_corrupt_file_is_empty() {
    let path = tmp_prefs_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();
    // Corrupt file must degrade to an empty map (fresh-install defaults), not panic.
    assert!(load_prefs(&path).is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_prefs_is_atomic_and_leaves_no_tmp() {
    let path = tmp_prefs_path();
    let mut map = HashMap::new();
    map.insert("k".to_string(), "v".to_string());
    save_prefs(&path, &map).expect("save");
    // The atomic rename must leave only the final file, not the `.tmp` sibling.
    assert!(path.exists());
    assert!(!path.with_extension("json.tmp").exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn host_pref_roundtrip_persists_and_reads_back() {
    let path = tmp_prefs_path();
    let host = SidecarQuestsHost::new(path.clone());
    assert_eq!(host.pref_get("quest-detection-mode").await, None);
    host.pref_set("quest-detection-mode", "off").await.unwrap();
    assert_eq!(
        host.pref_get("quest-detection-mode").await.as_deref(),
        Some("off")
    );
    // A fresh host over the same path sees the persisted value (restart durability).
    let host2 = SidecarQuestsHost::new(path.clone());
    assert_eq!(
        host2.pref_get("quest-detection-mode").await.as_deref(),
        Some("off")
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn host_stubs_degrade_gracefully() {
    let path = tmp_prefs_path();
    let host = SidecarQuestsHost::new(path);
    // Shadow context is stubbed to None (the sidecar cannot reach Core's MCP).
    assert!(host
        .shadow_call("shadow.recent_context", serde_json::json!({}))
        .await
        .is_none());
    // The scheduler backing job stub MUST return Ok — an Err would 500 every
    // quest creation (create_quest propagates it).
    assert!(host.sync_backing_job("q1", "t", "2m", true).is_ok());
    // delete_backing_job is a no-op that must not panic.
    host.delete_backing_job("q1");
}

#[test]
fn host_gateway_and_model_defaults() {
    let host = SidecarQuestsHost::new(tmp_prefs_path());
    // gateway_url always yields a usable loopback base.
    assert!(host.gateway_url().starts_with("http"));
    // Env-gated defaults: only assert the default when the env var is unset, so
    // the test stays deterministic without mutating process-global env.
    if std::env::var("RYU_GATEWAY_TOKEN").is_err() {
        assert!(host.gateway_token().is_none());
    }
    if std::env::var("RYU_DEFAULT_LLM_MODEL").is_err() {
        assert_eq!(host.default_judge_model(), DEFAULT_JUDGE_MODEL);
    }
    // Regardless, it never returns an empty model.
    assert!(!host.default_judge_model().is_empty());
}

#[test]
fn prefs_roundtrip_through_file() {
    let dir = std::env::temp_dir().join(format!("ryu-quests-test-{}", std::process::id()));
    let path = dir.join("prefs.json");
    let _ = std::fs::remove_file(&path);

    // Missing file → empty map (fresh install falls back to defaults).
    assert!(load_prefs(&path).is_empty());

    let mut map = HashMap::new();
    map.insert("quest-detection-mode".to_string(), "off".to_string());
    save_prefs(&path, &map).expect("save prefs");

    // Reloaded map survives (the restart-durability property).
    let reloaded = load_prefs(&path);
    assert_eq!(
        reloaded.get("quest-detection-mode").map(String::as_str),
        Some("off")
    );

    let _ = std::fs::remove_dir_all(&dir);
}
