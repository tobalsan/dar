# AGENTS.md / CLAUDE.md

This file provides guidance to coding agents when working with code in this repository.

Agentropy v0: a single Rust binary, folder-scoped agent runtime. Spec lives in `PRD.md`. README has user-facing usage.

## Commands

```bash
cargo build --release            # -> ./target/release/agentropy
cargo test --release             # unit tests (orchestrator sort + backoff)
cargo test --release backoff_grows_then_caps   # a single test by name

./target/release/agentropy doctor --dir ./example-agent   # config/template/tracker preflight
./target/release/agentropy run   --dir ./example-agent    # long-running loop + dashboard on :7878
```

`run` needs the `claude` CLI installed and authenticated on the host (not bundled). It dispatches real Claude children — running against `example-agent` spends API. `example-agent/` is the test fixture; reset between runs by setting `issues/*.md` `state:` back to `todo`, emptying `workspaces/` (keep `.gitkeep`), and deleting `logs/history.jsonl`.

## Architecture

### Two state layers (the core invariant)

- **Issue state** — the `state:` field in each `issues/*.md` file. Owned by the tracker. Changed only by the agent (it edits its own file) or a human. **The orchestrator NEVER writes issue state** — the `Tracker` trait is read-only by design (`src/tracker/`).
- **Run state** — the orchestrator's in-memory view of one dispatch attempt (`RunStatus` in `src/state.rs`: Running/RetryQueued/Cancelled/Failed/Succeeded). Lives in process memory + the log only. The one exception: finished runs are persisted to `logs/history.jsonl` and reloaded on startup.

### The tick loop (`src/orchestrator.rs`)

`Orchestrator::run` ticks every `poll_interval_ms`. Order inside `tick()` is load-bearing: `reconcile()` → `collect_finished()` → `dispatch()` → `publish_snapshots()`.

- **reconcile** re-reads each *running* slot's issue file. It must SKIP slots whose child already finished (`handle.is_finished()`) so `collect_finished` can classify a clean exit — otherwise a just-completed run gets stolen and mis-recorded. A terminal issue at reconcile = **Succeeded** (agent did its job); missing / non-active = **Cancelled**.
- **collect_finished** classifies exited children: normal exit + terminal issue → Succeeded; normal exit + still active → 1s **continuation** retry (does NOT count against `max_retries`); abnormal exit → **backoff** retry `min(retry_backoff_ms·2^(n-1), 5min)` up to `max_retries`, then Failed (log-only).
- **dispatch** sorts candidates by priority asc (null last) → `created_at` asc → identifier, dispatches up to `max_concurrent`. Due retries dispatch before fresh candidates.

State is in-memory: kill `agentropy` and rerun → it starts cold and trusts whatever the issue files say.

### Single-writer discipline (`src/state.rs`)

`AppState` is the shared, `Arc`-cloned bridge. The **orchestrator is the sole mutator** of run state (including the `paused` flag). The dashboard only SENDS `ControlMsg` (Stop/Pause/Resume) over `control_tx` and takes read locks to render. Stop/Pause/Resume mutate run state only, never issue state.

### Runner permission flags (`src/runner.rs`)

Spawns `claude -p --permission-mode bypassPermissions --add-dir <agent-folder>` with the per-issue workspace as cwd, prompt piped to stdin. These flags are deliberate (resolves a PRD open question): headless runs have no human to answer permission prompts, and `WORKFLOW.md` requires the child to edit its issue file at `../../issues/ISSUE-N.md` — outside the workspace cwd, blocked by Claude's default OS sandbox. **Do NOT use `--dangerously-skip-permissions`** — it hangs headless on a first-use acceptance gate (no TTY).

### Templating: two engines, both strict

- **Dashboard** (`templates/index.html`, `src/dashboard/view.rs`) uses **askama** — compiled at build time. A template/field mismatch is a `cargo build` error, not a runtime one.
- **Prompt** (`WORKFLOW.md`, `src/prompt.rs`) uses **minijinja** in strict mode — an unknown `{{ issue.* }}` variable fails the dispatch attempt (treated as abnormal → backoff retry).

The dashboard self-polls via HTMX into a `<div id="content">` wrapper (`hx-select="#content"`), NOT the `<body>` — swapping `<body>` outerHTML breaks htmx and blanks the page.

### Path containment (`src/paths.rs`)

All paths derive from one canonical agent root. `issue_workspace` / `assert_contained` enforce that a child cwd cannot escape `workspace.root` (canonicalized, rejects `..`/symlinks).
