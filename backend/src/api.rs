//! HTTP API for quests (`/api/quests/*`): the auto-detecting todo list.
//!
//! CRUD over quest definitions, manual complete/dismiss, accept/dismiss of a
//! pending detection suggestion, an immediate "run detection now" pass, an SSE
//! event stream (suggested / completed), and the detection-config knobs (how
//! aggressive auto-detection is + the judge model).
//!
//! Each *open* quest is mirrored by a scheduled job (created via the host's
//! `sync_backing_job` — the inverted scheduler coupling) so it rides the same
//! tick loop as monitors and workflows. Creating/updating a quest (re)writes that
//! job (enabled only while the quest is open); deleting or completing one removes
//! or disables it.
//!
//! The router is built with its own state ([`QuestsCtx`]) inside this crate so it
//! returns a state-less, mergeable `Router<()>`. The routes are declared relative
//! to `/api/quests` (Core nests this service at that prefix behind the Quests-App
//! gate), while the OpenAPI annotations keep the full external paths.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    CaptureSource, DetectionMode, Quest, QuestEngine, QuestKind, JUDGE_EFFORT_PREF,
    JUDGE_MODEL_PREF,
};

/// Router state for the quests HTTP surface: the [`QuestEngine`] (which owns the
/// store and the inverted host).
#[derive(Clone)]
pub struct QuestsCtx {
    pub engine: QuestEngine,
}

impl QuestsCtx {
    pub fn new(engine: QuestEngine) -> Self {
        Self { engine }
    }
}

/// Build the `/api/quests/*` router with its own state baked in, returning a
/// state-less `Router<()>` the host nests at `/api/quests` behind the App gate.
/// Static segments (`events`, `detection-config`) are registered before `:id` so
/// they match first.
pub fn routes(ctx: QuestsCtx) -> Router<()> {
    Router::new()
        .route("/events", get(quest_events))
        .route(
            "/detection-config",
            get(get_detection_config).put(set_detection_config),
        )
        .route("/capture", post(capture_item))
        .route("/scratchpad", get(get_scratchpad).put(set_scratchpad))
        .route("/", get(list_quests).post(create_quest))
        .route(
            "/:id",
            get(get_quest).put(update_quest).delete(delete_quest),
        )
        .route("/:id/use", post(use_item))
        .route("/:id/pin", post(pin_item))
        .route("/:id/judge", post(judge_quest))
        .route("/:id/complete", post(complete_quest))
        .route("/:id/dismiss", post(dismiss_quest))
        .route("/:id/reopen", post(reopen_quest))
        .route("/:id/suggestion/accept", post(accept_suggestion))
        .route("/:id/suggestion/dismiss", post(dismiss_suggestion))
        .with_state(ctx)
}

/// The OpenAPI sub-document for the quests surface, merged into Core's spec when
/// the `quests` feature is enabled.
pub fn openapi() -> utoipa::openapi::OpenApi {
    <QuestsApiDoc as utoipa::OpenApi>::openapi()
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    accept_suggestion,
    capture_item,
    complete_quest,
    create_quest,
    delete_quest,
    dismiss_quest,
    dismiss_suggestion,
    get_detection_config,
    get_quest,
    get_scratchpad,
    judge_quest,
    list_quests,
    pin_item,
    quest_events,
    reopen_quest,
    set_detection_config,
    set_scratchpad,
    update_quest,
    use_item,
))]
struct QuestsApiDoc;

/// Request body for creating/updating a quest.
#[derive(Debug, Deserialize)]
pub struct QuestBody {
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub completion_condition: String,
}

/// Query params for the list endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Restrict to one kind (`task`/`note`/`link`/`prompt`/`snippet`). Absent =
    /// every kind, which is what the board asks for.
    #[serde(default)]
    pub kind: Option<String>,
}

/// `GET /api/quests` — list all quests (optionally one kind).
#[utoipa::path(
    get,
    path = "/api/quests",
    tag = "Quests",
    summary = "list all quests (optionally filtered to one kind).",
    params(("kind" = Option<String>, Query, description = "task|note|link|prompt|snippet")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn list_quests(
    State(ctx): State<QuestsCtx>,
    Query(q): Query<ListQuery>,
) -> Json<serde_json::Value> {
    // Normalise through `QuestKind` so an unknown `?kind=` can't produce an empty
    // list that reads like "you have nothing" — it falls back to `task`.
    let result = match q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(kind) => {
            ctx.engine
                .store
                .list_quests_of_kind(QuestKind::from_wire(kind).as_str())
                .await
        }
        None => ctx.engine.store.list_quests().await,
    };
    match result {
        Ok(quests) => Json(json!({ "quests": quests })),
        Err(e) => Json(json!({ "quests": [], "error": e.to_string() })),
    }
}

/// Request body for a capture.
#[derive(Debug, Deserialize)]
pub struct CaptureBody {
    /// The captured text. The only required field.
    pub body: String,
    /// `task`/`note`/`link`/`prompt`/`snippet`. Inferred from the body when absent.
    #[serde(default)]
    pub kind: Option<String>,
    /// A label. Derived from the body's first line when absent.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source: Option<CaptureSource>,
}

/// `POST /api/quests/capture` — keep something grabbed while working.
#[utoipa::path(
    post,
    path = "/api/quests/capture",
    tag = "Quests",
    summary = "keep a captured selection, link, prompt, or note.",
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn capture_item(
    State(ctx): State<QuestsCtx>,
    Json(body): Json<CaptureBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "body is required" })),
        );
    }
    let kind = body.kind.as_deref().map(QuestKind::from_wire);
    match ctx
        .engine
        .capture(kind, body.title, body.body, body.source)
        .await
    {
        Ok(quest) => (StatusCode::OK, Json(json!({ "quest": quest }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// Request body for marking an item used.
#[derive(Debug, Default, Deserialize)]
pub struct UseBody {
    /// Also check the item off. A prompt list is worked through this way.
    #[serde(default)]
    pub complete: bool,
}

/// `POST /api/quests/:id/use` — record that the item was copied back out.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/use",
    tag = "Quests",
    summary = "record that an item was copied back out (optionally completing it).",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn use_item(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
    body: Option<Json<UseBody>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let complete = body.map(|Json(b)| b.complete).unwrap_or(false);
    match ctx.engine.mark_used(&id, complete).await {
        Ok(Some(quest)) => (StatusCode::OK, Json(json!({ "quest": quest }))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "quest not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// Request body for pinning.
#[derive(Debug, Default, Deserialize)]
pub struct PinBody {
    #[serde(default)]
    pub pinned: bool,
}

/// `POST /api/quests/:id/pin` — pin or unpin an item.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/pin",
    tag = "Quests",
    summary = "pin or unpin an item to the top of the board.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn pin_item(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
    body: Option<Json<PinBody>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pinned = body.map(|Json(b)| b.pinned).unwrap_or(true);
    match ctx.engine.set_pinned(&id, pinned).await {
        Ok(Some(quest)) => (StatusCode::OK, Json(json!({ "quest": quest }))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "quest not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// Request body for the scratchpad write.
#[derive(Debug, Default, Deserialize)]
pub struct ScratchpadBody {
    #[serde(default)]
    pub text: String,
}

/// `GET /api/quests/scratchpad` — the freeform brain-dump buffer.
#[utoipa::path(
    get,
    path = "/api/quests/scratchpad",
    tag = "Quests",
    summary = "read the freeform brain-dump buffer.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn get_scratchpad(State(ctx): State<QuestsCtx>) -> Json<serde_json::Value> {
    match ctx.engine.scratchpad().await {
        Ok(text) => Json(json!({ "text": text })),
        Err(e) => Json(json!({ "text": "", "error": e })),
    }
}

/// `PUT /api/quests/scratchpad` — overwrite the brain-dump buffer.
#[utoipa::path(
    put,
    path = "/api/quests/scratchpad",
    tag = "Quests",
    summary = "overwrite the freeform brain-dump buffer.",
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn set_scratchpad(
    State(ctx): State<QuestsCtx>,
    Json(body): Json<ScratchpadBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.engine.set_scratchpad(&body.text).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// `POST /api/quests` — create a quest (and its backing detection job).
#[utoipa::path(
    post,
    path = "/api/quests",
    tag = "Quests",
    summary = "create a quest (and its backing detection job).",
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn create_quest(
    State(ctx): State<QuestsCtx>,
    Json(body): Json<QuestBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.title.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "title is required" })),
        );
    }
    match ctx
        .engine
        .create_quest(body.title, body.detail, body.completion_condition)
        .await
    {
        Ok(quest) => (StatusCode::OK, Json(json!({ "quest": quest }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// `GET /api/quests/:id` — one quest.
#[utoipa::path(
    get,
    path = "/api/quests/{id}",
    tag = "Quests",
    summary = "one quest.",
    params(("id" = String, Path)),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn get_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.engine.store.get_quest(&id).await {
        Ok(Some(q)) => (StatusCode::OK, Json(json!({ "quest": q }))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// `PUT /api/quests/:id` — edit a quest's title / detail / completion condition.
#[utoipa::path(
    put,
    path = "/api/quests/{id}",
    tag = "Quests",
    summary = "edit a quest's title / detail / completion condition.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn update_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
    Json(body): Json<QuestBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.title.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "title is required" })),
        );
    }
    match ctx
        .engine
        .update_quest(&id, body.title, body.detail, body.completion_condition)
        .await
    {
        Ok(Some(q)) => (StatusCode::OK, Json(json!({ "quest": q }))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// `DELETE /api/quests/:id` — remove a quest, its history, and its job.
#[utoipa::path(
    delete,
    path = "/api/quests/{id}",
    tag = "Quests",
    summary = "remove a quest, its history, and its job.",
    params(("id" = String, Path)),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn delete_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.engine.delete_quest(&id).await {
        Ok(true) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// Optional request body for `POST /api/quests/:id/judge`: caller-supplied
/// detection context. Core gathers Shadow evidence its own side (the sidecar
/// cannot reach Core's `McpRegistry`) and posts it here; an absent/blank body
/// falls back to the sidecar's own `gather_context`.
#[derive(Debug, Default, Deserialize)]
pub struct JudgeBody {
    #[serde(default)]
    pub context: Option<String>,
}

/// `POST /api/quests/:id/judge` — run one detection pass immediately.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/judge",
    tag = "Quests",
    summary = "run one detection pass immediately.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn judge_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
    body: Option<Json<JudgeBody>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let external_context = body.and_then(|Json(b)| b.context);
    match ctx
        .engine
        .judge_quest_with_context(&id, external_context)
        .await
    {
        Ok(Some(v)) => (
            StatusCode::OK,
            Json(json!({ "met": v.met, "confidence": v.confidence, "reason": v.reason })),
        ),
        Ok(None) => (
            StatusCode::OK,
            Json(
                json!({ "skipped": true, "reason": "not open, snoozed, detection off, or no context available" }),
            ),
        ),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// `POST /api/quests/:id/complete` — mark a quest done (manual check-off). The
/// backing job is disabled by the engine's completion.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/complete",
    tag = "Quests",
    summary = "mark a quest done (manual check-off). The",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn complete_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    finish(ctx.engine.complete_quest(&id, false).await)
}

/// `POST /api/quests/:id/suggestion/accept` — confirm a pending detection.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/suggestion/accept",
    tag = "Quests",
    summary = "confirm a pending detection.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn accept_suggestion(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    finish(ctx.engine.complete_quest(&id, true).await)
}

/// `POST /api/quests/:id/dismiss` — abandon a quest entirely.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/dismiss",
    tag = "Quests",
    summary = "abandon a quest entirely.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn dismiss_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    finish(ctx.engine.dismiss_quest(&id).await)
}

/// `POST /api/quests/:id/reopen` — move a done/dismissed quest back to open.
#[utoipa::path(
    post,
    path = "/api/quests/{id}/reopen",
    tag = "Quests",
    summary = "reopen a done/dismissed quest.",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn reopen_quest(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    finish(ctx.engine.reopen_quest(&id).await)
}

/// `POST /api/quests/:id/suggestion/dismiss` — reject the pending suggestion but
/// keep the quest open (snoozes further detection for a while).
#[utoipa::path(
    post,
    path = "/api/quests/{id}/suggestion/dismiss",
    tag = "Quests",
    summary = "reject the pending suggestion but",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn dismiss_suggestion(
    State(ctx): State<QuestsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.engine.dismiss_suggestion(&id).await {
        Ok(Some(q)) => (StatusCode::OK, Json(json!({ "quest": q }))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// Shared tail for the status-changing ops (the engine has already re-synced the
/// backing job during the transition).
fn finish(result: Result<Option<Quest>, String>) -> (StatusCode, Json<serde_json::Value>) {
    match result {
        Ok(Some(q)) => (StatusCode::OK, Json(json!({ "quest": q }))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// `GET /api/quests/events` — SSE feed of quest events (suggested / completed).
#[utoipa::path(
    get,
    path = "/api/quests/events",
    tag = "Quests",
    summary = "SSE feed of quest events (suggested / completed).",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn quest_events(
    State(ctx): State<QuestsCtx>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;

    let rx = ctx.engine.store.subscribe();
    // Seed the stream with an immediate SSE comment so the FIRST body byte lands at
    // connect, not only when the first quest event (or the 15s keep-alive) arrives.
    // Quests is frequently idle for long stretches (no todo detected), so without this
    // seed the stream stays byte-silent until the keep-alive — and any intermediary that
    // withholds the response head behind the first upstream body byte (the ext-proxy's
    // pre-streaming failure mode) reads that as a "no headers for ~15s" hang. A comment
    // line is ignored by `EventSource`, so this is invisible to real consumers. The `true`
    // in the unfold seed is the "emit the priming comment on first poll" flag.
    let stream = futures_util::stream::unfold((rx, true), |(mut rx, first)| async move {
        if first {
            return Some((Ok(Event::default().comment("ready")), (rx, false)));
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    return Some((Ok(Event::default().data(data)), (rx, false)));
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `GET /api/quests/detection-config` — the current detection knobs.
#[utoipa::path(
    get,
    path = "/api/quests/detection-config",
    tag = "Quests",
    summary = "the current detection knobs.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn get_detection_config(State(ctx): State<QuestsCtx>) -> Json<serde_json::Value> {
    let mode = ctx.engine.detection_mode().await;
    let model = ctx
        .engine
        .pref_get(JUDGE_MODEL_PREF)
        .await
        .unwrap_or_default();
    let effort = ctx
        .engine
        .pref_get(JUDGE_EFFORT_PREF)
        .await
        .unwrap_or_default();
    let interval = ctx.engine.resolve_interval().await;
    Json(json!({
        "mode": mode.as_str(),
        "model": model,
        "effort": effort,
        "interval": interval,
    }))
}

/// Request body for `PUT /api/quests/detection-config`.
#[derive(Debug, Deserialize)]
pub struct DetectionConfigBody {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
}

/// `PUT /api/quests/detection-config` — set the detection mode + judge model.
#[utoipa::path(
    put,
    path = "/api/quests/detection-config",
    tag = "Quests",
    summary = "set the detection mode + judge model.",
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
pub async fn set_detection_config(
    State(ctx): State<QuestsCtx>,
    Json(body): Json<DetectionConfigBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(mode) = body.mode.as_ref() {
        // Normalize through the enum so only valid modes persist.
        let normalized = DetectionMode::from_pref(mode).as_str();
        if let Err(e) = ctx
            .engine
            .pref_set(crate::DETECTION_MODE_PREF, normalized)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            );
        }
    }
    if let Some(model) = body.model.as_ref() {
        let _ = ctx.engine.pref_set(JUDGE_MODEL_PREF, model.trim()).await;
    }
    if let Some(effort) = body.effort.as_ref() {
        let _ = ctx.engine.pref_set(JUDGE_EFFORT_PREF, effort.trim()).await;
    }
    if let Some(interval) = body.interval.as_ref() {
        let t = interval.trim();
        if !t.is_empty() && humantime::parse_duration(t).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": format!("interval '{t}' is not a valid duration (e.g. 2m)") }),
                ),
            );
        }
        let _ = ctx.engine.pref_set(crate::DETECTION_INTERVAL_PREF, t).await;
    }
    (StatusCode::OK, Json(json!({ "ok": true })))
}
