# AGENTS.md / CLAUDE.md

This file provides guidance to coding agents when working with code in this repository.

Agentropy v0: a folder-scoped agent runtime. Cargo workspace; the shipped binary is assembled in `dist/` from an explicit plugin list. Spec lives in `PRD.md` (+ `PRD-EXTENSIONS.md`). README has user-facing usage.

## Commands

```bash
cargo build --release            # -> ./target/release/agentropy (and cargo-agentropy)
cargo test --release             # workspace tests
cargo test --release backoff_grows_then_caps   # a single test by name

./target/release/agentropy doctor --dir ./example-agent   # config/template/tracker preflight
./target/release/agentropy run   --dir ./example-agent    # long-running loop + dashboard on :7878
```

`run` needs the `claude` CLI installed and authenticated on the host (not bundled). It dispatches real Claude children — running against `example-agent` spends API. `example-agent/` is the test fixture; reset between runs by setting `issues/*.md` `state:` back to `todo`, emptying `workspaces/` (keep `.gitkeep`), and deleting `data/store.db` (persisted run history) plus `logs/agent.log`.

## Architecture

### Extension architecture

Domain-free host (`crates/agentropy-host`) + contract crates (`crates/host-api`, `cap-tracker`, `cap-runner`, `cap-chat`, `orchestrator-api`, `runner-core`); every feature is one crate under `extensions/`. The composition root is `dist/`: the `plugins![]` list in `dist/src/main.rs` is the only place naming the shipped extension mix — adding/removing an extension = one list line + a `dist/Cargo.toml` dependency, then rebuild. Extensions import `host-api` (plus at most one cap/api crate) and read zero host internals. Integration surfaces:

- **Typed service registry** — named services, e.g. runners register `dyn Runner` under `"pi"`/`"claude"`/`"claude-code"`/`"codex"`/`"cli"`/`"fake"`, trackers register `dyn TrackerFactory` under `"files"`/`"linear"`, chat backends register `dyn ChatBackend` under `"pi"` (id + Rust type form the key, so it coexists with the runner's `"pi"`). Linked ≠ enabled: `agent.yaml` `tracker.use` / `runner.use` picks which registered service actually runs.
- **Typed event bus** — broadcast + retained topics. Orchestration payloads live in `crates/orchestrator-api`: `RunSnapshot` (retained), `ControlMsg`, `RunRequested`, `DispatchRequested`. Semantics documented in `crates/host-api/src/lib.rs`.
- **Foreground slot** — at most one extension owns the terminal; selected per agent via top-level `foreground:` key in `agent.yaml` (default `"logs"`); unknown id → clean boot error, exit 1. Per-extension config: top-level `extensions:` map in `agent.yaml`, keyed by extension id, delivered via `ConfigStore`.

`extensions/example` is the living reference; `cargo agentropy new <name> --kind background|service|foreground` scaffolds a compiling extension.

### TUI foreground (`extensions/tui` + `extensions/chat-pi` + `crates/cap-chat`)

`foreground: tui` renders Chat / Logs / Dash tabs in the terminal. `cap-chat` is the chat contract (`ChatBackend`/`ChatSession`); `chat-pi` registers `dyn ChatBackend @ "pi"` driving one long-lived `pi --mode rpc` child (cwd = agent root, sessions under `data/tui/sessions/`). The chat backend is resolved lazily at first submit: `extensions.tui.chat.backend` config → follow `runner.use` (via the retained snapshot's `agent.runner`) when that id has a registered chat backend → fallback `"pi"` with a transcript notice — never a boot failure (`extensions/tui/src/backend.rs`). `tui` registers NO topics — pure consumer of `frontend-log`'s (`host.log-events`/`host.app-done`/`host.startup-banner`) and the orchestrator's two; the Dash tab is absent entirely when the snapshot topic isn't registered, and its `p`/`r`/`s` keys only publish `ControlMsg` (single-writer preserved). **Quitting quits the whole agent**: `Ctrl-C` anywhere or `q` on Logs/Dash (foreground return = host shutdown, children killed). Non-TTY stdout degrades to the exact `foreground: logs` line stream.

### Two state layers (the core invariant)

- **Issue state** — the `state:` field in each issue (e.g. `issues/*.md` for the files tracker). Owned by the tracker. Changed only by the agent (it edits its own file) or a human. **The orchestrator never writes issue state** — the `Tracker` trait (`crates/cap-tracker`) is read-only by design; the single exception is `park_issue_needs_human` (safety park on stall / retries exhausted), whose default impl errors.
- **Run state** — the orchestrator's in-memory view of one dispatch attempt (`RunStatus` in `extensions/orchestrator/src/state.rs`). Finished runs are persisted to SQLite at `data/store.db` (`extensions/orchestrator/src/store.rs`, WAL mode); on startup the history ring is reseeded and leftover live PIDs are killed and marked `Crashed`.

### The tick loop (`extensions/orchestrator/src/lib.rs`)

Ticks every `poll_interval_ms`. Order inside `tick()` is load-bearing: `reconcile()` → `collect_finished()` → `dispatch()` → `publish_snapshots()`.

- **reconcile** re-reads each *running* slot's issue. It must SKIP slots whose child already finished (`handle.is_finished()`) so `collect_finished` can classify a clean exit — otherwise a just-completed run gets stolen and mis-recorded. Terminal issue → finish (`Terminal`, no retry); `needs_human` state → released without retry; missing / non-active → kill + cancel; stalled (no runner events for `stall_timeout_ms`) → kill + park to `needs_human`.
- **collect_finished** classifies exited children: normal exit + terminal issue → Succeeded; normal exit + still active → 1s **continuation** retry (does NOT count against `max_retries`); abnormal exit → **backoff** retry `min(retry_backoff_ms·2^attempt, 30min)` up to `max_retries`, then Failed + park to `needs_human`. `attempt` is 1-based (first retry = attempt 1).
- **dispatch** sorts candidates by priority asc (null last) → `created_at` asc → identifier, dispatches up to `max_concurrent`. Due retries dispatch before fresh candidates.

Kill `agentropy` and rerun → it starts cold and trusts whatever the issue files say; only run history is restored from SQLite.

### Single-writer discipline (`extensions/orchestrator/src/state.rs`)

The **orchestrator is the sole mutator** of run state (including the `paused` flag). The dashboard extension only publishes `ControlMsg` (Stop/Pause/Resume) on the bus `CONTROL_TOPIC` (the orchestrator bridges it to its internal `control_tx`) and renders from the retained `RunSnapshot`. Stop/Pause/Resume mutate run state only, never issue state.

### Runner permission flags (`extensions/runner-claude/src/lib.rs`)

Spawns `claude -p --permission-mode bypassPermissions --add-dir <agent-folder>` with the per-issue workspace as cwd, prompt piped to stdin. These flags are deliberate (resolves a PRD open question): headless runs have no human to answer permission prompts, and `WORKFLOW.md` requires the child to edit its issue file at `../../issues/ISSUE-N.md` — outside the workspace cwd, blocked by Claude's default OS sandbox. **Do NOT use `--dangerously-skip-permissions`** — it hangs headless on a first-use acceptance gate (no TTY).

### Templating: two engines, both strict

- **Dashboard** (`extensions/dashboard/templates/`, `src/view.rs`) uses **askama** — compiled at build time. A template/field mismatch is a `cargo build` error, not a runtime one.
- **Prompt** (`WORKFLOW.md`, `extensions/orchestrator/src/prompt.rs`) uses **minijinja** in strict mode — an unknown `{{ issue.* }}` variable fails the dispatch attempt (treated as abnormal → backoff retry).

The dashboard self-polls `GET /content` into the `<div id="content">` wrapper (innerHTML swap), NOT a whole-page `<body>` swap — swapping `<body>` outerHTML breaks htmx and blanks the page.

### Path containment

Two layers: `host-api` exposes `HostPaths::assert_contained` for extensions, and `extensions/orchestrator/src/paths.rs` (`issue_workspace` / `assert_contained`) enforces that a child cwd cannot escape `workspace.root` (canonicalized, rejects `..`/symlinks).
