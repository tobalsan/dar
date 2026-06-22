# Agentropy

A self-contained, folder-scoped agent runtime. One Rust binary, run from inside
an agent folder. It polls an issue tracker, dispatches AI coding-agent children
into per-issue workspaces, watches them finish, and loops — with a live
dashboard to observe and control the world.

Supports Pi, Codex, and arbitrary CLI runners. Trackers: local
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
├── logs/
│   └── agent.log
├── bin/agentropy       # agent's own binary (build B only)
└── .agentropy/         # committed composition crate (build B only)
```

Move the folder, move the agent. Everything it needs lives inside.

## Build

There are two independent build models. Pick based on your goal:

- **A — From-source build (repo dev, monolith):** you are hacking on the
  runtime itself or want a single checkout with all stock extensions. One
  `cargo build` produces one binary.
- **B — Agent-specific build (self-contained):** each agent folder carries its
  own composition crate (`.agentropy/`) and binary (`bin/agentropy`), links
  only what it uses, supports local extension crates, and can self-update.
  This is the "move folder, move agent" model.

### A · From-source build (repo dev, monolith)

```bash
cargo build --release          # → ./target/release/agentropy
```

One binary, all stock extensions baked in. To add or remove an extension,
edit the `dist/` composition root:

1. Add the crate as a dependency in `dist/Cargo.toml`:

   ```toml
   my-extension = { path = "../extensions/my-extension" }
   ```

2. Add one line to the `plugins![]` list in `dist/src/main.rs`:

   ```rust
   plugins![
       frontend_log::FrontendLogExtension,
       tracker_files::TrackerFilesExtension,
       tracker_linear::TrackerLinearExtension,
       orchestrator::OrchestratorExtension,
       dashboard::DashboardExtension::default(),
       runner_pi::RunnerPiExtension,
       runner_codex::RunnerCodexExtension,
       runner_cli::RunnerCliExtension,
       runner_fake::RunnerFakeExtension,
       chat_pi::ChatPiExtension,
       tui::TuiExtension,
       my_extension::MyExtension,           // <- new
   ],
   ```

3. `cargo build --release`.

Removing an extension is the reverse: delete its `plugins![]` line and
`dist/Cargo.toml` dependency, rebuild.

### B · Agent-specific build (self-contained)

```bash
agentropy init-build --dir ./my-agent   # one-time: writes .agentropy/ crate
agentropy build --dir ./my-agent        # → ./my-agent/bin/agentropy
```

The agent links only the extensions it uses, supports local extension crates
under `extensions/` inside the agent folder, and can self-update. See
[Self-contained agents (build B)](#self-contained-agents-build-b--per-agent-binary--self-update)
for the full workflow (folder layout, self-update loop, portability, local
extensions, and toolchain prerequisites).

## Extensions

The codebase is a cargo workspace: a domain-free host (`crates/agentropy-host`)
plus small contract crates (`crates/host-api`, `crates/cap-tracker`,
`crates/cap-runner`, `crates/cap-chat`, `crates/orchestrator-api`), with
features living as one crate each under `extensions/`. The binary is assembled
from an explicit plugin list in the composition root (`dist/`). Extensions
import `host-api` (and optionally one cap/api crate) and read zero host
internals.

Editing `dist/` is the **build A** path (see above). In **build B**, each agent
gets its own composition crate (`.agentropy/`) and can add agent-local
extensions under its own `extensions/` folder — scaffold one with
`cargo agentropy new my-extension --kind background` (or `service` /
`foreground`). See [Self-contained agents (build B)](#self-contained-agents-build-b--per-agent-binary--self-update).

For writing your own extension, see the [authoring guide](docs/extensions.md);
start from `extensions/example` (the living reference, kept green in CI).

### Enabling & configuring extensions

Linked is not the same as enabled. Tracker and runner extensions only
*register* named services; `agent.yaml` `use:` keys pick which one actually
runs:

```yaml
tracker:
  use: files            # files | linear

runner:
  use: pi               # pi | codex | cli | fake
```

Background extensions in `plugins![]` (orchestrator, dashboard, frontend-log)
start unconditionally. The foreground extension — the one that owns terminal
output — is selected per agent via the top-level `foreground:` key in
`agent.yaml` (default `"logs"`, the frontend-log extension; `"tui"` selects the
[terminal UI](#terminal-ui-foreground-tui)). An unknown id causes a clean boot
error and exit 1.

Per-extension config is passed via the top-level `extensions:` map in
`agent.yaml`, keyed by extension id. The host delivers each value to the
matching extension via `ConfigStore`. Missing section = empty config.

```yaml
foreground: logs          # optional; default "logs"

extensions:
  dashboard:
    port: 7878
```

Some stock extensions are **opt-in**: in an agent-specific build they link only
when their `extensions.<id>` section is present, so a binary without the section
behaves exactly as before. The **`scheduler`** extension is opt-in this way — it
fires per-agent cron jobs from `cron/jobs.json` (cron + IANA timezone, optional
`startAt`) on the agent's default runner and writes each run to
`cron/output/<job_id>/<timestamp>.md`:

```yaml
extensions:
  scheduler:
    enabled: true        # presence selects it; `enabled: false` = boot-time kill switch
    jobTimeoutMs: 600000 # default per-run timeout (10 min); per-job `timeoutMs` overrides
```

Execution guards (the no-drift parity semantics): a scheduled fire of a job that
is still running is skipped (and bookmarked) with its next run recomputed; every
run is bounded by a timeout (`extensions.scheduler.jobTimeoutMs`, default 10
minutes, overridable per job via `timeoutMs`) that kills the runner child and
records an error output. `extensions.scheduler.enabled: false` is a **boot-time**
kill switch: no timers arm and nothing fires, but `cron/jobs.json` stays
readable/writable. Because `extensions.*` config is frozen after boot, flipping
the switch (or `jobTimeoutMs`) takes effect only after a host restart. Invalid
`extensions.scheduler` config fails boot with a clean error naming the problem.

Parity gaps vs the aihub scheduler (later slices): no per-job model override, no
`sessionId`, no HTTP API or CLI, no hot reload of `cron/jobs.json`. See
[docs/extensions.md](docs/extensions.md#scheduler) for the job schema and output
format.

See [Configuration](#configuration-agentyaml) for the full `agent.yaml` reference.

## CLI

```bash
# Bootstrap the per-agent composition crate (.agentropy/) — one-time setup.
agentropy init-build --dir ./my-agent
agentropy init-build --dir ./my-agent --vendor   # vendor deps for offline use

# Build the agent's own binary → <folder>/bin/agentropy.
agentropy build --dir ./my-agent
agentropy build --dir ./my-agent --vendor --offline   # air-gapped build

# Refresh the per-agent Cargo.lock (deliberate dep bump; commit result).
agentropy lock-refresh --dir ./my-agent

# Self-update: recompose, build, doctor-gate, atomic swap, execv.
agentropy self rebuild --dir ./my-agent
agentropy self rebuild --dir ./my-agent --vendor --offline

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
  # team: ALG                     # Linear team key (use: linear only)
  # assignee: "@thinh"            # UUID / @displayName / name / email (use: linear only)
  # label: [bug, urgent]          # single label or list; OR within labels (use: linear only)

# Linear filters (project_slug / team / assignee / label) combine with strict AND;
# at least one must resolve or the daemon refuses to boot.

runner:
  use: pi                     # pi | codex | cli | fake
  command: pi                 # executable; defaults to runner kind's canonical command
  # model: gpt-5             # passed to runners that accept --model
  # provider: anthropic       # passed to runners that accept a provider flag
  # thinking: high            # reasoning level (alias: effort); see "Thinking / reasoning level"
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
  # team: ALG                     # Linear team key
  # assignee: "@thinh"            # UUID / @displayName / name / email
  # label: [bug, urgent]          # single label or list; OR within labels
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
  runner: pi                      # or codex, cli
  command: pi
  model: gpt-5
  # provider: openai              # passed to runners that accept a provider flag
  # thinking: high                # reasoning level (alias: effort); overrides runner.thinking
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
| `codex` | `codex` | `codex app-server` + JSON-RPC turn request |
| `cli` | `sh` (configurable) | No stdin; reads from `AGENT_*` env vars |
| `fake` | `sh` | Echoes `$AGENT_PROMPT`; test shim only |

All runners spawn in their own process group so SIGTERM reaches the whole
subprocess tree. `pi` persists per-issue session dirs under
`pi-sessions/ISSUE-N/`.

## Thinking / reasoning level

A single canonical reasoning-level knob, `runner.thinking` in `agent.yaml`
(overridable per run via WORKFLOW.md `agent.thinking`). `effort` is accepted as
an alias in both places. The value is a level word on the canonical scale:

```
none | minimal | low | medium | high | xhigh
```

The level is validated against the resolved runner's supported subset at
config-load / `doctor` time. An unsupported or unknown level fails with a clean
error naming the runner and its allowed values (it is never clamped or passed
through), and no dispatch is attempted. Absent → no flag is emitted and the
runner default applies.

| Runner | Mechanism | Supported levels |
|---|---|---|
| `pi` | `--thinking <level>` | `none` (mapped to pi's `off`), `minimal`, `low`, `medium`, `high`, `xhigh` |
| `codex` | `-c model_reasoning_effort=<level>` | `minimal`, `low`, `medium`, `high`, `xhigh` |
| `cli` / `fake` | ignored | — |

> **Breaking change:** the previous WORKFLOW.md `agent.thinking` semantics — a
> pi token-budget string like `"8000"` — have been removed. Only level words are
> accepted; a numeric value is a validation error. The OpenCode runner mapping
> (`reasoningEffort`) is a follow-up tracked under ALG-226.

## Environment variables exported to children

On startup, `agentropy run` and `agentropy doctor` load `<agent-folder>/.env`
when it exists. Values from the real process environment take precedence over
the file. `.env` entries are for the daemon's own config/secrets lookup and are
not exported wholesale to runner, hook, or HITL CLI child processes; those
children receive their inherited environment with file-loaded keys removed plus
the documented `AGENT_*` variables below. `agentropy init-workflow` ensures
`.env` is listed in the agent folder's `.gitignore`.

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
| `AGENT_SESSION_DIR` | Per-issue session directory (Pi runner only) |

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

- `LINEAR_API_KEY` environment variable with a valid Linear API key, either in
  the process environment or in `<agent-folder>/.env`.
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

## Terminal UI (`foreground: tui`)

Set `foreground: tui` in `agent.yaml` to replace the plain log stream with an
in-terminal UI: a **Chat** tab (interactive operator chat with an AI agent
running inside the agent folder), a **Logs** tab (the same lines `foreground:
logs` would print), and a **Dash** tab (run snapshot + Stop/Pause/Resume —
present only when the orchestrator extension is linked).

```yaml
foreground: tui

extensions:
  tui:
    chat:
      backend: pi       # optional; default: follow runner.use, then pi
      command: pi       # optional binary override
      model: gpt-5      # optional, forwarded to the backend
```

**Chat.** Backed by a long-lived `pi --mode rpc` child (cwd = the agent
folder, so it can read issues, workspaces, and logs with its own tools), with
session transcripts kept under `data/tui/sessions/`. The first message is
prepended with a context preamble (run snapshot summary + `issues/` listing).
Backend resolution at first message: `extensions.tui.chat.backend` if set,
else the configured `runner.use` when it has a registered chat backend, else
`pi` (with a transcript notice when the runner had no chat backend; chat is
disabled with a banner when nothing is registered).

**Keys:** `Tab`/`Shift+Tab` cycle tabs. Chat: `Enter` send, `Esc` abort the
in-flight turn, `PgUp`/`PgDn`/`End` scroll. Logs: arrow/page keys scroll,
`End` re-follows the tail. Dash: `p` pause, `r` resume, `s` stop (run state
only — issue files are never touched).

**Quitting quits the whole agent:** `Ctrl-C` anywhere, or `q` on the
Logs/Dash tabs (on Chat it types a "q"), exits the foreground — which shuts
agentropy down and kills running children, exactly like Ctrl-C on
`foreground: logs`.

When stdout is not a terminal (piped/CI), the TUI degrades to the exact
`foreground: logs` line stream.

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

## Self-contained agents (build B — per-agent binary + self-update)

This is **build B**. Each agent folder carries its own composition crate and
binary — no shared repo checkout required. Move the folder, move the agent.

### Setup (one-time per agent)

```bash
# Requires: Rust toolchain + cargo on the host (or vendor deps for offline use)

agentropy init-build --dir ./my-agent          # writes .agentropy/ — commit it
agentropy init-build --dir ./my-agent --vendor # also vendor deps for offline/air-gap

agentropy build --dir ./my-agent               # → ./my-agent/bin/agentropy
agentropy build --dir ./my-agent --vendor --offline   # air-gapped build
```

`.agentropy/` is the composition crate for this agent. Commit it alongside
`agent.yaml` and `WORKFLOW.md`; it pins which extensions the agent links.

### Worked example: a `worker` agent

A self-contained agent at `~/agents/worker` using the Linear tracker, the
Codex runner, a chat-enabled TUI, and one agent-local extension.

Prerequisites: `cargo`/`rustc` on PATH, plus the `cargo-agentropy` helper
(built from a repo checkout with `cargo build --release`, then put
`cargo-agentropy` on PATH).

1. Write `~/agents/worker/agent.yaml` — the `use:` / `foreground:` keys
   decide which stock extensions get linked:

   ```yaml
   id: worker
   name: "Worker"

   tracker:
     use: linear            # links tracker-linear
     project_slug: abc123   # Linear project slugId
   runner:
     use: codex             # links runner-codex
   foreground: tui          # links tui + frontend-log + chat-pi, and chat-codex
                            # (TUI chat backend follows runner.use)
   ```

   The Linear tracker needs `LINEAR_API_KEY` in `~/agents/worker/.env`.

2. Scaffold the prompt and one local extension (run from inside the folder):

   ```bash
   cd ~/agents/worker
   agentropy init-workflow --dir .                       # writes WORKFLOW.md
   cargo agentropy new standup-poster --kind background  # → extensions/standup-poster/
   ```

   The new crate's `Cargo.toml` carries the discovery marker
   `[package.metadata.agentropy] factory = "standup_poster::extension"` and a
   `pub fn extension() -> Box<dyn Extension>`. The scaffold pins `host-api`
   to the same `git`/`rev` source the composer uses for stock crates.

3. Bootstrap, build, run:

   ```bash
   agentropy init-build --dir .   # generates .agentropy/ (commit it)
   agentropy build --dir .        # → ~/agents/worker/bin/agentropy
   ./bin/agentropy run
   ```

**Where each extension comes from in the final binary:**

| Extension | Source | How it's linked |
|---|---|---|
| `orchestrator`, `tracker-linear`, `runner-codex`, `chat-codex`, `chat-pi`, `tui`, `frontend-log` | the agentropy repo | pinned `git = "…", rev = "…"` dep in `.agentropy/Cargo.toml`, feature-gated — only the subset `agent.yaml` selects is compiled in |
| `standup-poster` | the agent's own `extensions/standup-poster/` | relative `path = "../extensions/standup-poster"`, auto-discovered via its `[package.metadata.agentropy] factory` marker |

`orchestrator` and `tracker-linear` are always linked; the rest of the stock
subset follows `tracker.use` / `runner.use` / `foreground`. The composer
regenerates both the `[dependencies]` and the `plugins![…]` list in
`.agentropy/` on every `init-build` / `build` — stock entries
`#[cfg(feature = …)]`-gated, local entries always present, never hand-edited.

### Local extension crates

Drop an extension crate under the agent's `extensions/` folder. The composer
auto-discovers crates with agentropy package metadata. Scaffold one:

```bash
cargo agentropy new my-extension --kind background   # or service | foreground
```

The agent's `.agentropy/` composition root lists only what this agent needs —
unrelated stock extensions are not linked.

### Self-update loop

An agent can rebuild its own binary from inside the folder and hot-swap itself:

```bash
agentropy self rebuild --dir ./my-agent
agentropy self rebuild --dir ./my-agent --vendor --offline   # air-gapped
```

Sequence: recompose `.agentropy/` → `agentropy build` → `agentropy doctor` gate
→ atomic binary swap → `execv` into the new binary. The running process is
replaced in place; no external orchestration needed.

To bump dependencies deliberately (then commit the updated lock):

```bash
agentropy lock-refresh --dir ./my-agent
```

### Portability

`agentropy build` runs `cargo build --release` against the host's native
target — the result is a dynamically-linked binary for that platform/arch.
`bin/agentropy` can be copied to another host only if that host is
ABI-compatible (same OS, arch, and libc). It is not a portable static binary.

A Rust toolchain is not needed merely to *run* `bin/agentropy`, but is
required to rebuild or self-update (`agentropy build` /
`agentropy self rebuild`).

For truly air-gapped hosts, run `init-build --vendor` once on a connected
machine, commit the `vendor/` tree inside `.agentropy/`, then build offline
with `--vendor --offline`.
