# ryu-quests

Quests for Ryu — an auto-detecting todo list surfaced from your chats and activity, tracked as a lightweight quest board.

> **The public home of `ryu-quests`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

- Binary: `ryu-quests` from the [Ryu releases](https://github.com/amajorai/ryu/releases).
- Crate: `cargo install ryu-quests`.

## License

Apache-2.0 — see [LICENSE](./LICENSE).

---

# Quests

Auto-detecting todo list. A **quest** is a task with a natural-language *completion
condition*. On a schedule the engine gathers what the user has recently been doing from
Shadow's always-on context (screen text / activity / semantic history) and asks a judge
model whether the task looks done, then either *suggests* completion (a chip the user
confirms) or *auto-completes* it, per the configured detection mode.

## Parts

- **`backend/` (`ryu-quests`)** — an extracted Core capability crate: `QuestEngine`, the
  SQLite `QuestStore`, event types, and the `/api/quests/*` HTTP surface. **Now served
  OUT-OF-PROCESS** by the `ryu-quests` bin (`[[bin]]`, `kind:local`, `public_mount`,
  `RYU_QUESTS_BIN`/`RYU_QUESTS_PORT`, default `:7991`); Core links **zero quest code** (no
  path-dep, no `quests` cargo feature). Its three reverse-couplings — the scheduler judge
  run, the `JobTarget::Quest` job lifecycle, and the activity feed — reach the sidecar over
  loopback via `apps/core/src/quests_client.rs`. Everything the engine needs *from* the host
  is inverted through the `QuestsHost` trait, so the crate has **zero dependency on
  `apps/core`**.
- **`ui/` (`@ryu/quests-app`)** — the companion surface: a React app built to one
  self-contained HTML via `vite-plugin-singlefile`, consuming `@ryu/ui`. Shipped as a
  full-page Companion (Path B, `ui_format: "html"`).

## Manifest

- **id** `com.ryu.quests` · companion `Quests` (icon `target`).
- **grant** `quests:crud` — the bridge capability the UI drives Core's `/api/quests/*`
  through.
- **contributes** a declarative `quest-board` `list-detail` view (GET `/api/quests`, a
  `Complete` item action) for hosts that render manifest views directly.

## Surface

`/api/quests` (list/create) · `/api/quests/events` (SSE) · per-quest `judge`, `complete`,
`dismiss`, and `suggestion/{accept,dismiss}`.

## Swap seam

The judge model is never hardcoded: pref `quest-judge-model` → env → the host's bundled
local default. Detection mode / interval / effort are prefs (`quest-detection-*`). Routing
the judge call through the Gateway keeps it governed like every other model call.
