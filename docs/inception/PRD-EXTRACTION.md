---
title: "Agentropy — Orchestrator Extraction PRD"
status: draft
scope: extraction / feature-parity
source: AIHub orchestrator extension
target: self-contained Rust runner (this repo)
platform: macOS arm64 (initial)
---

# Orchestrator Extraction PRD

## Purpose

The orchestrator currently lives as an extension inside AIHub, where it is too
centralized and carries too many responsibilities tied to the host (gateway
lifecycle, hub config, event bus, web capability gating, notification channels).

Following Symphony's philosophy, an orchestrator is better as its own
self-contained process. **This repo is that process.** Today it implements a v0
slice (file tracker, single Claude runner, minimal dashboard — see `PRD.md`).
This PRD enumerates **everything that must be imported from AIHub's orchestrator
extension to reach 100% functional parity** as a standalone Rust runner.

This document is **feature- and capability-focused**. It stays high-level on
architecture and intentionally does not prescribe module layout. Each section
states *what the standalone runner must do*, marks the **gap vs. today's v0**,
and flags anything that was AIHub-platform-coupled and must be re-homed or dropped.

### TOP PRIORITY: tiny footprint (the defining feature)

**Small size and low resource usage are THE core product feature — not a nice-to-
have.** The whole reason to extract the orchestrator from AIHub is to get a lean,
self-contained runner. Every design and implementation decision must defend the
footprint; feature parity must never come at the cost of bloating the runtime.

**Hard requirement (acceptance gate):** a single average retail computer with
**8 GB RAM** must comfortably run **10+ Agentropy instances at once** (one per agent
folder/project), all idle-polling and able to dispatch, without exhausting memory or
saturating the CPU. The runner's own overhead — excluding the AI child processes it
spawns — must be small enough that 10+ copies leave ample headroom for those
children and the OS.

Concretely, the **idle/steady-state resident footprint of one Agentropy process**
(the orchestrator + dashboard + SQLite, with no active child) must be a small
fraction of a single instance's fair share of 8 GB across 10+ instances — target
tens of MB, not hundreds. Implications the implementer must honor:

- **Idle should be nearly free.** Between ticks the process should be effectively
  asleep — no busy-wait, no constant CPU. Polling and timers must be cheap; CPU near
  0% when idle.
- **Bounded memory.** No unbounded in-memory growth: cap the recent-events ring,
  stream logs to disk/SQLite rather than holding them, and keep run history in
  SQLite (paged queries) instead of memory. Memory must not grow with run count or
  uptime.
- **Lean binary & deps.** Prefer a small static binary and a minimal dependency
  tree. Avoid heavy runtimes, embedded browsers, or large frameworks in the runner
  itself. The dashboard must stay lightweight (server-rendered + light JS), not a
  bundled SPA.
- **Modest async runtime.** Size the tokio/threadpool conservatively; a single
  single-project runner does not need many worker threads.
- **Cost scales with work, not idle.** Resource use should track active children and
  events, not wall-clock time or number of historical runs.

This requirement applies to **every** section below: when a capability can be built
two ways, choose the one with the smaller steady-state footprint. See §16 (parity
checklist item 14) and §17 for the explicit footprint acceptance criteria.

### Scope decision: single-project

**This runtime is single-project by design.** AIHub needed multi-project support
because it is one centralized platform serving many projects. This runtime is
small and self-contained: it launches *inside one agent folder* that contains a
`WORKFLOW.md`, and **that folder is the only project**. We do **not** import
AIHub's multi-project machinery:

- No `projects[]` list, no `projectsRoot`, no project registry of N folders.
- No per-project timers — one polling timer for the single project.
- No project filter / project count in the dashboard, no project selection.
- Concurrency is a single cap (the project's `agent.max_concurrent`); there is no
  separate "global vs per-project" distinction.
- Duplicate-slug rejection across projects is moot (only one project exists).

Wherever AIHub iterates "for each project", this runtime operates on the single
launch folder. The rest of this PRD is written with that simplification applied;
sections below note where AIHub's multi-project behavior collapses to single-project.

### Scope decision: agent definition vs. orchestrator workflow

The launch folder is an **agent folder**, not merely an orchestrator project. The
orchestrator loop is **one feature** of that agent; future iterations add more
(e.g. chat). Two config files therefore have distinct, complementary roles and
**both stay**:

- **`agent.yaml` — the agent definition (identity & underlying coding agent).** The
  durable description of *who/what the agent is*: `id`, `name`, `description`, the
  underlying coding-agent SDK/runner (`sdk:` — e.g. `pi`, `claude`, `codex`,
  `opencode`), `model` (`provider` + `model`), sandbox, extensions, system files,
  etc. This is shared across **all** agent features, not just the orchestrator loop.
  (Reference shape: `config/agents/<name>/agent.yaml`, e.g. the `sally` agent.)
- **`WORKFLOW.md` — the orchestrator-loop config + prompt.** YAML frontmatter plus a
  Markdown prompt body that configures *only* the orchestrator loop feature
  (tracker, polling, workspace, hooks, concurrency, server, etc.) and the worker
  prompt template. It does **not** redefine the agent.

**Runner/model resolution (load-bearing):** for the orchestrator loop, the effective
runner/SDK/provider/model is resolved as an **override-with-fallback**:

- If `WORKFLOW.md` frontmatter specifies a runner (`agent.runner`/`agent.kind` and/or
  model/provider/thinking), **those override** the agent definition — for the
  orchestrator loop only.
- Otherwise, the orchestrator loop **falls back to `agent.yaml`** (`sdk` → runner,
  `model.provider`/`model.model`).

This keeps the agent's identity in one place (`agent.yaml`) while letting the
orchestrator feature pin a different runner/model when desired. `WORKFLOW.md` does
not supersede `agent.yaml`; it layers on top of it for one feature.

### Scope boundary: import vs. re-home vs. drop

AIHub's orchestrator splits into two halves:

- **Self-contained orchestrator logic** — import nearly wholesale (re-expressed in
  Rust), scoped to the single project: `WORKFLOW.md` loader/parser/templater,
  Linear GraphQL polling + safety mutations, claiming/concurrency/tick loop,
  run/event persistence, workspace creation/cleanup, hook execution, runner
  abstraction + protocol runners, retry/stall/timeout/failure semantics, webhook
  relevance/signature handling.
- **AIHub-platform surfaces** — replace with Rust-native equivalents owned by this
  process: configuration loading, HTTP API, event/live updates, dashboard,
  notifications. **Drop** outright: extension lifecycle (`Extension.start/stop/
  registerRoutes`), gateway config coupling, web-capability gating, hub
  registration, and `extensions.subagents.profiles` resolution (legacy fallback —
  see §13).

---

## 1. Core orchestration loop

The orchestrator **observes** issue state and **controls** child process
lifetime. The worker (agent) owns normal progress writes; the orchestrator owns
only safety/parking writes (§3).

**Must import (single-project):**

- **One polling timer** for the project. Delay = `polling.interval_ms ± jitter_ms`.
  (AIHub's per-project timers collapse to a single timer.)
- **Manual / event ticks** in addition to the timer:
  - `POST /tick` (manual)
  - Linear webhook enqueue (§10)
  - direct claim endpoint
  - A queued tick re-polls the single project; it is not issue-targeted.
- **Tick flow (load-bearing order):**
  1. Set heartbeat + `lastTickAt`.
  2. Detect stalled runs (§6).
  3. Observe worker completions.
  4. For the project: load `WORKFLOW.md` → poll Linear (active + terminal +
     needs-human states) → release already-claimed terminal/needs-human issues →
     skip blocked issues → apply retry backoff → respect concurrency → dispatch
     eligible issues.
- **Claiming.** In-memory claim registry keyed by `issueId`, mirrored by a persisted
  claim row. Prevents duplicate workers for the same issue.
- **Concurrency.** A single cap: `WORKFLOW.md agent.max_concurrent` (default 3).
  (No separate global cap — single project means one limit.) Same issue never
  reserved twice.
- **Dispatch ordering.** Today AIHub dispatches in the order Linear's GraphQL
  returns (no explicit priority sort, despite priority being mapped). Parity =
  preserve this. *(Note: this repo's v0 file tracker sorts priority asc → created_at
  → identifier. Keep v0 sort for the file tracker; match Linear's native order for
  the Linear tracker. Reconcile in design.)*

**Run lifecycle / outcomes (full set — import all):**

Worker protocol statuses: `running`, `done`, `error`, `interrupted`.

Persisted run outcomes:

| Outcome | Meaning |
|---|---|
| `completed` | worker returned `done` |
| `error` | worker errored; issue parked to needs-human first |
| `interrupted` | worker interrupted |
| `stalled` | no events within stall timeout |
| `terminal` | Linear issue moved to terminal state while claimed |
| `needs_human` | Linear issue moved to needs-human while claimed |
| `hook_failed` | hook returned non-zero before run |
| `dispatch_failed` | failed to start/dispatch worker |
| `released` | manual release API |
| `orphaned` | store-supported; not central in current daemon path |
| `interrupted_gateway_restart` | open runs marked this on (re)start |
| `park_barrier` | synthetic marker after max active reruns (§7) |
| `killed` | full kill teardown via the Kill control (§8.5) |

UI "Completed / Failed / Interrupted / Stalled" are **display buckets**, not the
internal set (§8.4 for mapping).

**Gap vs v0:** v0 has only `Running/RetryQueued/Cancelled/Failed/Succeeded`
in-memory. Parity requires the full outcome set, the persisted claim registry, and
the `agent.max_concurrent` concurrency cap. (Both v0 and this target are
single-project — no per-project loops needed.)

**Re-home / drop:** the extension's gateway-driven start/stop becomes this
process's own `run` entrypoint.

---

## 2. Tracker / issue source

**Must import:**

- **Linear tracker** (`tracker.kind: linear`). GraphQL endpoint configurable;
  default `https://api.linear.app/graphql`.
- **Polling** filters by Linear project `slugId` + configured state names. Pulls per
  issue: `id`, `identifier` (e.g. `ALG-123`), `title`, `description`, `url`,
  `state`, `labels`, project name/slug, `parent id`, blocking relations.
- **Project scoping (single-project).** The launch folder's `WORKFLOW.md`
  `tracker.project_slug` maps this one local project to one Linear project. (No
  folder list, no `projectsRoot`, no cross-project duplicate-slug check — only one
  project exists.)
- **Project validation** (at load): the launch folder must contain a `WORKFLOW.md`;
  honor strict-vs-lenient validation (`validation.strict`, §11) — strict fails fast
  on an invalid `WORKFLOW.md`, lenient logs and (where possible) falls back to a
  cached snapshot (§4).
- **Blocking relations.** Blocked issues are skipped at dispatch.
- **Rate-limit handling.** Read Linear headers
  (`x-ratelimit-requests-remaining`, `x-ratelimit-complexity-remaining`,
  `x-ratelimit-remaining`, + reset headers). If remaining ≤ 0, wait until reset
  + 1s. On HTTP 429, sleep once then retry once. Track minimum remaining for the
  dashboard "RATE LIMIT" stat.

**Tracker write surface (orchestrator-owned, limited):** see §3. Orchestrator
writes Linear only for safety/parking. The worker owns normal workflow writes.

**Keep the existing v0 abstraction:** the `Tracker` trait stays read-oriented for
candidate discovery; add the small safety-write surface the Linear tracker needs
(transition-to-needs-human + comment). The file tracker remains for the local
demo / tests; `LinearTracker` is the parity target.

**Gap vs v0:** v0 ships only `FileTracker` with no network, no rate limiting, no
project-slug mapping, no blocking relations.

---

## 3. Tracker writes (orchestrator-owned safety/parking)

The orchestrator writes Linear **only** in these cases — everything else is the
worker's job:

- Move issue to **needs-human** when: worker errors; run stalls; profile missing /
  parked; or too many consecutive completed runs without the issue leaving active
  state (§7).
- Add **comments** for park / stall messages.

The default worker prompt (§4) instructs the agent to update Linear state/comments
itself for normal progress. This split (orchestrator = safety writes, worker =
progress writes) must be preserved.

---

## 4. Prompt / workflow system

**Must import:**

- **The launch folder's `WORKFLOW.md`** with YAML frontmatter (config) + Markdown
  body (prompt template). This single file is the project. Watched for changes
  (emit workflow-changed events to the UI).
- **Templating variables:** `{{ issue.<field> }}` and `{{ attempt }}`. After
  rendering, the daemon appends orchestrator context: project id, Linear tool
  project hint, and Linear issue identifier/title/description/url.
- **Frontmatter config sections:** `tracker`, `polling`, `workspace`, `agent`,
  `hooks`, `server`, `linear`. (`digest` exists in the type but is not central —
  may omit.)
- **State-name configuration** (full list — import all):
  - `tracker.active_states` (default `[Todo, In Progress]`) — eligible for dispatch.
  - `tracker.terminal_states` (default `[Closed, Cancelled, Canceled, Duplicate,
    Done]`) — claimed issue entering one → release as `terminal`.
  - `tracker.needs_human` (default `Needs Human`) — claimed issue entering it →
    release as `needs_human`; orchestrator also moves issues here on
    stall/error/park.
  - Legacy nested form `tracker.states.{active,terminal,needs_human,
    in_progress_target}` is also accepted. `in_progress_target` is parsed but
    effectively unused by the daemon. There is **no** daemon config for
    `in_review` — "In Review" exists only in prompt text and is a worker-driven
    Linear transition.

**Typical default prompt capabilities (prompt-level, NOT daemon-enforced):** fetch
the Linear issue first; move Todo → In Progress; comment progress; work only in the
issue workspace; use a worktree for code changes; spawn a reviewer subagent; commit
only after a clean review; create a PR with `gh`; link the PR to Linear; move to In
Review; move to Needs Human when blocked. These are requirements expressed in the
prompt body, not orchestrator logic — they ship as the default `WORKFLOW.md` body.

**Workflow watching / cache:** watch the `WORKFLOW.md`; cache the last good
snapshot; on a load/parse error, fall back to the cached snapshot when `allowStale`
is set (otherwise surface the error). Emit workflow-changed events to the UI.

**Gap vs v0:** v0 uses minijinja strict rendering of `{{ issue.* }}` only, no
`{{ attempt }}`, no appended orchestrator/Linear context, no state-name config
beyond `active_states`/`terminal_states`, no needs-human, no workflow
watching/cache.

---

## 5. Workspace handling

**Must import:**

- **Workspace root** from `WORKFLOW.md workspace.root`; relative paths resolve from
  the project folder; supports `~` and `$AIHUB_HOME` (rename/keep an equivalent
  env var for the standalone runner).
- **Per-issue workspace** = `<workspace.root>/<sanitized issue identifier>`
  (e.g. `workspaces/ALG-123`). Sanitization prevents unsafe paths; containment
  under root is enforced. *(v0 already implements sanitize + containment in
  `src/paths.rs` — reuse.)*
- **Reuse** (`workspace.reuse`, default `true`).
- **Cleanup** (`cleanup_on_terminal`, default `false`). When enabled, cleanup
  occurs only on outcomes `terminal`, `hook_failed`, `dispatch_failed`. Preserved
  for `completed`, `error`, `interrupted`, `stalled`, `needs_human`.
- **Hooks** (lifecycle shell hooks): `after_create`, `before_run`, `after_run`,
  `before_remove`. Hook env: `AIHUB_PROJECT_ID`, `AIHUB_ISSUE_ID`,
  `AIHUB_ISSUE_IDENTIFIER`, `AIHUB_WORKSPACE`; `LINEAR_API_KEY` **intentionally
  unset** in hook env. Non-zero `before_run` → outcome `hook_failed`.

**Git/repo note:** the core orchestrator only creates directories. It does **not**
clone repos or create worktrees itself — worktree/clone/PR behavior is delegated to
hooks or the worker prompt. Parity = keep this delegation; do not build git into the
orchestrator.

**Gap vs v0:** v0 creates/reuses workspaces but has no cleanup policy and **no
hooks**.

---

## 6. Runner / child process model

**Must import (runner abstraction + protocol runners):**

Supported runners: `pi`, `claude`, `codex`, `cli`, `fake`. **Effective-runner
resolution (override-with-fallback):** use `WORKFLOW.md agent.runner` / `agent.kind`
(+ model/provider/thinking) when present — these override for the orchestrator loop;
otherwise fall back to the agent definition in `agent.yaml` (`sdk` → runner,
`model.provider`/`model.model`). Default = `pi` only when neither source specifies a
runner. (See "Scope decision: agent definition vs. orchestrator workflow".)

- **pi** — JSON-RPC over child stdin/stdout. Session dir under
  `.aihub/pi-sessions`. Supports `--provider`, `--model`, `--thinking`, turn
  timeout, request timeout, abort RPC with SIGTERM fallback.
- **claude** — Claude Code RPC/shim. Default unattended:
  `--permission-mode bypassPermissions`. Supports `--model`, `--effort`, session
  persistence under `.aihub/claude-sessions`, abort. *(v0's single runner is a
  subset of this — see `src/runner.rs`; note v0's deliberate `--add-dir` /
  not-`--dangerously-skip-permissions` rationale in AGENTS.md must be preserved.)*
- **codex** — `codex app-server` with cwd. Defaults `approvalPolicy: never`,
  full-access / danger-full-access sandbox. Supports model + reasoning effort.
- **cli** — spawns configured `agent.command`, cwd = workspace. Env:
  `AIHUB_RUN_ID`, `AIHUB_PROJECT_ID`, `AIHUB_ISSUE_ID`, `AIHUB_ISSUE_IDENTIFIER`,
  `AIHUB_WORKER_PROMPT`, `AIHUB_WORKER_MODEL`. Currently `stdio: ignore`; protocol
  runners emit richer events.
- **fake** — test runner.

**Event capture.** All runner/protocol events persist to the `events` store. The API
normalizes them into UI log rows: `assistant`, `thinking`, `user`, `tool_call`,
`tool_output`, `error`. ANSI is stripped.

**Per-attempt timeout.** Enforce `agent.turn_timeout_ms` (default 1h) — runner
interrupts/aborts the current turn; surfaces as `interrupted` (reason
`turn_timeout`). *(v0 has `max_run_timeout_ms` hard-kill; reconcile naming.)*

**Gap vs v0:** v0 has exactly one runner (Claude `-p`, stdin pipe, line-streamed to
`agent.log` + ring). Parity requires the multi-runner abstraction with at least pi +
claude + codex + cli, structured event capture, and session persistence dirs.

---

## 7. Retry / failure semantics

**Must import:**

- **Dispatch retry** (kind `dispatch`): backoff 30s base, exponential `2^attempt`,
  capped at 30m. Reset on successful dispatch. *(v0 uses base 10s, cap 5m,
  `2^(n-1)`, `max_retries` — reconcile to AIHub's 30s/30m for parity.)*
- **Stall detection** (`agent.stall_timeout_ms`, default 5m): claim `lastEventAt`
  updated on worker events; on timeout → comment on Linear, move to needs-human,
  abort worker, mark run `stalled`, notify HITL.
- **Turn timeout** (`agent.turn_timeout_ms`, default 1h): runner aborts current
  turn → worker `interrupted` (reason `turn_timeout`).
- **Worker error**: park issue (Linear comment + move to needs-human + notify
  HITL), then release with outcome `error`.
- **Gateway/process restart**: on startup, SIGTERM stale persisted PIDs that belong
  to the old process **before** marking their open runs
  `interrupted_gateway_restart`; **no** live continuation/resume of in-flight
  workers.
- **Max active reruns** (`agent.max_active_runs`, default 3) → `park_barrier`:
  - Counter = consecutive prior runs for the same `issue_id` whose outcome is
    exactly `completed`, counted since the latest non-`completed` finished outcome.
  - **Increments** when a worker finishes `done` → released `completed` → issue
    still active → eligible next tick.
  - **Resets** on any non-`completed` finished outcome (`error`, `stalled`,
    `needs_human`, `terminal`, `interrupted`, `dispatch_failed`, …).
  - **Park condition** (checked before dispatch): `consecutiveCompleted >=
    max_active_runs` → comment on Linear, move to `needs_human`, emit HITL
    notification, insert synthetic `park_barrier` run row, skip dispatch.

**Gap vs v0:** v0 has dispatch backoff + a 1s continuation retry but **no** stall
detection, no turn/error parking, no needs-human, no restart-marking, and no
`max_active_runs`/`park_barrier`.

---

## 8. Dashboard / UI

Parity target (the screenshots): a single live dashboard owned by this process.

**8.1 Header stats:** Active count, Recent count, Last tick, Rate limit (tracked
minimum remaining), Refresh. (Drop AIHub's Projects count + project selector —
single project. The agent id/name + folder path may be shown instead.)

**8.2 Lists:** Active runs ("Idle, nothing running." empty state), Recent runs,
live updates via polling + WebSocket. (No project filter — single project.)

**8.3 Run detail drawer:** summary metadata + three tabs:

- **Logs** — normalized worker transcript (cursor/`since` fetching by event id).
- **Events** — raw orchestrator event rows.
- **Workflow** — resolved workflow snapshot for the project: `path`, `projectPath`,
  `sha`, `frontmatter`, merged/resolved `config`, `body`. **Redacted:**
  `frontmatter.tracker.api_key` and `config.tracker.apiKey` → `[redacted]`. Note:
  this is the *current project workflow*, not the run's exact historical rendered
  prompt; the real worker prompt is built at dispatch from rendered body + appended
  context.

**8.4 Recent-runs buckets** (computed client-side from `run.outcome`; `runs` table
ordered by `started_at DESC`):

- **Completed** — outcome contains `complete` / `success` / `done` / `finish`.
- **Failed** — contains `fail` / `error` (e.g. `error`, `hook_failed`,
  `dispatch_failed`).
- **Interrupted** (warning bucket) — contains `interrupt` / `cancel` / `orphan` /
  `needs_human` / `stall` / `human` (e.g. `interrupted`,
  `interrupted_gateway_restart`, `stalled`, `needs_human`, `orphaned`).
- **Other** — anything else (e.g. `terminal`, `released`, `killed`). `park_barrier`
  is filtered out of the recent query.
- **Live** — no `finished_at` / no `outcome`; counted as active, not a recent
  filter.

**8.5 Controls:** Refresh; **Interrupt run** (abort the worker, leave the claim/
workspace); **Kill run** — full teardown sequence: interrupt worker → run
`before_remove` hook → remove workspace → release claim → mark outcome `killed`.
Manual claim/tick endpoints exist in the API. *(v0 has Stop/Pause/Resume; align
toward Interrupt/Kill + keep Pause/Resume if desired — design decision.)*

**8.6 Pagination:** Recent runs API takes `limit` (default 50). No full cursor
pagination for runs. Logs/events support a `since` cursor by event id.

**Gap vs v0:** v0's askama+HTMX dashboard is self-polling only (no WebSocket), no
run-detail drawer, no tabs, no buckets. Parity requires the dashboard with the
run-detail drawer and live
updates.

**Re-home:** the AIHub web-route + capability gating is dropped; this process serves
its own dashboard directly (keep v0's bind/port model, drop auth as today).

---

## 9. HTTP API

**Must import (re-homed to this process, no AIHub mount):** health; read workflow
(resolved + redacted); list runs (`limit`); run detail; logs (`since`); release;
interrupt; kill; claim issue; tick; webhook. (Drop the multi-project "list
projects" route — single project; a single `GET /project`/workflow read suffices.)

**Linear export** (import): an export route/CLI (`/export`) that dumps the Linear
project + issues to local files under the data dir.

**Re-home / drop:** AIHub's Hono mount under `/api/orchestrator`, event-bus
broadcasts, and web-capability gating become this process's own HTTP server +
WebSocket. Keep route *semantics*; drop the host coupling.

---

## 10. Linear webhook

**Must import:** `POST /webhook` with HMAC-SHA256 verification (secret from config).
Relevant events **enqueue a tick** that re-polls the single project (not
issue-targeted).

**Relevance filter:** payload must carry an issue-ish field (`data.issue`,
`data.issueId`, `data.identifier`, `data.id`) **and** be either an issue
update/state event (`type`/`action` contains `issue` and includes `update`/`state`,
or payload has `state`) **or** a comment event (`type`/`action` contains `comment`,
or payload has `comment`).

**Gap vs v0:** no webhook at all in v0.

---

## 11. Configuration

**Two config files, distinct roles (both kept).** The launch folder is an *agent
folder*. `agent.yaml` is the **agent definition** (identity + underlying coding
agent: `sdk`/runner, `model.provider`/`model`, sandbox, extensions, system files) and
is shared across all agent features. `WORKFLOW.md` frontmatter is the **orchestrator-
loop config** (tracker/polling/workspace/agent-loop/hooks/server/linear) plus the
worker prompt. For the orchestrator loop, `WORKFLOW.md` runner/model fields
**override** `agent.yaml`; when absent, the loop falls back to `agent.yaml`'s
`sdk`/`model`. `WORKFLOW.md` does **not** replace `agent.yaml`. (See "Scope
decision: agent definition vs. orchestrator workflow".)

**Single-project model:** the launch folder *is* the project. AIHub's gateway-level
`extensions.orchestrator.*` knobs that only existed for multi-project hosting are
**dropped**: `projects` (folder list), `projectsRoot`, `concurrency.global`. The few
remaining process-level knobs (`validation.strict`, notifier target,
`linear.exposeGraphqlTool`, `webhook.{enabled,path,secret}`) fold into either CLI
flags or the `WORKFLOW.md` frontmatter (`server`/`linear` sections) — there is no
separate gateway config file.

**Orchestrator-loop config (`WORKFLOW.md` frontmatter — the source of truth for the
loop, import as-is):** tracker config; polling interval/jitter; workspace
root/reuse/cleanup; runner kind/command/model/provider/thinking/settings (override
over `agent.yaml`); concurrency
(`agent.max_concurrent`, the only concurrency cap); max turns; turn timeout; stall
timeout; max active runs; hooks; Linear options; server bind/port.

**Credentials:** Linear API key from workflow frontmatter (commonly
`$LINEAR_API_KEY`); webhook secret from config/frontmatter. GitHub creds are **not**
orchestrator-managed — PR creation is prompt-level via `gh`.

**Gap vs v0:** v0's `agent.yaml` is a single flat config that today carries both
identity and loop settings. Parity **splits responsibilities**: `agent.yaml` stays
as the agent definition (identity + underlying coding agent / `sdk` / `model`), and
the orchestrator-loop settings move into the richer `WORKFLOW.md` frontmatter schema
above. The loop reads `WORKFLOW.md` (overriding `agent.yaml` runner/model when set,
else falling back to it). `agent.yaml` is **not** removed.

---

## 12. Persistence

**Must import:** a local SQLite store (under this process's data dir) with tables
`runs`, `events`, `claims`, `heartbeats`.

Persisted fields: run id; issue id/identifier; workspace; profile JSON; workflow
path + sha; pid; worker id; start/finish times; outcome; exit code; `process_alive`;
raw event payloads; claim/release times; heartbeat timestamp. (AIHub's `project id`
column is redundant in single-project mode — may keep a constant or drop it.)

In-memory: active worker handles; live claim registry; tick queue; timers; retry
state; HITL burst buffer; workflow watchers.

**Gap vs v0:** v0 persists only finished runs to `logs/history.jsonl` (reloaded on
startup) and a plain `agent.log`. Parity requires SQLite with the four tables and
event payload storage feeding the dashboard's Logs/Events tabs.

---

## 13. Notifications (HITL)

**Must import (re-home the channel):** a human-in-the-loop notification path used for
stalls, startup errors, and parked issues. **Burst buffer / debounce:** window 60s,
max 5 items; flush immediately at 5, otherwise after 60s; de-dupe exact duplicates;
flush pending on stop.

**Re-home / drop:** AIHub's `notify` channel becomes a pluggable notifier for the
standalone process (e.g. webhook/CLI/stdout). Keep the burst-buffer policy exactly.

---

## 14. Other capabilities

- **Manual API surface** (§9): health, read workflow, list runs, run detail, logs,
  release, interrupt, kill, claim issue, tick, webhook.
- **CLI commands:** an `init-workflow` (scaffold a `WORKFLOW.md` for the orchestrator
  loop in an existing agent folder — it does not scaffold `agent.yaml`, which is the
  separate agent definition); optionally bootstrap a Linear project for this folder. *(Map to this
  repo's `agentropy` CLI; v0 has `run` + `doctor`. AIHub's `init-project` — which
  registered a new folder into the multi-project list — is not needed; the launch
  folder is the project.)*
- **Worker Linear tool:** optionally expose `linear_graphql` to workers. Gated by
  `linear.exposeGraphqlTool`. (The project hint is implicit — the single project.)
- **Code review / PR automation:** **prompt-level only** — the default workflow
  instructs reviewer subagents, PR creation, and Linear linking. Not hardcoded
  daemon behavior; ships as default `WORKFLOW.md` content.

---

## 15. Explicit DROP list (AIHub-coupled, do not import)

- Extension lifecycle: `Extension.start/stop/registerRoutes`.
- Gateway config loading + nested `extensions.*` schema.
- Hono API route mount under `/api/orchestrator`.
- AIHub event bus / WebSocket broadcast plumbing (replace with native).
- Web-route capability gating.
- **Multi-project machinery:** `projects[]`, `projectsRoot`, project registry of N
  folders, per-project timers, global-vs-per-project concurrency split, project
  filter/selector/count in the UI, cross-project duplicate-slug rejection, and the
  "list projects" API route. The launch folder is the sole project.
- AIHub notification channel binding (replace with pluggable notifier — §13).
- `extensions.subagents.profiles` resolution (legacy fallback). Make
  `WORKFLOW.md agent.*` canonical. **Lost if dropped:** compatibility with old
  AIHub configs, the "missing profile parks issue" behavior, and profile-sourced
  runner/model/provider defaults. **Not lost:** runner selection, model/provider
  from `WORKFLOW.md`, pi/claude/codex runners, the core loop. Acceptable to drop.
- AIHub shared schemas/types and the existing web dashboard implementation (rebuild
  natively).

---

## 16. Parity checklist (acceptance)

Standalone parity = all of:

0. **Single-project**: launch inside one folder containing `WORKFLOW.md`; that
   folder is the only project. No multi-project machinery (§15).
1. Single-project polling loop with one timer, one concurrency cap
   (`agent.max_concurrent`), claim registry, and the load-bearing tick order (§1).
2. Linear tracker: GraphQL polling with project-slug scoping, blocking-relation
   skip, full issue field set, and rate-limit handling (§2); orchestrator-owned
   safety/parking writes only (§3).
3. `WORKFLOW.md` loader/parser/templater with `{{ issue.* }}` + `{{ attempt }}`,
   appended orchestrator/Linear context, full state-name config incl. needs-human,
   and change-watching (§4).
4. Workspace root resolution, per-issue sanitized+contained dirs, reuse, cleanup
   policy, and all four lifecycle hooks with the specified env (§5).
5. Runner abstraction with pi + claude + codex + cli (+ fake), structured event
   capture (assistant/thinking/user/tool_call/tool_output/error, ANSI-stripped),
   session dirs, and per-turn timeout (§6).
6. Full retry/failure semantics: dispatch backoff (30s→30m), stall detection, turn
   timeout, worker-error parking, restart-marking, and `max_active_runs` →
   `park_barrier` (§7).
7. Dashboard with header stats, active/recent lists (no project filter), run-detail
   drawer (Logs/Events/Workflow, redacted), the bucket mapping, controls
   (interrupt/kill), and pagination — live via polling + WebSocket (§8).
8. Re-homed HTTP API with all listed routes (§9) + Linear webhook with HMAC +
   relevance filter (§10).
9. `WORKFLOW.md` frontmatter as the orchestrator-loop config source, layered over
   `agent.yaml` (the agent definition) with override-with-fallback runner/model
   resolution; Linear API key + webhook secret handling (§11).
10. SQLite persistence: `runs`, `events`, `claims`, `heartbeats` with the full
    field set (§12).
11. HITL notifier with the 60s / 5-item burst buffer + de-dupe (§13).
12. Init CLIs, optional worker Linear tool, and the default code-review/PR workflow
    shipped as prompt content (§14).
13. Project validation (§2), workflow watching/cache with `allowStale` (§4), startup
    stale-PID cleanup before restart-marking (§7), full Kill teardown sequence
    (§8.5), and the Linear export route/CLI (§9).
14. **Footprint gate (§Top Priority):** 10+ instances run comfortably on one 8 GB
    retail machine; one idle process sits at ~tens of MB resident with ~0% CPU
    between ticks; memory does not grow with run count or uptime. **Treat this as a
    blocking acceptance criterion equal in weight to functional parity.**
15. None of the AIHub-coupled surfaces in §15 imported.

---

## 17. Notes / reconciliations for implementation

- **Backoff/timeout constants** differ between v0 and AIHub (v0: 10s base / 5m cap /
  `2^(n-1)`; AIHub: 30s base / 30m cap / `2^attempt`). Parity defaults to AIHub's
  values; v0 file-tracker tests will need updating.
- **Naming:** v0 `max_run_timeout_ms` vs AIHub `turn_timeout_ms` / `stall_timeout_ms`
  — adopt AIHub's split (turn vs stall) for parity.
- **Dispatch ordering:** keep v0's priority sort for the file tracker, but the Linear
  tracker matches Linear's native GraphQL return order (no extra sort).
- **Env var prefix:** AIHub uses `AIHUB_*` for hook/CLI runner env and
  `$AIHUB_HOME` for workspace roots. Decide whether to keep `AIHUB_*` (drop-in
  compatibility for existing hooks/workflows) or rename to an agentropy prefix
  (cleaner, but breaks existing `WORKFLOW.md` hook scripts). **Recommendation:**
  keep `AIHUB_*` for now to preserve 100% functional parity of existing workflows.
- **Profiles dropped (§13/§15):** make `WORKFLOW.md agent.*` the single source of
  runner/model config.
- **Footprint verification (blocking):** add a measurable check before calling the
  work done — e.g. launch 10+ instances against test folders on an 8 GB-class
  machine and confirm total runner overhead (excluding AI children) leaves ample
  headroom, idle RSS per process stays in the tens of MB, and idle CPU is
  negligible. Capture the numbers. When a feature can be implemented two ways,
  the smaller-footprint option wins by default; any choice that materially grows
  steady-state memory/CPU must be justified against this gate.
