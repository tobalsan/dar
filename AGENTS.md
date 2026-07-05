# AGENTS.md / CLAUDE.md

This file provides guidance to coding agents when working with code in this repository.

Dar v0: a folder-scoped agent runtime. Cargo workspace; the shipped binary is assembled in `dist/` from an explicit plugin list. Spec lives in `PRD.md` (+ `PRD-EXTENSIONS.md`). README has user-facing usage.

**Three pillars.** (1) a small domain-free **core** (`crates/dar-host` + the contract crates) that knows nothing about trackers, runners, or chat; (2) a stable **SDK** (`crates/extension-sdk`, published as `dar-extension-sdk`) — the one crate a third-party extension author depends on; (3) an **ecosystem of extensions** under `extensions/`, each adding one capability. Everything below is an instance of one of these three.

## Commands

```bash
cargo build --release            # -> ./target/release/dar (and cargo-dar)
cargo test --release             # workspace tests
cargo test --release backoff_grows_then_caps   # a single test by name

./target/release/dar doctor --dir ./example-agent   # config/template/tracker preflight
./target/release/dar run   --dir ./example-agent    # long-running loop + dashboard on :7878
```

The runner backend named by `agent.yaml` `runner.use` must be installed and authenticated on the host (not bundled); `example-agent` ships the `fake` runner so it has no host dependency. `example-agent/` is the test fixture; reset between runs by setting `issues/*.md` `state:` back to `todo`, emptying `workspaces/` (keep `.gitkeep`), and deleting `data/store.db` (persisted run history) plus `logs/agent.log`.

## Maintenance

Update `CHANGELOG.md` for every user-visible behavior change, bug fix, new
extension surface, config option, or CLI/runtime change. Put entries under
`[Unreleased]` and keep wording user-facing, not implementation-only.

## Architecture

### Extension architecture

Domain-free host (`crates/dar-host`) + contract crates (`crates/host-api`, `cap-tracker`, `cap-runner`, `cap-chat`, `cap-dashboard-tab`, `tool-registry`, `orchestrator-api`, `runner-core`); every feature is one crate under `extensions/`. The composition root is `dist/`: the `plugins![]` list in `dist/src/main.rs` is the only place naming the shipped extension mix — adding/removing an extension = one list line + a `dist/Cargo.toml` dependency, then rebuild. List order matters: substrate extensions (e.g. `tool-registry-host`, `frontend-log`) must precede their consumers.

**Authoring surface (the SDK pillar).** New extensions depend on **`dar-extension-sdk`** (`crates/extension-sdk`), the stable re-export of `host-api` + the caps an author needs (`chat`, `orchestrator`, tool registry, a structured `log` hook) — *not* on individual workspace crates. Extensions read zero host internals. `extensions/example` is the living reference; `cargo dar new <name> --kind background|service|foreground` scaffolds a compiling extension.

Integration surfaces (all typed, all opt-in — linked ≠ enabled):

- **Service registry** — named `dyn Trait` services keyed by id + Rust type. Runners register `dyn Runner` under `"pi"`/`"codex"`/`"opencode"`/`"cli"`/`"fake"`; trackers register `dyn TrackerFactory` under `"files"`/`"linear"`; chat backends register `dyn ChatBackend` under `"pi"`/`"codex"`/`"opencode"` (coexists with the same-id runner). `agent.yaml` `tracker.use` / `runner.use` picks which actually runs.
- **Event bus** — broadcast + retained topics. Orchestration payloads in `crates/orchestrator-api`: `RunSnapshot` (retained), `ControlMsg`, `RunRequested`, `DispatchRequested`. The retained `system.context` identity payload lives in `crates/system-files::bus` (not `orchestrator-api`). Semantics in `crates/host-api/src/lib.rs`.
- **Host tool registry** (`crates/tool-registry` + `extensions/tool-registry-host`) — extensions expose runtime tools to agents: register a `ToolSpec` (name + JSON schema) + async executor against the one shared `ToolRegistry` (published as a service by `tool-registry-host`, early in the list). Duplicate names are a hard boot/doctor error. The host **MCP bridge** serves the registry to runners natively; `crates/tool-shim` is the runner-agnostic turn-loop fallback (same registry, same `ToolOutcome` observability — only the transport differs).
- **Dashboard-tab contract** (`crates/cap-dashboard-tab`) — any extension contributes a dashboard tab by adding an `Arc<dyn DashboardTab>` to the shared `DashboardTabs` service; the tab returns an HTML **fragment** and the dashboard splices it into its htmx `#content` shell (no `<body>` swap). Dashboard stays ignorant of the extension.
- **Foreground slot** — at most one extension owns the terminal; top-level `foreground:` in `agent.yaml` (default `"logs"`, `frontend-log`); unknown id → clean boot error, exit 1.
- **Config** — top-level `extensions:` map in `agent.yaml`, keyed by extension id, delivered via `ConfigStore`.

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

Kill `dar` and rerun → it starts cold and trusts whatever the issue files say; only run history is restored from SQLite.

### Single-writer discipline (`extensions/orchestrator/src/state.rs`)

The **orchestrator is the sole mutator** of run state (including the `paused` flag). The dashboard extension only publishes `ControlMsg` (Stop/Pause/Resume) on the bus `CONTROL_TOPIC` (the orchestrator bridges it to its internal `control_tx`) and renders from the retained `RunSnapshot`. Stop/Pause/Resume mutate run state only, never issue state.

### Templating: two engines, both strict

- **Dashboard** (`extensions/dashboard/templates/`, `src/view.rs`) uses **askama** — compiled at build time. A template/field mismatch is a `cargo build` error, not a runtime one.
- **Prompt** (`WORKFLOW.md`, `extensions/orchestrator/src/prompt.rs`) uses **minijinja** in strict mode — an unknown `{{ issue.* }}` variable fails the dispatch attempt (treated as abnormal → backoff retry).

The dashboard self-polls `GET /content` into the `<div id="content">` wrapper (innerHTML swap), NOT a whole-page `<body>` swap — swapping `<body>` outerHTML breaks htmx and blanks the page.

### Path containment

Two layers: `host-api` exposes `HostPaths::assert_contained` for extensions, and `extensions/orchestrator/src/paths.rs` (`issue_workspace` / `assert_contained`) enforces that a child cwd cannot escape `workspace.root` (canonicalized, rejects `..`/symlinks).

### Other ecosystem extensions

- **Scheduler** (`extensions/scheduler`) — loads `cron/jobs.json`, arms a timer per enabled job (cron + IANA tz + optional `startAt`), and at fire time spawns the agent's `runner.use` with the job's `payload.message`, writing the response to `cron/output/<job_id>/<ts>.md`. Hot-reloads the jobs file (per-job `enabled` live; the `extensions.scheduler.enabled` kill switch is read once at boot). Exposes HTTP job CRUD + run-now/tail and a read-only Cron dashboard tab.
- **System context** (`extensions/system-context` + `crates/system-files`) — the `crates/system-files` resolver assembles the agent's identity files into one path-tagged string: `AGENTS.md` first (position 0 if present), then declared `system_files` entries in order (`{ path, required? }`, deduped, root-contained). The `system-context` substrate extension reads `system_files` from `agent.yaml`, resolves them (plus any workspace `skills/`), and publishes the retained `system.context` topic (payload + topic const in `crates/system-files::bus`) once at boot — *independent of whether the orchestration loop is enabled*, so passive agents get the same identity as tracker-driven ones. It must precede the orchestrator and all consumers (TUI chat, issue runner) in `dist/src/main.rs`. The orchestrator and TUI are consumers: they read the retained topic and prepend its text to runner/chat prompts.
- **Fleet / presence** (`crates/dar-presence` + `dar dash`) — each live agent dashboard writes one JSON presence file to a registry dir (`~/.dar/dashboards`); `dar dash` reads the dir, prunes dead pids, and serves a unified host-wide view that iframes each agent's own dashboard.
