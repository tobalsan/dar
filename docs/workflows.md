# WORKFLOW.md

`WORKFLOW.md` configures the issue-loop: tracker, polling, workspace, and the per-issue prompt template.

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

## Template variables

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

Fields omitted from frontmatter fall back to: `interval_ms 1000`,
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

## Running a non-default workflow (`--workflow`)

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
without clobbering each other. Passive agents publish no workflow path; a
workflow's own dashboard header shows `id · folder · workflow`.
