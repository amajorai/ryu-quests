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

/// The document Core imports. `components(schemas(...))` is what turns each
/// `request_body = T` into a resolvable `#/components/schemas/T` entry: without
/// it the operation still carries a `$ref`, but the target is missing and Core's
/// `resolve_ref` yields nothing — a derived write tool with zero visible
/// arguments. utoipa 5 also auto-collects schemas reachable from the annotated
/// paths, so these rows are belt-and-braces; they are listed explicitly anyway so
/// the registration is greppable and cannot be silently lost to an attribute edit.
///
/// `CaptureSource` is here because it is reachable only TRANSITIVELY, through
/// `CaptureBody::source` — the transitive graph is the part that breaks builds.
/// That field carries `#[schema(inline)]`, so nothing currently `$ref`s this entry;
/// it stays registered so dropping the `inline` cannot leave a dangling pointer.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
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
    ),
    components(schemas(
        CaptureBody,
        CaptureSource,
        DetectionConfigBody,
        JudgeBody,
        PinBody,
        QuestBody,
        ScratchpadBody,
        UseBody,
    ))
)]
struct QuestsApiDoc;

/// Request body for creating/updating a quest.
///
/// The field docs below are not decoration: they are lifted verbatim into the
/// OpenAPI schema and become the argument descriptions the model reads when it
/// decides how to call the derived `create`/`update` tool.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct QuestBody {
    /// The one-line name of the quest. Required and must be non-blank.
    pub title: String,
    /// Optional longer description of what the quest involves.
    #[serde(default)]
    pub detail: Option<String>,
    /// Natural-language condition the judge evaluates to decide the quest is
    /// done. Empty means "use the title".
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CaptureBody {
    /// The captured text. The only required field.
    pub body: String,
    /// `task`/`note`/`link`/`prompt`/`snippet`. Inferred from the body when absent.
    #[serde(default)]
    pub kind: Option<String>,
    /// A label. Derived from the body's first line when absent.
    #[serde(default)]
    pub title: Option<String>,
    /// Where the capture came from (app / window title / URL), so a kept snippet
    /// never becomes an orphan quote.
    // `#[schema(inline)]` — NOT a doc comment, because everything above IS lifted
    // into the schema and read by the model, and this rationale is not for it.
    // The field is an `Option<Struct>`, which utoipa renders as
    // `oneOf: [null, <schema>]`; Core follows only a `$ref` sitting at the TOP of a
    // node, so a ref buried in that `oneOf` reaches the model as an opaque pointer
    // it cannot interpret. Inlined, it sees the three real fields instead.
    #[serde(default)]
    #[schema(inline)]
    pub source: Option<CaptureSource>,
}

/// `POST /api/quests/capture` — keep something grabbed while working.
#[utoipa::path(
    post,
    path = "/api/quests/capture",
    tag = "Quests",
    summary = "keep a captured selection, link, prompt, or note.",
    request_body = CaptureBody,
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
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
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
    // NOT `Option<UseBody>`, even though the handler takes `Option<Json<UseBody>>`
    // and an absent body is legal. utoipa 5 renders an optional request body as
    // `{"oneOf":[{"type":"null"},{"$ref":…}]}`, and Core's importer only resolves a
    // TOP-LEVEL `$ref` — a `oneOf` node passes through unresolved, has no
    // `properties`, and the derived tool is back to zero arguments. utoipa derives
    // `required` solely from `is_option()` (there is no `required = false` knob), so
    // the plain type is the only shape that keeps the arguments visible. The cost is
    // a body documented as required that the handler in fact tolerates omitting —
    // a far smaller lie than an uncallable tool.
    request_body = UseBody,
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
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PinBody {
    /// `true` pins the item to the top of the board, `false` unpins it. An
    /// absent body pins (the endpoint's only useful default).
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
    // Plain, not `Option<PinBody>` — see the note on `use_item`.
    request_body = PinBody,
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
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct ScratchpadBody {
    /// The full new contents of the brain-dump buffer. This OVERWRITES, it does
    /// not append — send the whole buffer, not just the addition.
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
    request_body = ScratchpadBody,
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
    request_body = QuestBody,
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
    request_body = QuestBody,
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
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct JudgeBody {
    /// Evidence for the judge to evaluate. Omit it to let the sidecar gather its
    /// own context.
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
    // Plain, not `Option<JudgeBody>` — see the note on `use_item`.
    request_body = JudgeBody,
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
    // No `request_body`: this handler takes only the path id, so declaring one
    // would document a body the endpoint reads and ignores.
    params(("id" = String, Path)),
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
    // No `request_body`: this handler takes only the path id, so declaring one
    // would document a body the endpoint reads and ignores.
    params(("id" = String, Path)),
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
    // No `request_body`: this handler takes only the path id, so declaring one
    // would document a body the endpoint reads and ignores.
    params(("id" = String, Path)),
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
    // No `request_body`: this handler takes only the path id, so declaring one
    // would document a body the endpoint reads and ignores.
    params(("id" = String, Path)),
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
    // No `request_body`: this handler takes only the path id, so declaring one
    // would document a body the endpoint reads and ignores.
    params(("id" = String, Path)),
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

/// Request body for `PUT /api/quests/detection-config`. Every field is a partial
/// update: an omitted field leaves that knob untouched.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct DetectionConfigBody {
    /// How aggressive auto-detection is: `off`, `manual`, `assist`, or `auto`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Model id the completion judge runs on. Empty string clears it back to the
    /// node default.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for the judge (e.g. `low`, `medium`, `high`).
    #[serde(default)]
    pub effort: Option<String>,
    /// How often detection runs, as a humantime duration (e.g. `2m`, `1h`). An
    /// unparseable value is rejected with 400.
    #[serde(default)]
    pub interval: Option<String>,
}

/// `PUT /api/quests/detection-config` — set the detection mode + judge model.
#[utoipa::path(
    put,
    path = "/api/quests/detection-config",
    tag = "Quests",
    summary = "set the detection mode + judge model.",
    request_body = DetectionConfigBody,
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

#[cfg(test)]
mod tests {

    // ── OpenAPI document ───────────────────────────────────────────────────────

    /// This app's own manifest, read at compile time. The route contract lives there,
    /// so the invariants below compare the document against the real declaration
    /// rather than against a second list that could drift from it.
    fn openapi_manifest() -> serde_json::Value {
        serde_json::from_str(include_str!("../../manifest.json")).expect("valid JSON")
    }

    /// The manifest sidecar whose HTTP surface this router serves: the one that
    /// declares an `http.mount`. Selected BY mount rather than by index because an app
    /// may declare a second, mountless sidecar (finetune already does), and
    /// `sidecars[0]` would then quietly start asserting against the wrong process.
    fn mounted_sidecar() -> serde_json::Value {
        openapi_manifest()["sidecars"]
            .as_array()
            .expect("sidecars must be an array")
            .iter()
            .find(|s| s["http"]["mount"].is_string())
            .expect("one sidecar must declare an http.mount")
            .clone()
    }

    /// A manifest route (relative to the mount, in axum's `:param` form) rewritten
    /// into the form the OpenAPI document uses (absolute, in `{param}` form).
    ///
    /// The two forms differ ON PURPOSE — the router registers paths relative to the
    /// mount because Core nests it there, while the `#[utoipa::path]` annotations carry
    /// the absolute EXTERNAL path a caller actually hits. Normalise here; do not
    /// "align" either side.
    fn doc_path_for(mount: &str, route: &str) -> String {
        let joined = if route == "/" {
            mount.to_owned()
        } else {
            format!("{mount}{route}")
        };
        joined
            .split('/')
            .map(|seg| match seg.strip_prefix(':') {
                Some(name) => format!("{{{name}}}"),
                None => seg.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn openapi_doc_is_served_and_non_empty() {
        // The doc is no longer dead code: Core fetches it to derive tools.
        assert!(!super::openapi().paths.paths.is_empty());
    }

    #[test]
    fn the_served_wire_form_is_what_core_parses() {
        // The struct is not what Core sees — `axum::Json` serializes it and Core runs
        // the BYTES through `openapi_import::parse_spec` + `spec_to_api_with_base`,
        // which read `paths` off a plain `serde_json::Value` and, on failure, log once
        // at warn and latch the app at zero tools for the life of the process. So
        // assert the wire form the route actually returns, not just the struct: a
        // utoipa bump that changed the serialized shape would otherwise ship green.
        //
        // Asserted in ONE backend rather than all eleven because the wire shape is
        // decided by the shared utoipa version, not by any app's annotations — every
        // other app's `openapi_doc_is_served_and_non_empty` covers its own content.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        assert!(
            wire["openapi"]
                .as_str()
                .is_some_and(|v| v.starts_with("3.")),
            "expected an OpenAPI 3.x document, got {:?}",
            wire["openapi"]
        );
        let paths = wire["paths"].as_object().expect("paths must be an object");
        assert!(
            !paths.is_empty(),
            "a document with no paths derives no tools"
        );
    }

    /// The one pointer Core reads to give a derived write tool its arguments.
    fn body_schema(wire: &serde_json::Value, path: &str, method: &str) -> serde_json::Value {
        wire.pointer(&format!(
            "/paths/{}/{method}/requestBody/content/application~1json/schema",
            path.replace('/', "~1")
        ))
        .unwrap_or_else(|| panic!("{method} {path} must declare a JSON request body"))
        .clone()
    }

    #[test]
    fn post_routes_document_their_request_body() {
        // The regression this locks down: every annotation here used to say
        // `request_body = serde_json::Value`, which serialises to an untyped schema.
        // Core derives a tool per operation and fills `input_schema` from THIS node,
        // so an untyped body produced a tool the model could discover, could call,
        // and could never pass a single argument to — discoverable and useless, with
        // nothing logged to explain it.
        //
        // A `$ref` is the CORRECT and expected shape, not a near-miss: Core's
        // `openapi_import::resolve_ref` resolves it against `components.schemas`
        // before reading `properties`. So accept either a ref or inlined properties;
        // asserting "inlined" would fail on a healthy document.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        for (path, method) in [
            ("/api/quests", "post"),                 // create_quest -> QuestBody
            ("/api/quests/capture", "post"),         // capture_item -> CaptureBody
            ("/api/quests/{id}", "put"),             // update_quest -> QuestBody
            ("/api/quests/{id}/use", "post"),        // use_item -> UseBody
            ("/api/quests/{id}/pin", "post"),        // pin_item -> PinBody
            ("/api/quests/{id}/judge", "post"),      // judge_quest -> JudgeBody
            ("/api/quests/scratchpad", "put"),       // set_scratchpad -> ScratchpadBody
            ("/api/quests/detection-config", "put"), // set_detection_config
        ] {
            let schema = body_schema(&wire, path, method);
            assert!(
                schema.get("$ref").is_some() || schema.get("properties").is_some(),
                "a derived write tool for {method} {path} would have no arguments: {schema}"
            );
        }
    }

    #[test]
    fn every_request_body_ref_resolves_against_components() {
        // The half of the retrofit that a `$ref`-shaped assertion alone cannot see:
        // a `$ref` pointing at a schema that was never registered in
        // `components(schemas(...))` looks identical in the operation and still
        // yields zero arguments once Core tries to resolve it. Walk every request
        // body in the document and check the target actually exists and carries
        // properties.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        let paths = wire["paths"].as_object().expect("paths must be an object");
        let mut checked = 0usize;
        for (path, item) in paths {
            for (method, op) in item.as_object().expect("a path item is an object") {
                let Some(schema) = op.pointer("/requestBody/content/application~1json/schema")
                else {
                    continue;
                };
                let Some(reference) = schema.get("$ref").and_then(|r| r.as_str()) else {
                    // Inlined schemas are fine as long as they describe something.
                    // The failure this catches in practice is `request_body =
                    // Option<T>`, which utoipa renders as a nullable `oneOf` wrapper:
                    // Core resolves only a TOP-LEVEL `$ref`, so the wrapper reaches the
                    // importer unresolved and contributes no properties at all.
                    assert!(
                        schema.get("properties").is_some(),
                        "{method} {path} has a request-body schema Core cannot read \
                         (a `oneOf` here means `request_body = Option<T>` — use the \
                         plain type): {schema}"
                    );
                    checked += 1;
                    continue;
                };
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or_else(|| {
                        panic!("unexpected ref form '{reference}' at {method} {path}")
                    });
                let target = wire
                    .pointer(&format!("/components/schemas/{name}"))
                    .unwrap_or_else(|| {
                        panic!(
                            "{method} {path} refs '{name}' but it is missing from \
                             components.schemas — add it to components(schemas(..))"
                        )
                    });
                assert!(
                    target.get("properties").is_some(),
                    "{method} {path} refs '{name}', which has no properties: {target}"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 8,
            "expected every write route to carry a body schema, saw {checked}"
        );
    }

    #[test]
    fn body_field_docs_reach_the_schema_as_argument_descriptions() {
        // Doc comments on the body-struct fields are the whole payoff of the
        // retrofit: they are the only prose the model reads when choosing arguments.
        // utoipa lifts them into `description`, so a future edit that drops them
        // silently degrades tool-call quality with no compile error.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        let title = wire
            .pointer("/components/schemas/QuestBody/properties/title/description")
            .and_then(|d| d.as_str())
            .unwrap_or_default();
        assert!(
            title.contains("one-line name"),
            "QuestBody::title lost its doc comment, got {title:?}"
        );
    }

    #[test]
    fn a_nested_struct_argument_is_self_describing() {
        // `CaptureBody::source` is an `Option<CaptureSource>`. utoipa wraps that in
        // `oneOf: [null, …]`, and Core resolves a `$ref` only at the TOP of a node —
        // so a ref nested inside the wrapper would reach the model as an opaque
        // pointer. `#[schema(inline)]` is what makes the three real fields visible;
        // this test fails the moment someone removes it.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        let source = wire
            .pointer("/components/schemas/CaptureBody/properties/source")
            .expect("CaptureBody must document `source`");
        let variants = source["oneOf"]
            .as_array()
            .expect("an optional struct field is a oneOf wrapper");
        let object = variants
            .iter()
            .find(|v| v["type"] == "object")
            .expect("the non-null variant must be an inlined object, not a $ref");
        for field in ["app", "title", "url"] {
            assert!(
                object["properties"].get(field).is_some(),
                "CaptureSource::{field} is invisible to the model: {object}"
            );
        }
    }

    #[test]
    fn body_less_routes_declare_no_request_body() {
        // The other direction of the same bug. `complete`/`dismiss`/`reopen` and the
        // two suggestion routes take ONLY the path id — the handlers have no `Json`
        // extractor at all. Declaring a body for them documented something the
        // endpoint never reads, and (before the retrofit) an untyped one at that.
        let wire = serde_json::to_value(super::openapi()).expect("the doc must serialize");
        for path in [
            "/api/quests/{id}/complete",
            "/api/quests/{id}/dismiss",
            "/api/quests/{id}/reopen",
            "/api/quests/{id}/suggestion/accept",
            "/api/quests/{id}/suggestion/dismiss",
        ] {
            let op = wire
                .pointer(&format!("/paths/{}/post", path.replace('/', "~1")))
                .unwrap_or_else(|| panic!("{path} must have a POST operation"));
            assert!(
                op.get("requestBody").is_none(),
                "{path} takes no body but the document declares one"
            );
            // …and the id it DOES take must still be an argument.
            assert!(
                op.get("parameters").is_some(),
                "{path} must still document its path id"
            );
        }
    }

    #[test]
    fn every_declared_route_appears_in_the_openapi_doc() {
        // The direction that decides tool yield. Core's `ext_api::lower` keeps only the
        // document operations the manifest ALSO declares, so a declared route with no
        // `#[utoipa::path]` annotation is a tool that silently never exists — nothing
        // errors, the agent simply cannot call it. (The other direction is harmless: an
        // annotated path the manifest does not declare is dropped by the same filter.)
        let sidecar = mounted_sidecar();
        let mount = sidecar["http"]["mount"].as_str().expect("an http.mount");
        let doc = super::openapi();
        for route in sidecar["http"]["routes"]
            .as_array()
            .expect("routes must be an array")
        {
            let path = route["path"].as_str().expect("a route path");
            let expected = doc_path_for(mount, path);
            assert!(
                doc.paths.paths.contains_key(&expected),
                "'{path}' is declared in manifest.json but the OpenAPI document has no \
                 '{expected}' operation — Core derives no tool for it"
            );
        }
    }
}
