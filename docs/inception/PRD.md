---
title: "Dar v0 — mini PRD"
status: draft
scope: prototype
platform: macOS arm64
---

# Dar v0

A self-contained, folder-scoped agent runtime. Single Rust binary, run from inside an agent folder. Symphony-aligned orchestration loop over a pluggable tracker, with the smallest dashboard that's still useful.

v0 proves the loop, the folder boundary, and the tracker abstraction. Nothing else.

## Goal

`cd my-agent && dar run` polls issues, dispatches a Claude Code subagent into a per-issue workspace, watches it finish, and moves on. A local dashboard shows what's happening and lets the operator stop the world if needed.

## Terminology

- **Issue state** — the `state:` field inside an issue file (e.g. `todo`, `in_progress`, `done`). Owned by the tracker. Mutated only by the agent (via its prompt/tools) or by a human editing the file. The orchestrator never writes it.
- **Run state** — the orchestrator's in-memory view of one dispatch attempt (`Running`, `RetryQueued`, `Cancelled`, `Failed`, `Succeeded`). Lives in process memory and log only.

## External dependencies

v0 ships **one Rust binary** (`dar`). It requires:

- **Claude Code CLI** (`claude`) installed and authenticated on the host. Dar does not bundle, install, or auth it.

No other system dependencies.

## Non-goals (v0)

- Channels (Slack/Discord/etc.)
- Chat UI
- WASM / subprocess extension host
- Scheduler / cron
- Hot reload
- Hub registration / outbound surfaces
- Multi-platform builds (macOS arm64 only)
- Persistence beyond log file
- Auth on the dashboard
- Multiple trackers (files only)

## Agent folder layout

```
my-agent/
├── agent.yaml
├── WORKFLOW.md
├── issues/
│   ├── ISSUE-1.md
│   ├── ISSUE-2.md
│   └── ISSUE-3.md
├── workspaces/
│   └── <created at runtime, one dir per issue>
└── logs/
    └── agent.log
```

Everything the agent needs lives in the folder. Move the folder, move the agent.

## `agent.yaml` (v0)

```yaml
id: my-agent
name: "My Agent"

tracker:
  use: files
  config:
    path: ./issues
  active_states: [todo, in_progress]
  terminal_states: [done, cancelled]

runner:
  use: claude-code
  command: claude
  max_run_timeout_ms: 1800000   # 30 min, REQUIRED

orchestrator:
  poll_interval_ms: 10000
  max_concurrent: 1             # v0: one issue at a time
  max_retries: 3
  retry_backoff_ms: 10000

workspace:
  root: ./workspaces

dashboard:
  bind: 127.0.0.1
  port: 7878
```

## `WORKFLOW.md` (v0)

Symphony-style: optional YAML frontmatter, Markdown prompt body. v0 frontmatter is empty or absent; the body is the prompt template, rendered with `minijinja` against `{{ issue.* }}`.

The workflow body is responsible for instructing the agent to **transition the issue state itself** when the task is complete — e.g. by editing the issue file's frontmatter. The orchestrator does not transition issue state.

```markdown
You are working on issue {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}

When you finish the task, update the issue file at `../../issues/{{ issue.identifier }}.md`
so its frontmatter `state:` becomes `done`. Then stop.
```

Strict template rendering: unknown variables fail the run attempt.

## Issue file format (`tracker-files`)

One Markdown file per issue under `./issues/`. Filename = identifier.

```markdown
---
id: ISSUE-1
identifier: ISSUE-1
title: "Add a hello.md"
state: todo
priority: 2
created_at: 2026-01-15T10:00:00Z
---

Create a file `hello.md` in this workspace with the text "hello from dar".
```

The tracker reads/writes these files directly. State transitions update the frontmatter `state:` field.

## Orchestration loop (Symphony §7–§8, trimmed)

The orchestrator **observes** issue state and **controls** child process lifetime. It never writes issue state.

1. Tick every `poll_interval_ms`.
2. Reconcile running runs: re-read each running issue's file.
   - If the file is missing or its state ∈ `terminal_states` → terminate the child (SIGTERM → grace → SIGKILL), set run state `Cancelled`, release the claim.
   - If state is in neither `active_states` nor `terminal_states` (e.g. agent moved it to `in_review`) → terminate the child, set run state `Cancelled`, release the claim. Do not re-dispatch until the issue is back in an active state.
   - Otherwise: keep running.
3. Validate config (best-effort).
4. Read candidate issues whose state ∈ `active_states`, not currently running, not retry-queued.
5. Sort: priority asc (null last), then `created_at` asc, then identifier.
6. While slots available (`max_concurrent`): dispatch the next eligible issue.
7. On worker **normal exit** (exit code 0): re-fetch the issue.
   - If state ∈ `terminal_states` → mark run `Succeeded`, release the claim.
   - If state ∈ `active_states` → schedule a short continuation retry (1s) on the same issue.
   - If state is neither → mark run `Succeeded`, release the claim. Do not re-dispatch.
8. On worker **abnormal exit** (non-zero, signal, or timeout): schedule a retry with backoff `min(retry_backoff_ms * 2^(n-1), 5min)` up to `max_retries`. On exhaustion, mark run `Failed` and release the claim. **Issue state is left untouched.** Failure is log-only.
9. Enforce `max_run_timeout_ms` per attempt: hard-kill child (SIGTERM → grace → SIGKILL) and treat as abnormal exit.

In-memory state only. On restart, the loop starts cold and re-reads issue files. Whatever the issue file says is the truth.

## Workspace handling (Symphony §9, trimmed)

- Per-issue dir: `<workspace.root>/<sanitized(issue.identifier)>/`
- Sanitization: any char outside `[A-Za-z0-9._-]` → `_`
- Containment invariant: child cwd MUST be inside `workspace.root`. Reject otherwise.
- Reused across runs. Never auto-deleted in v0.
- No hooks in v0. (Symphony's `before_run`/`after_run` deferred.)

## Runner: Claude Code (v0)

- Spawn `claude -p` (non-interactive print mode) with workspace as cwd. The rendered prompt is piped to the child's stdin; stdin is then closed.
- Stream stdout/stderr line by line into `./logs/agent.log` and the in-memory recent-events ring.
- Track child PID, started_at, last_event_at.
- Completion = process exit. Exit code 0 → normal exit; non-zero, signal, or `max_run_timeout_ms` → abnormal.
- The runner **does not** touch the issue file. Any issue-state change is the agent's responsibility, performed during its turn via filesystem edits or tools the workflow tells it to use.

## Dashboard (v0)

- Bind `127.0.0.1:<port>`. No auth.
- Single page, HTMX-driven, polls itself every 2s.
- Sections:
  - **Agent**: id, folder path, polling state (running/paused), tracker, runner.
  - **Active run**: issue identifier, state, workspace path, child PID, started_at, elapsed, last event line.
  - **Queue**: next N candidate issues in dispatch order.
  - **Retry queue**: issue, attempt, due_at, last error.
  - **Recent events**: last 50 lines (issue/runner stdout/stderr/lifecycle).
- Emergency controls (the only mutating actions — they affect run state only, never issue state):
  - `POST /control/stop` — stop the active child: SIGTERM, wait grace (5s), SIGKILL. Set run state `Cancelled`. The issue file is **not** modified; on the next tick the orchestrator will see it still in an active state and may re-dispatch unless the operator/agent edits it.
  - `POST /control/pause` — stop picking up new issues. Current run keeps running.
  - `POST /control/resume` — resume polling.

Pause state is in-memory only.

## CLI

```
dar run [--dir PATH]   # default: cwd; long-running
dar doctor [--dir PATH] # validate agent.yaml + WORKFLOW.md + tracker, exit code only
```

That's it for v0.

## Logging

- Single file: `./logs/agent.log`.
- Structured-ish: `time level issue=ID event=... msg=...`.
- Child stdout/stderr appended with prefix `child[ISSUE-1]: ...`.
- Stderr of `dar` also prints to terminal.

## Tracker abstraction (in-tree)

```rust
trait Tracker {
    fn poll_candidates(&self) -> Result<Vec<Issue>>;
    fn fetch_states(&self, ids: &[String]) -> Result<Vec<Issue>>;
    fn fetch_terminal(&self) -> Result<Vec<Issue>>;
    fn fetch_one(&self, id: &str) -> Result<Option<Issue>>;
    // v0 write surface (used by runner on normal exit; not by orchestrator):
}
```

`FileTracker` is the only impl in v0. The trait has **no write verbs**: in v0 the orchestrator never writes issue state, and the agent edits the issue file directly via its workspace tools. Write verbs (`transition`, `comment`, `link_pr`) will be added later as optional capabilities exposed to the agent as tools, not called by the orchestrator.

Trait exists to lock the read verb set and unblock `LinearTracker` next.

## Domain model (v0)

```rust
struct Issue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    state: String,
    priority: Option<i32>,
    assignees: Vec<String>,
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    // pr, blocked_by deferred
}
```

## Stack

- `tokio`, `axum`, `tower-http`
- `serde`, `serde_yaml`, `serde_json`
- `askama` (compile-time templates for dashboard)
- `minijinja` (runtime templates for `WORKFLOW.md` prompt rendering)
- `rust-embed` (static dashboard assets if any)
- `tokio::process` for child
- `notify` — **not** in v0 (no hot reload)
- `tracing` + `tracing-appender` for logs
- `clap` for CLI

Single static binary via `cargo build --release`.

## Success criterion (the demo)

Given:
- An agent folder with `agent.yaml`, `WORKFLOW.md`, and three `issues/ISSUE-{1,2,3}.md` files in state `todo`.
- A `WORKFLOW.md` whose prompt instructs the agent to edit its issue file's frontmatter `state:` to `done` when the task is complete.
- Claude Code CLI installed and authenticated on the host.

When the operator runs `dar run` in the folder and opens `http://localhost:7878/`, then:

1. Within one poll tick, ISSUE-1 appears as the active run with a workspace at `./workspaces/ISSUE-1/`.
2. The Claude child produces its output; lines stream into the dashboard's recent events.
3. The agent edits `./issues/ISSUE-1.md` to set `state: done` and exits. The next tick observes the terminal state and dispatches ISSUE-2.
4. Pressing **Stop** during ISSUE-2 terminates the child within the grace window; run state becomes `Cancelled`; the issue file is unchanged. On the next tick, because the issue is still in `todo`/`in_progress`, the orchestrator re-dispatches it (this is expected v0 behavior).
5. Pressing **Pause** prevents ISSUE-3 from starting; **Resume** lets it proceed.
6. Killing `dar` and rerunning resumes from whatever the issue files say. No in-memory state is required for correctness.

If the above runs without manual intervention beyond the dashboard controls, v0 is done.

## Out of scope, explicitly

- `tracker-linear` — next adapter, validates the trait. Not v0.
- WASM / subprocess extensions — v1.
- Chat UI — v0.5.
- Channels (Slack/Discord) — v1+.
- Hub registration — v1+.
- Workflow hooks — v0.5 (`before_run` first).
- Hot reload — v0.5.
- Multi-platform builds — when needed.
- SQLite / persistent state — only if a real need shows up.

## Open questions to resolve during implementation

- Whether to treat "Stop then no operator edit" as automatic re-dispatch (current v0 behavior) or to add an in-memory "do not re-dispatch this issue until restart" set. v0 default: re-dispatch; revisit if it bites.
- How to detect "agent gave up cleanly" vs "task complete" beyond exit code + issue state. v0: rely on the agent transitioning the issue file. Exit code 0 with the issue still active triggers a continuation retry by design.
- Claude CLI flags beyond `-p`: model selection, tool permissions, timeout. v0: rely on the user's local Claude Code config; dar passes none.
