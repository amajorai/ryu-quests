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

An auto-detecting todo list **and** a keep pile for AI-assisted work. Two item families
share one board, and the split between them is load-bearing:

- **Tasks** (`kind: "task"`) — a task with a natural-language *completion condition*. On a
  schedule the engine gathers what the user has recently been doing from Shadow's
  always-on context (screen text / activity / semantic history) and asks a judge model
  whether the task looks done, then either *suggests* completion (a chip the user
  confirms) or *auto-completes* it, per the configured detection mode.
- **Captures** (`kind: "note" | "link" | "prompt" | "snippet"`) — the answers, links,
  code fragments, and follow-up prompts collected while moving between chats and editors.
  A capture keeps its `body` verbatim plus the `source` it came from (app / window title /
  URL), so a kept quote never becomes an orphan.

## Captures are NEVER judged

A capture has no completion condition and stays `status: "open"` forever. If the detection
sweep did not gate on the kind it would judge every kept note **on every tick, forever** —
a model call per item per interval, producing a meaningless verdict each time. Three places
enforce this and must move together:

1. `QuestKind::is_judged` — the single predicate.
2. `judge_quest_with_context` returns `Ok(None)` for a non-task before it reaches a model.
3. `sync_job` / `capture` / `update_quest` skip `sync_backing_job` entirely for a capture —
   not "call it with `open: false`", which would still write a permanently-disabled job row
   per kept item.

`captures_are_never_judged_and_carry_no_backing_job` covers all three.

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

- **id** `@ryu/quests` · companion `Quests` (icon `target`).
- **grant** `quests:crud` — the bridge capability the UI drives Core's `/api/quests/*`
  through.
- **permission levels** `quests.view` ⊂ `quests.edit` ⊂ `quests.capture`. Capture is its
  own level because keeping text selected in *another* app is a wider reach than editing
  the board.
- **contributes** a declarative `quest-board` `list-detail` view (GET `/api/quests`, a
  `Complete` item action) for hosts that render manifest views directly, plus the
  `quest.created` / `quest.captured` / `quest.suggested` / `quest.completed` hook events.

## Surface

`/api/quests` (list/create, `?kind=` filter) · `/api/quests/capture` · `/api/quests/events`
(SSE) · `/api/quests/scratchpad` (GET/PUT) · per-quest `judge`, `complete`, `dismiss`,
`reopen`, `use`, `pin`, and `suggestion/{accept,dismiss}`.

Every one of those paths is also listed in `sidecars[0].http.routes` — that array is the
ext-proxy allowlist, and a route missing from it is unreachable from outside the process no
matter what the router says.

## Bridge vocabulary

The companion frame has `connect-src 'none'`, so it reaches Core only through
`window.ryu.quests.*`. Adding an endpoint therefore means adding a *verb*, and the verb
table has one source of truth — `crates/core/kernel-contracts/src/host_api.rs`
(`HOST_API_METHODS`), from which `schemas/host-api.json` is regenerated with
`RYU_REGEN_SCHEMAS=1` and both the TS host and Core's Rust bridge derive their maps. The
capture work added `quests.capture`, `quests.use`, `quests.pin`, `quests.scratchpad`, and
`quests.setScratchpad`, which meant touching, in order:

1. `host_api.rs` (+ regen the JSON),
2. `packages/app-host/src/rpc-tables.test.ts` — the frozen lockstep fixture,
3. `packages/app-host/src/rpc.ts` — `HostServices` + dispatch + arg narrowers,
4. `packages/app-host/src/third-party-plugin.ts` — the injected `window.ryu` surface,
5. `packages/core-client/src/quests.ts` and `apps/desktop/src/lib/api/quests.ts` — clients,
6. the two host closure sites (`PluginHostPanel.tsx`, `plugin-app-mount.tsx`).

## Capture gesture (NOT yet wired)

`POST /api/quests/capture` is the write path a global "keep this" gesture would call, and
it is complete and tested. What is **not** built is the OS-level trigger, because it needs
two host capabilities Ryu does not have yet:

- **A bare-modifier gesture** (Copper uses a double-Shift tap). This can NOT ride
  `tauri-plugin-global-shortcut`: a registered accelerator needs a non-modifier key code,
  and a double-tap needs keydown/keyup *timing* a hotkey callback never sees. The only
  proven mechanism in this repo is `uiohook-napi`, already shipping hold-to-talk in
  `apps/island/src/main/voice-control.ts` (and already requiring macOS Input Monitoring).
  Island is default-OFF for v1, so riding it makes the gesture default-off too.
- **Reading the focused app's text selection.** There is no such API here today.
  `AXSelectedText` is the clean route but is flaky-to-empty in exactly the targets that
  matter (browser and Electron apps — ChatGPT, Claude, Cursor); the portable fallback is
  synthesize ⌘C → watch `NSPasteboard.changeCount` → read → restore the prior clipboard.
  Pick between them from a measurement against those apps, not from principle.

Whichever surface owns the trigger, it should reach the sidecar through the **generic
ext-proxy** (`/api/ext/@ryu/quests/capture`) rather than growing a `com.ryu.quests` branch
in Core or the shell.

## Swap seam

The judge model is never hardcoded: pref `quest-judge-model` → env → the host's bundled
local default. Detection mode / interval / effort are prefs (`quest-detection-*`). Routing
the judge call through the Gateway keeps it governed like every other model call.
