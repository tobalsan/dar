# Dar

A self-contained, folder-scoped agent runtime. One Rust binary, run from inside
an agent folder. It polls an issue tracker, dispatches AI coding-agent children
into per-issue workspaces, watches them finish, and loops — with a live
dashboard to observe and control the world.

Supports Pi, Codex, and arbitrary CLI runners. Trackers: local
Markdown files or Linear.

## How it works

```
                 ┌─────────────── dar run ───────────────┐
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

State is in-memory: kill `dar` and rerun → it starts cold and trusts
whatever the issue files say. Run history is restored from SQLite on startup.

## Agent folder layout

```
my-agent/
├── agent.yaml          # base config
├── WORKFLOW.md         # required only when the issue loop is enabled (tracker/polling/workspace live here)
├── issues/             # required only for the local files tracker
│   ├── ISSUE-1.md
│   └── ISSUE-2.md
├── workspaces/         # created at runtime by the orchestrator loop
├── data/
│   └── store.db        # SQLite: runs, events, claims, heartbeats
├── logs/
│   └── agent.log
├── bin/dar       # agent's own binary (build B only)
└── .dar/         # committed composition crate (build B only)
```

Move the folder, move the agent. Everything it needs lives inside.

## Build

There are two independent build models. Pick based on your goal:

- **A — From-source build (repo dev, monolith):** you are hacking on the
  runtime itself or want a single checkout with all stock extensions. One
  `cargo build` produces one binary.
- **B — Agent-specific build (self-contained):** each agent folder carries its
  own composition crate (`.dar/`) and binary (`bin/dar`), links
  only what it uses, supports local extension crates, and can self-update.
  This is the "move folder, move agent" model.

### A · From-source build (repo dev, monolith)

```bash
cargo build --release          # → ./target/release/dar
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
dar init-build --dir ./my-agent   # one-time: writes .dar/ crate
dar build --dir ./my-agent        # → ./my-agent/bin/dar
```

The agent links only the extensions it uses, supports local extension crates
under `extensions/` inside the agent folder, and can self-update. See
[Self-contained agents (build B)](#self-contained-agents-build-b--per-agent-binary--self-update)
for the full workflow (folder layout, self-update loop, portability, local
extensions, and toolchain prerequisites).

## Extensions

The codebase is a cargo workspace: a domain-free host (`crates/dar-host`)
plus small contract crates (`crates/host-api`, `crates/cap-tracker`,
`crates/cap-runner`, `crates/cap-chat`, `crates/orchestrator-api`), with
features living as one crate each under `extensions/`. The binary is assembled
from an explicit plugin list in the composition root (`dist/`). Extensions
import `host-api` (and optionally one cap/api crate) and read zero host
internals.

Editing `dist/` is the **build A** path (see above). In **build B**, each agent
gets its own composition crate (`.dar/`) and can add agent-local
extensions under its own `extensions/` folder — scaffold one with
`cargo dar new my-extension --kind background` (or `service` /
`foreground`). See [Self-contained agents (build B)](#self-contained-agents-build-b--per-agent-binary--self-update).

For writing your own extension, see the [authoring guide](docs/extensions.md);
start from `extensions/example` (the living reference, kept green in CI).

### Enabling & configuring extensions

Linked is not the same as enabled. Runner and tracker extensions only
*register* named services. `runner.use` is always required and selects the
agent harness/model backend:

```yaml
runner:
  use: pi               # pi | codex | cli | fake
```

The issue loop itself is a `WORKFLOW.md` concern, not `agent.yaml`: tracker
(`tracker.kind`, `tracker.projects`, states, …), polling, and workspace config
all live in `WORKFLOW.md` frontmatter — see [WORKFLOW.md](#workflowmd) below.
Background extensions in `plugins![]` (orchestrator, dashboard, frontend-log)
start unconditionally. With no resolved `WORKFLOW.md`, or one whose
frontmatter is missing `tracker.kind` or non-empty `active_states`/
`terminal_states`, the orchestrator starts in passive mode: no issue loop, no
tracker required. The foreground extension — the one that owns terminal
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

Jobs can also be managed remotely over the host HTTP server under `/scheduler`
(list/create/update/delete); see
[docs/extensions.md](docs/extensions.md#scheduler-http-api). A create/update/delete
re-arms the timer in-process so a sooner schedule fires immediately.
`POST /scheduler/jobs/{id}/run-now` fires a job immediately without disturbing
its schedule (the next fire is preserved unless a scheduled fire was
overlap-skipped during the manual run), and `GET /scheduler/jobs/{id}/tail`
returns the newest output file for a job — an operator test-and-inspect loop
over HTTP.

Parity gaps vs the aihub scheduler (later slices): no per-job model override, no
`sessionId`, no CLI, no hot reload of `cron/jobs.json`. See
[docs/extensions.md](docs/extensions.md#scheduler) for the job schema and output
format.

See [Configuration](#configuration-agentyaml) for the full `agent.yaml` reference.

## CLI

```bash
# Bootstrap the per-agent composition crate (.dar/) — one-time setup.
dar init-build --dir ./my-agent
dar init-build --dir ./my-agent --vendor   # vendor deps for offline use

# Build the agent's own binary → <folder>/bin/dar.
dar build --dir ./my-agent
dar build --dir ./my-agent --vendor --offline   # air-gapped build

# Refresh the per-agent Cargo.lock (deliberate dep bump; commit result).
dar lock-refresh --dir ./my-agent

# Self-update: recompose, build, doctor-gate, atomic swap, execv.
dar self rebuild --dir ./my-agent
dar self rebuild --dir ./my-agent --vendor --offline
dar self rebuild my-agent
dar self rebuild my-agent --workflow ./my-agent/workflows/release

# Scaffold the default WORKFLOW.md prompt in an agent folder.
dar init-workflow --dir ./my-agent
dar init-workflow --dir ./my-agent --force                      # overwrite existing
dar init-workflow --dir ./my-agent --linear-project-slug abc123 # seed Linear frontmatter
dar init-workflow --dir ./my-agent --expose-graphql-tool        # enable linear_graphql tool

# Validate agent.yaml. If the resolved WORKFLOW.md has a valid loop config,
# also validates the tracker. Exit code only.
dar doctor --dir ./my-agent

# Run the agent host (long-running). When the resolved WORKFLOW.md has a
# valid loop config, this runs the issue loop; otherwise it runs
# foreground/custom extensions only.
cd my-agent && dar run
dar run --dir ./my-agent          # or point at a folder

# Run a non-default workflow: one agent identity, several WORKFLOW.md hats.
# --workflow accepts a directory (its WORKFLOW.md is used) or an explicit
# .../WORKFLOW.md path; also accepted by doctor and export.
dar run --dir ./my-agent --workflow ./workflows/triage
dar run --dir ./my-agent --workflow ./workflows/triage/WORKFLOW.md

# Export the configured tracker's project and issues to data/.
dar export --dir ./my-agent
dar export --dir ./my-agent --workflow ./workflows/triage

# Quick start with the bundled example:
dar doctor --dir ./example-agent
dar run   --dir ./example-agent
open http://127.0.0.1:7878/
```

`run` loops until Ctrl-C or SIGTERM (children are killed on shutdown).

## Configuration (`agent.yaml`)

`agent.yaml` is agent **identity and host config only**: `id`, `name`,
`runner`, `hitl`, `dashboard`, `foreground`, `providers`, `extensions`,
`system_files`. The issue-loop config — tracker, polling, workspace — lives
entirely in `WORKFLOW.md` frontmatter (see [WORKFLOW.md](#workflowmd) below);
`agent.yaml` has no tracker/orchestrator/workspace keys of its own. Old
`agent.yaml` files that still carry those keys keep parsing: they're unknown
fields now, silently ignored.

```yaml
id: my-agent
name: "My Agent"

runner:
  use: pi                     # pi | codex | cli | fake
  command: pi                 # executable; defaults to runner kind's canonical command
  # model: gpt-5             # passed to runners that accept --model
  # provider: anthropic       # passed to runners that accept a provider flag
  # thinking: high            # reasoning level (alias: effort); see "Thinking / reasoning level"
  max_run_timeout_ms: 3600000 # 1 h hard cap per attempt (alias: turn_timeout_ms)
  stall_timeout_ms: 300000    # 5 min silence → stall kill

# Optional identity files for agent prompts. Root AGENTS.md loads first when present.
system_files:
  - docs/style.md             # optional path, relative to agent folder
  - path: docs/policy.md
    required: true            # missing file fails `dar doctor`

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

### System files

`AGENTS.md` in agent folder loads automatically, first. `system_files` adds
agent-folder-relative files in listed order. Entries are optional unless
`required: true`; missing optional files warn and skip. Paths cannot escape
agent folder. Do not list `AGENTS.md` again.

## WORKFLOW.md

`WORKFLOW.md` is required only when the issue loop is enabled: the resolved
`WORKFLOW.md` must exist and its frontmatter must carry `tracker.kind` plus
non-empty `active_states`/`terminal_states`, or the orchestrator stays
passive. Passive agents may omit it.

When present, `WORKFLOW.md` = optional YAML frontmatter + Markdown prompt body.
The **body** is a minijinja template rendered per issue against `{{ issue.* }}`
variables. Strict mode: an unknown variable fails the dispatch attempt (treated
as abnormal → backoff retry).

The **frontmatter** is the sole home for the issue-loop config (tracker,
polling, workspace) — `agent.yaml` carries none of it. The `agent:` section is
the one exception: it overrides matching `agent.yaml` `runner` fields for the
run, without touching the base config. All sections are optional (defaults
noted inline):

```yaml
---
tracker:
  kind: linear                    # files | linear | plane
  active_states: [In Progress]    # issue states the loop treats as dispatchable
  terminal_states: [Done, Cancelled] # issue states that stop the loop; no retry
  needs_human: "Needs Human"      # pauses re-dispatch; no default (omit ⇒ no parking)
  projects: abc123                 # scalar: one Linear slugId / Plane project UUID
  # projects: [abc123, def456]     # or a list: OR-matched (Linear), merged fetch (Plane)
  # path: ./issues                 # files tracker only; default "issues", relative to this WORKFLOW.md
  # team: ALG                     # Linear team key filter
  # assignee: "@thinh"            # UUID / @displayName / name / email
  # label: [bug, urgent]          # single label or list; OR within labels
  # endpoint: https://api.linear.app/graphql   # Linear GraphQL endpoint override

polling:
  interval_ms: 15000              # poll frequency, ms (default 1000)
  jitter_ms: 500                  # random jitter added to each poll (default 0)
  max_concurrent: 2               # parallel run slots (default 3)
  max_retries: 5                  # backoff retry cap; doesn't count continuation retries (default 3)
  retry_backoff_ms: 60000         # base delay; doubles each attempt, capped at 30 min (default 30000)
  allow_stale: true               # keep last-good snapshot on reload error (default true)

workspace:
  root: ./workspaces              # relative to WORKFLOW.md's dir; supports $AGENT_HOME/~ (default "workspaces")
  reuse: true                     # reuse existing workspace dir (default true)
  cleanup_on_terminal: false      # remove workspace once the run reaches a terminal outcome (default false)

agent:
  runner: pi                      # runner override: pi | codex | cli (default: agent.yaml runner.use)
  command: pi                     # runner binary override (default: agent.yaml runner.command)
  model: gpt-5                    # model id passed to the runner (default: agent.yaml runner.model)
  # provider: openai              # passed to runners that accept a provider flag
  # thinking: high                # reasoning level (alias: effort); overrides runner.thinking
  max_run_timeout_ms: 1800000     # hard per-attempt timeout (alias: turn_timeout_ms)
  stall_timeout_ms: 300000        # no runner events for this long → stall kill
  max_active_runs: 3              # park barrier: runs completed in a row w/o leaving active state (default 3)

hooks:
  after_create: ./scripts/setup.sh    # new workspace created
  before_run: ./scripts/pre-run.sh    # before each dispatch
  after_run: ./scripts/post-run.sh    # after each run (not called on kill)
  before_remove: ./scripts/cleanup.sh # before workspace removal

server:
  bind: 127.0.0.1                 # dashboard bind override (default: agent.yaml dashboard.bind)
  port: 7878                      # dashboard port override (default: agent.yaml dashboard.port)

linear:
  project: my-project             # shown to the agent in the rendered prompt context (informational only)
  worker_tool: true               # deprecated no-op: linear_graphql is now always on via the host MCP bridge
  webhook_secret: secret123       # HMAC-SHA256 secret for POST /webhook (default: dashboard.webhook_secret)
---

You are working on {{ issue.identifier }}: {{ issue.title }}
...
```

### Template variables

The body is rendered under minijinja strict mode: referencing anything not
listed here fails the dispatch attempt (treated as abnormal → backoff retry).
Nullable fields render as `none` when absent — guard with `{% if %}`.

- `issue.*` — the tracker's read-only view of the current issue: `id`,
  `identifier`, `title`, `description` (nullable), `url` (nullable), `state`,
  `priority` (nullable), `assignees` (list), `labels` (list), `created_at`
  (nullable), `updated_at` (nullable), `parent_id` (nullable), `blocked_by`
  (list), `project_name` (nullable), `project_slug` (nullable), `metadata`
  (tracker-native extra fields, map).
- `attempt` — 0-based retry counter for this dispatch.
- `workflow.dir` — directory containing the resolved `WORKFLOW.md` (the repo
  root, for WORKFLOW.md-inside-a-repo setups).
- `workflow.file` — the resolved `WORKFLOW.md` path itself.

Fields omitted from frontmatter fall back to: `poll_interval_ms 1000`,
`max_concurrent 3`, `max_active_runs 3`, `max_retries 3`, `retry_backoff_ms
30000`, `jitter_ms 0`, `workspace.root "workspaces"`, `reuse true`,
`cleanup_on_terminal false`. `tracker.needs_human` has no default — omitting
it means the orchestrator has no dedicated parking state.

Scaffold the canonical default body:

```bash
dar init-workflow --dir ./my-agent
```

The child must eventually leave the issue in a non-active state (or set it to
`needs_human`). The orchestrator never writes issue state (except safety parks).

### Running a non-default workflow (`--workflow`)

One agent identity (`agent.yaml`, `.env`, system files) can drive more than
one `WORKFLOW.md` — "one agent, many hats." `dar run`, `dar doctor`, and
`dar export` all accept `--workflow <path>`:

```bash
dar run --dir ./my-agent                                            # default: <dir>/WORKFLOW.md
dar run --dir ./my-agent --workflow ./workflows/triage               # a dir containing WORKFLOW.md
dar run --dir ./my-agent --workflow ./workflows/triage/WORKFLOW.md   # or the explicit file
```

`--workflow` takes a directory (its `WORKFLOW.md` is used) or an explicit path
that must be named `WORKFLOW.md`. Workflow identity is the *canonical*
resolved path, so re-running the same `--workflow` value resumes its state.
Everything still lives under the agent folder:

- **Default workflow** (`<agent>/WORKFLOW.md`, i.e. no flag, or a flag that
  resolves to it): unchanged, legacy layout — `<agent>/data/store.db`,
  `<agent>/logs/agent.log`.
- **Non-default workflow**: run-history db + logs live under
  `<agent>/workflows/<key>/{data/store.db,logs/agent.log}`, where `<key>` is
  `<workflow-dir-basename>-<shorthash-of-canonical-path>` (e.g.
  `triage-3f9c2a`), so concurrent workflows never share state.
- **Identity** — `agent.yaml`, `.env`, system files, extension data dirs
  (`cron/`, `pi-sessions/`, …) — always resolves against the agent root, never
  the workflow dir.
- **Workspaces** resolve relative `workspace.root` values against the
  WORKFLOW.md's own directory. `$AGENT_HOME` explicitly targets the agent
  identity root (default `workspaces`, so the default layout is unchanged).

A non-default `--workflow` process skips **agent-singleton extensions** — the
scheduler, and any future extension that connects the agent to an external
surface at most once (e.g. a Telegram/IRC bridge). Chat backends (`chat-pi`,
`chat-codex`, `chat-opencode`) are not singletons: the TUI Chat tab works
normally in every `--workflow` process.

`dar dash` tracks one live presence entry per agent **+ workflow**, so
concurrent `dar run --workflow` processes for the same agent both show up
without clobbering each other; a workflow's own dashboard header shows
`id · folder · workflow`.

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

On startup, `dar run` and `dar doctor` load `<agent-folder>/.env`
when it exists. Values from the real process environment take precedence over
the file. `.env` entries are for the daemon's own config/secrets lookup and are
not exported wholesale to runner, hook, or HITL CLI child processes; those
children receive their inherited environment with file-loaded keys removed plus
the documented `AGENT_*` variables below. `dar init-workflow` ensures
`.env` is listed in the agent folder's `.gitignore`.

While `dar run` is active, the orchestrator checks the parsed `.env` content at
each poll interval. A complete valid replacement updates file-owned values and
removes file-owned values no longer present; real process environment values
remain authoritative. Reload consumers opt in (the configured tracker does), so
this is not universal cache invalidation or immediate credential revocation.
Already-running children and sibling MCP bridge processes retain their own
environment until they exit. The `reload_secrets` MCP tool refreshes only its
bridge-local process; the live host converges on its next poll.

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

`workspace.root` in WORKFLOW.md supports `$AGENT_HOME` (expands to the agent
identity folder) and `~` (expands to `$HOME`) in addition to relative paths
(resolved against the WORKFLOW.md folder) and absolute paths.

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

Create a file `hello.md` in this workspace with the text "hello from dar".
```

## Linear tracker

Set `tracker.kind: linear` in `WORKFLOW.md` frontmatter (a build B binary also
needs agent.yaml `tracker: {use: linear}` to link the `tracker-linear`
extension — see [self-contained agents](#self-contained-agents-build-b--per-agent-binary--self-update)).
Requires:

- A Linear auth token, either in the process environment or in
  `<agent-folder>/.env`. Two token types are supported via the same header:
  - `LINEAR_API_KEY` — a personal API key, sent raw (`Authorization: <key>`).
  - `LINEAR_OAUTH_TOKEN` — an OAuth app access token (`actor=app`), sent as
    `Authorization: Bearer <token>`. App tokens are long-lived and don't
    consume a workspace seat. When both are set, `LINEAR_OAUTH_TOKEN` wins.
- `tracker.projects` set to the Linear project's slugId (scalar), or a list of
  slugIds — matched OR (`{"or":[{"project":{"slugId":{"eq":p}}},…]}`).

A complete minimal runnable frontmatter — tracker + polling + workspace, the
three sections the loop actually needs:

```yaml
# WORKFLOW.md frontmatter
tracker:
  kind: linear
  active_states: [In Progress]
  terminal_states: [Done, Cancelled]
  needs_human: "Needs Human"
  projects: abc123          # Linear project slugId

polling:
  interval_ms: 10000        # poll every 10s
  max_concurrent: 2         # 2 parallel run slots

workspace:
  root: ./workspaces
```

```yaml
# agent.yaml — only needed for a build B (self-contained) binary
tracker:
  use: linear                # links tracker-linear at build time
```

The Linear tracker polls via GraphQL, scopes to the configured project(s), and
respects the configured `active_states` / `terminal_states` / `needs_human`
names. Rate-limit responses are handled with backoff. The tracker is read-only;
all issue-state writes are done by the child agent. `dar export` requires
exactly one configured project (bails on 0 or more than 1).

Linear's app/agent assignment appears as `delegate` in the API while the parent
human/account remains `assignee`. Use `tracker.assignee` to target the human
assignee and `tracker.delegate` to target a delegated app/agent such as
`@workeragent`; both resolve by UUID, display name, name, or email and compose
with project, team, label, and state filters using AND semantics.

Safety "park" writes (stall, retries exhausted) use the `needs_human` state
value and are the only orchestrator writes to Linear.

## Plane tracker

Set `tracker.kind: plane` in `WORKFLOW.md` frontmatter (a build B binary also
needs agent.yaml `tracker: {use: plane}` to link the `tracker-plane`
extension). Scope the tracker to one Plane workspace + zero or more projects,
and provide a Plane auth token in the environment or `<agent-folder>/.env`:

- `PLANE_BOT_TOKEN` — a Plane bot/OAuth token (`Authorization: Bearer <token>`).
- `PLANE_OAUTH_TOKEN` — legacy/alternate OAuth token env var, also Bearer.
- `PLANE_API_KEY` — a personal API key (`X-API-Key: <key>`). Bearer tokens win
  over `PLANE_API_KEY`; `PLANE_BOT_TOKEN` wins over `PLANE_OAUTH_TOKEN`.

```yaml
# WORKFLOW.md frontmatter
tracker:
  kind: plane
  active_states: [Todo, "In Progress"]
  terminal_states: [Done, Cancelled]
  needs_human: "Needs Human"
  workspace: my-workspace          # workspace slug
  projects: 00000000-0000-0000-0000-000000000000   # one project UUID (scalar)
  # projects: [00000000-…, 11111111-…]             # or a list: fetched per-project and merged
  # endpoint: https://api.plane.so   # self-hosted API base override
  mention: Worker Agent  # optional bot display name; filters description @mentions
```

```yaml
# agent.yaml
extensions:
  tracker-plane:
    # app_url: https://app.plane.so   # self-hosted web app base override
```

Empty/absent `tracker.projects` polls the whole workspace, as before. The Plane
tracker polls work items via the REST API, resolves state names from the
project's state table, skips issues blocked by non-terminal `blocked_by`
relations, and honours Plane's rate-limit headers. Optional `tracker.mention`
resolves a bot display name at boot and polls only work items whose description
@mentions that bot. It also exposes a `plane_api` host tool (host-held auth,
token redaction) so the child agent can call the Plane REST API directly. `dar
init-workflow` / `dar export` route to the Plane tracker automatically when
`tracker.kind: plane`. `dar export` requires exactly one configured project.

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
curl -X POST http://127.0.0.1:7878/self-update/rebuild # returns 202
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

`/self-update/rebuild` is intentionally unauthenticated in v1: the dashboard
port is a trusted control plane and must be limited to localhost or a trusted
network. It returns `202` before rebuild/restart; concurrent requests return
`409`. A fast exec can interrupt delivery, so clients confirm success through
the changed dashboard boot identity and `/health`; `dar self rebuild <agent>`
does this automatically. Name lookup requires dashboard presence and
`--workflow` disambiguates multiple live workflows.

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
dar down and kills running children, exactly like Ctrl-C on
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

dar init-build --dir ./my-agent          # writes .dar/ — commit it
dar init-build --dir ./my-agent --vendor # also vendor deps for offline/air-gap

dar build --dir ./my-agent               # → ./my-agent/bin/dar
dar build --dir ./my-agent --vendor --offline   # air-gapped build
```

`.dar/` is the composition crate for this agent. Commit it alongside
`agent.yaml` and `WORKFLOW.md`; it pins which extensions the agent links.

### Worked example: a `worker` agent

A self-contained agent at `~/agents/worker` using the Linear tracker, the
Codex runner, a chat-enabled TUI, and one agent-local extension.

Prerequisites: `cargo`/`rustc` on PATH, plus the `cargo-dar` helper
(built from a repo checkout with `cargo build --release`, then put
`cargo-dar` on PATH).

1. Write `~/agents/worker/agent.yaml` — the `use:` / `foreground:` keys
   decide which stock extensions get linked. `tracker.use` here only selects
   the linked tracker crate at build time; the tracker's actual behavior
   (project scope, states, …) is configured in `WORKFLOW.md`, not here:

   ```yaml
   id: worker
   name: "Worker"

   tracker:
     use: linear             # links tracker-linear (build-time selection only)
   runner:
     use: codex             # links runner-codex
   foreground: tui          # links tui + frontend-log + chat-pi, and chat-codex
                            # (TUI chat backend follows runner.use)
   ```

   The Linear tracker needs `LINEAR_API_KEY` (personal API key) or
   `LINEAR_OAUTH_TOKEN` (OAuth app token, sent with a `Bearer ` prefix) in
   `~/agents/worker/.env`.

2. Scaffold the prompt and one local extension (run from inside the folder):

   ```bash
   cd ~/agents/worker
   dar init-workflow --dir . --linear-project-slug abc123  # writes WORKFLOW.md with tracker.projects
   cargo dar new standup-poster --kind background          # → extensions/standup-poster/
   ```

   Add `tracker.delegate: "@workeragent"` (or `tracker.team`, `tracker.label`,
   …) to the generated `WORKFLOW.md` frontmatter for any other Linear filters
   — see [Linear tracker](#linear-tracker).

   The new crate's `Cargo.toml` carries the discovery marker
   `[package.metadata.dar] factory = "standup_poster::extension"` and a
   `pub fn extension() -> Box<dyn Extension>`. The scaffold pins `host-api`
   to the same `git`/`rev` source the composer uses for stock crates.

3. Bootstrap, build, run:

   ```bash
   dar init-build --dir .   # generates .dar/ (commit it)
   dar build --dir .        # → ~/agents/worker/bin/dar
   ./bin/dar run
   ```

**Where each extension comes from in the final binary:**

| Extension | Source | How it's linked |
|---|---|---|
| `orchestrator`, `tracker-linear`, `runner-codex`, `chat-codex`, `chat-pi`, `tui`, `frontend-log` | the dar repo | pinned `git = "…", rev = "…"` dep in `.dar/Cargo.toml`, feature-gated — only the subset `agent.yaml` selects is compiled in |
| `standup-poster` | the agent's own `extensions/standup-poster/` | relative `path = "../extensions/standup-poster"`, auto-discovered via its `[package.metadata.dar] factory` marker |

`orchestrator` and `tracker-linear` are always linked; the rest of the stock
subset follows `tracker.use` / `runner.use` / `foreground`. The composer
regenerates both the `[dependencies]` and the `plugins![…]` list in
`.dar/` on every `init-build` / `build` — stock entries
`#[cfg(feature = …)]`-gated, local entries always present, never hand-edited.

### Local extension crates

Drop an extension crate under the agent's `extensions/` folder. The composer
auto-discovers crates with dar package metadata. Scaffold one:

```bash
cargo dar new my-extension --kind background   # or service | foreground
```

The agent's `.dar/` composition root lists only what this agent needs —
unrelated stock extensions are not linked.

### Self-update loop

An agent can rebuild its own binary from inside the folder and hot-swap itself:

```bash
dar self rebuild --dir ./my-agent
dar self rebuild --dir ./my-agent --vendor --offline   # air-gapped
```

Sequence: recompose `.dar/` → `dar build` → `dar doctor` gate
→ atomic binary swap → `execv` into the new binary. The running process is
replaced in place; no external orchestration needed.

To bump dependencies deliberately (then commit the updated lock):

```bash
dar lock-refresh --dir ./my-agent
```

### Portability

`dar build` runs `cargo build --release` against the host's native
target — the result is a dynamically-linked binary for that platform/arch.
`bin/dar` can be copied to another host only if that host is
ABI-compatible (same OS, arch, and libc). It is not a portable static binary.

A Rust toolchain is not needed merely to *run* `bin/dar`, but is
required to rebuild or self-update (`dar build` /
`dar self rebuild`).

For truly air-gapped hosts, run `init-build --vendor` once on a connected
machine, commit the `vendor/` tree inside `.dar/`, then build offline
with `--vendor --offline`.
