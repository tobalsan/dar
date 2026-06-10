# Agentropy

A self-contained, folder-scoped agent runtime. One Rust binary, run from inside
an agent folder. It polls an issue tracker, dispatches AI coding-agent children
into per-issue workspaces, watches them finish, and loops — with a live
dashboard to observe and control the world.

Supports Claude Code, Pi, Codex, and arbitrary CLI runners. Trackers: local
Markdown files or Linear.

## How it works

```
                 ┌─────────────── agentropy run ───────────────┐
   issues/       │  reconcile → collect_finished → dispatch      │   dashboard
  (the truth) ──▶│            (in-memory run state only)        │──▶ :7878 (HTMX + WS)
                 └──────────────────────┬──────────────────────┘
                                        ▼
                        runner child  (cwd = workspaces/ISSUE-N/)
                        edits its own issue file → state: done
```

Two state layers, kept strictly separate:

- **Issue state** — the `state:` field in each issue file. Changed only by the
  child agent or a human. **The orchestrator never writes it** (except for
  safety "park" writes when a run stalls or exhausts retries).
- **Run state** — the orchestrator's in-memory view of one dispatch attempt
  (`Running`, `RetryQueued`, `Cancelled`, `Failed`, `Succeeded`, …). Persisted
  to SQLite at `data/store.db`.

Every tick (`poll_interval_ms`):

1. **Reconcile** running children against their issue files. Missing/terminal/
   non-active issue → SIGTERM → 5s grace → SIGKILL, run state `Cancelled`.
   Stalled child (no output for `stall_timeout_ms`) → kill + park to
   `needs_human`.
2. **Collect finished** children: classify exit and schedule retries or record
   outcomes.
3. **Dispatch** up to `max_concurrent` candidates. Sort: priority asc (null
   last) → `created_at` asc → identifier. Due retries go first.

Exit classification:

- **Exit 0 + terminal issue** → `Succeeded`.
- **Exit 0 + still active** → 1s **continuation retry** (doesn't count against
  `max_retries`).
- **Abnormal exit** → exponential backoff `min(retry_backoff_ms · 2^attempt,
  30min)` up to `max_retries`, then `Failed` + park to `needs_human`.

State is in-memory: kill `agentropy` and rerun → it starts cold and trusts
whatever the issue files say. Run history is restored from SQLite on startup.

## Agent folder layout

```
my-agent/
├── agent.yaml          # base config
├── WORKFLOW.md         # prompt template (minijinja, strict) + frontmatter overrides
├── issues/             # one Markdown file per issue (local tracker only)
│   ├── ISSUE-1.md
│   └── ISSUE-2.md
├── workspaces/         # created at runtime, one dir per issue
├── data/
│   └── store.db        # SQLite: runs, events, claims, heartbeats
└── logs/
    └── agent.log
```

Move the folder, move the agent. Everything it needs lives inside.

## Build

```bash
cargo build --release          # → ./target/release/agentropy
```

For the `claude` runner: the **Claude Code CLI** (`claude`) must be installed
and authenticated on the host. Agentropy does not bundle, install, or auth it.

## CLI

```bash
# Scaffold the default WORKFLOW.md prompt in an agent folder.
agentropy init-workflow --dir ./my-agent
agentropy init-workflow --dir ./my-agent --force                      # overwrite existing
agentropy init-workflow --dir ./my-agent --linear-project-slug abc123 # seed Linear frontmatter
agentropy init-workflow --dir ./my-agent --expose-graphql-tool        # enable linear_graphql tool

# Validate agent.yaml + WORKFLOW.md + tracker. Exit code only.
agentropy doctor --dir ./my-agent

# Run the orchestration loop (long-running). Defaults to the current directory.
cd my-agent && agentropy run
agentropy run --dir ./my-agent          # or point at a folder

# Export the configured Linear project and issues to data/.
agentropy export --dir ./my-agent

# Quick start with the bundled example:
agentropy doctor --dir ./example-agent
agentropy run   --dir ./example-agent
open http://127.0.0.1:7878/
```

`run` loops until Ctrl-C or SIGTERM (children are killed on shutdown).

## Configuration (`agent.yaml`)

`agent.yaml` is the base config. Most fields can be overridden per-run in
`WORKFLOW.md` frontmatter (WORKFLOW.md wins).

```yaml
id: my-agent
name: "My Agent"

tracker:
  use: files                  # "files" or "linear"
  config:
    path: ./issues            # required when use: files
  active_states: [todo, in_progress]
  terminal_states: [done, cancelled]
  # needs_human: "Needs Human"    # state that stops re-dispatch (default: "Needs Human")
  # project_slug: abc123          # Linear project slugId (use: linear only)

runner:
  use: claude-code            # pi | claude | claude-code | codex | cli | fake
  command: claude             # executable; defaults to runner kind's canonical command
  # model: claude-opus-4-6   # passed to runners that accept --model
  max_run_timeout_ms: 3600000 # 1 h hard cap per attempt (alias: turn_timeout_ms)
  stall_timeout_ms: 300000    # 5 min silence → stall kill

orchestrator:
  poll_interval_ms: 10000
  max_concurrent: 3           # parallel slots
  max_retries: 3              # backoff retry cap (not counting continuation retries)
  retry_backoff_ms: 30000     # base delay; doubles each attempt, capped at 30 min
  # max_active_runs: 3        # park barrier: max consecutive completed runs without leaving active

workspace:
  root: ./workspaces          # supports $AGENT_HOME and ~ expansion

dashboard:
  bind: 127.0.0.1
  port: 7878
  # webhook_secret: ...       # HMAC-SHA256 secret for Linear webhook verification

hitl:
  notifier:
    use: stdout               # none | stdout | webhook | cli
    window_secs: 60           # burst-dedup window
    max_items: 5              # max unique notifications per window
    # webhook_url: https://...  # required when use: webhook
    # command: [notify-send]    # required when use: cli
```

## WORKFLOW.md

`WORKFLOW.md` = optional YAML frontmatter + Markdown prompt body.

The **body** is a minijinja template rendered per issue against `{{ issue.* }}`
variables. Strict mode: an unknown variable fails the dispatch attempt (treated
as abnormal → backoff retry).

The **frontmatter** overrides any matching `agent.yaml` field for the run
without touching the base config. All sections are optional:

```yaml
---
tracker:
  kind: linear                    # or "files"
  active_states: [In Progress]
  terminal_states: [Done, Cancelled]
  needs_human: "Needs Human"
  project_slug: abc123            # Linear project slugId
  # endpoint: https://api.linear.app/graphql

polling:
  interval_ms: 15000
  jitter_ms: 500
  max_concurrent: 2
  max_retries: 5
  retry_backoff_ms: 60000
  allow_stale: true               # keep last-good snapshot on reload error (default true)

workspace:
  root: ./workspaces
  reuse: true                     # reuse existing workspace dir (default true)
  cleanup_on_terminal: false      # remove workspace on success (default false)

agent:
  runner: claude                  # or pi, codex, cli
  command: claude
  model: claude-opus-4-6
  # provider: anthropic           # passed to runners that accept a provider flag
  # thinking: "8000"              # thinking budget; passed to pi runner
  # effort: high                  # reasoning effort; passed to codex runner
  max_run_timeout_ms: 1800000
  stall_timeout_ms: 300000
  max_active_runs: 3              # park barrier

hooks:
  after_create: ./scripts/setup.sh    # new workspace created
  before_run: ./scripts/pre-run.sh    # before each dispatch
  after_run: ./scripts/post-run.sh    # after each run (not called on kill)
  before_remove: ./scripts/cleanup.sh # before workspace removal

server:
  bind: 127.0.0.1
  port: 7878

linear:
  project: my-project
  worker_tool: true               # expose linear_graphql tool to child (alias: exposeGraphqlTool)
  webhook_secret: secret123       # HMAC-SHA256 secret for POST /webhook
---

You are working on {{ issue.identifier }}: {{ issue.title }}
...
```

Scaffold the canonical default body:

```bash
agentropy init-workflow --dir ./my-agent
```

The child must eventually leave the issue in a non-active state (or set it to
`needs_human`). The orchestrator never writes issue state (except safety parks).

## Runners

| `use` value | Binary | Protocol |
|---|---|---|
| `pi` (default) | `pi` | JSON-RPC turn request over stdin |
| `claude` / `claude-code` | `claude` | `claude -p --permission-mode bypassPermissions --add-dir <agent-folder>`, prompt on stdin |
| `codex` | `codex` | `codex app-server` + JSON-RPC turn request |
| `cli` | `sh` (configurable) | No stdin; reads from `AGENT_*` env vars |
| `fake` | `sh` | Echoes `$AGENT_PROMPT`; test shim only |

All runners spawn in their own process group so SIGTERM reaches the whole
subprocess tree. `pi` and `claude` persist per-issue session dirs under
`pi-sessions/ISSUE-N/` and `claude-sessions/ISSUE-N/` respectively.

## Environment variables exported to children

Every runner receives the following `AGENT_*` variables. Hook scripts receive
the subset applicable to their lifecycle point.

| Variable | Value |
|---|---|
| `AGENT_ISSUE_IDENTIFIER` | Issue identifier, e.g. `PROJ-42` |
| `AGENT_ISSUE_ID` | Same as above (alias) |
| `AGENT_RUN_ID` | Unique run attempt ID |
| `AGENT_PROJECT_ID` | Agent/project ID |
| `AGENT_WORKSPACE` | Absolute path to the per-issue workspace dir |
| `AGENT_WORKSPACE_ROOT` | Absolute path to the shared workspaces root |
| `AGENT_PROMPT` | Rendered prompt text |
| `AGENT_WORKER_PROMPT` | Same as `AGENT_PROMPT` (alias) |
| `AGENT_MODEL` | Model name (only when configured) |
| `AGENT_WORKER_MODEL` | Same as `AGENT_MODEL` (alias, only when configured) |
| `AGENT_LINEAR_GRAPHQL_TOOL` | `1` when the Linear GraphQL tool is enabled |
| `AGENT_SESSION_DIR` | Per-issue session directory (Pi and Claude runners only) |

`workspace.root` in `agent.yaml`/WORKFLOW.md supports `$AGENT_HOME` (expands
to the agent folder) and `~` (expands to `$HOME`) in addition to relative paths
(resolved against the agent folder) and absolute paths.

## Issue files (local tracker)

One Markdown file per issue under `issues/`. Filename = identifier. YAML
frontmatter drives the tracker; the body is the task.

```markdown
---
id: ISSUE-1
identifier: ISSUE-1
title: "Add a hello.md"
state: todo
priority: 1
created_at: 2026-01-15T10:00:00Z
---

Create a file `hello.md` in this workspace with the text "hello from agentropy".
```

## Linear tracker

Set `tracker.use: linear` (or `tracker.kind: linear` in WORKFLOW.md). Requires:

- `LINEAR_API_KEY` environment variable with a valid Linear API key.
- `tracker.project_slug` set to the Linear project's slugId.

The Linear tracker polls via GraphQL, scopes to the configured project, and
respects the configured `active_states` / `terminal_states` / `needs_human`
names. Rate-limit responses are handled with backoff. The tracker is read-only;
all issue-state writes are done by the child agent.

Safety "park" writes (stall, retries exhausted) use the `needs_human` state
value and are the only orchestrator writes to Linear.

## Dashboard

`http://127.0.0.1:<port>/` — single page, live updates via HTMX + WebSocket.
No auth.

**Panels:** agent identity, active runs with elapsed time and last event,
queue, retry queue, run history (persisted in SQLite across restarts).

**Controls** (mutate run state only, never issue state):

- **Stop** — kill all active children (SIGTERM → 5s grace → SIGKILL). Issue
  files untouched; next tick may re-dispatch.
- **Pause** — stop picking up new issues; current runs keep going.
- **Resume** — resume polling.

### HTTP API

```bash
# Controls
curl -X POST http://127.0.0.1:7878/control/stop
curl -X POST http://127.0.0.1:7878/control/pause
curl -X POST http://127.0.0.1:7878/control/resume

# Trigger an immediate tick
curl -X POST http://127.0.0.1:7878/tick

# Manually claim/dispatch an issue
curl -X POST http://127.0.0.1:7878/claim \
  -H 'Content-Type: application/json' \
  -d '{"identifier":"ISSUE-5"}'

# Per-run actions (run_id from /runs)
curl -X POST http://127.0.0.1:7878/runs/<run_id>/release
curl -X POST http://127.0.0.1:7878/runs/<run_id>/interrupt
curl -X POST http://127.0.0.1:7878/runs/<run_id>/kill

# Data
curl http://127.0.0.1:7878/health
curl http://127.0.0.1:7878/runs               # paged run list
curl http://127.0.0.1:7878/runs/<run_id>      # run detail
curl http://127.0.0.1:7878/runs/<run_id>/logs # run events (paged by event_id)
curl "http://127.0.0.1:7878/api/events/<identifier>?since=0&limit=100"

# Linear webhook (triggers an immediate tick; HMAC-SHA256 verified)
curl -X POST http://127.0.0.1:7878/webhook \
  -H 'Linear-Signature: sha256=<sig>' \
  -d '<linear-payload>'
```

## Persistence

SQLite at `<agent-folder>/data/store.db` (WAL mode). Tables: `runs`, `events`,
`claims`, `heartbeats`. On startup, any run whose PID was left open from a
previous invocation is killed and marked `crashed`. The in-memory history ring
is seeded from SQLite, so the dashboard's run history survives restarts.

Log file at `logs/agent.log` (also streamed to terminal stderr).

## HITL notifier

Fires on stall, safety park, and startup errors. Backends:

- `stdout` (default) — log line only.
- `webhook` — HTTP POST to `webhook_url` with a JSON batch.
- `cli` — pipe JSON batch to `command`.
- `none` — silent.

Burst-dedup: notifications are batched per `window_secs`; duplicate events
within the window are collapsed. Max `max_items` unique items per batch.
