# Agentropy v0

A self-contained, folder-scoped agent runtime. One Rust binary, run from inside
an agent folder. It polls an issue tracker, dispatches a Claude Code subagent
into a per-issue workspace, watches it finish, and moves on — with a small local
dashboard to observe and stop the world.

> v0 is a prototype: macOS arm64, files-only tracker, Claude Code runner, no
> auth, no persistence beyond the log file.

## How it works

```
                 ┌─────────────── agentropy run ───────────────┐
   issues/*.md   │  poll → reconcile → sort → dispatch → watch  │   dashboard
  (the truth) ──▶│            (in-memory run state only)        │──▶ :7878 (HTMX)
                 └──────────────────────┬──────────────────────┘
                                        ▼
                        claude -p  (cwd = workspaces/ISSUE-N/)
                        edits its own issue file → state: done
```

Two state layers, kept strictly separate:

- **Issue state** — the `state:` field in each issue file. Owned by the tracker.
  Changed only by the agent (it edits the file) or a human. **The orchestrator
  never writes it.**
- **Run state** — the orchestrator's in-memory view of one dispatch attempt
  (`Running`, `RetryQueued`, `Cancelled`, `Failed`, `Succeeded`). Lives in
  process memory + the log only.

Every tick (`poll_interval_ms`):

1. **Reconcile** running children against their issue files. If an issue went
   terminal, missing, or to a non-active state → terminate the child
   (SIGTERM → grace → SIGKILL), run state `Cancelled`.
2. **Read** candidate issues whose state is in `active_states`, **sort** by
   priority asc (null last) → `created_at` asc → identifier, and **dispatch** up
   to `max_concurrent` into per-issue workspaces.
3. **On exit 0** → re-read the issue: terminal = `Succeeded`; still active = a 1s
   continuation retry; otherwise release.
4. **On abnormal exit** (non-zero / signal / timeout) → retry with backoff
   `min(retry_backoff_ms · 2^(n-1), 5min)` up to `max_retries`, then `Failed`
   (log-only — issue state untouched).

State is in-memory only. Kill `agentropy` and rerun: it starts cold and trusts
whatever the issue files say.

## Agent folder layout

```
my-agent/
├── agent.yaml          # config
├── WORKFLOW.md         # prompt template (minijinja, strict)
├── issues/             # one Markdown file per issue (filename = identifier)
│   ├── ISSUE-1.md
│   ├── ISSUE-2.md
│   └── ISSUE-3.md
├── workspaces/         # created at runtime, one dir per issue
└── logs/agent.log
```

Move the folder, move the agent. Everything it needs lives inside.

## Build

```bash
cargo build --release          # → ./target/release/agentropy
```

Requires the **Claude Code CLI** (`claude`) installed and authenticated on the
host. Agentropy does not bundle, install, or auth it.

## CLI

```bash
# Scaffold the default WORKFLOW.md prompt in an agent folder.
agentropy init-workflow --dir ./my-agent
agentropy init-workflow --dir ./my-agent --force   # overwrite existing

# Validate a folder's agent.yaml + WORKFLOW.md + tracker. Exit code only.
agentropy doctor --dir ./my-agent

# Run the loop (long-running). Defaults to the current directory.
cd my-agent && agentropy run
agentropy run --dir ./my-agent          # or point at a folder

# Quick demo with the bundled example agent:
agentropy doctor --dir ./example-agent
agentropy run   --dir ./example-agent
# then open the dashboard:
open http://127.0.0.1:7878/
```

`run` keeps going until you Ctrl-C it (children are killed on shutdown).

## Configuration (`agent.yaml`)

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
  max_run_timeout_ms: 1800000        # 30 min hard cap per attempt

orchestrator:
  poll_interval_ms: 10000
  max_concurrent: 1                  # v0: one issue at a time
  max_retries: 3
  retry_backoff_ms: 10000

workspace:
  root: ./workspaces

dashboard:
  bind: 127.0.0.1
  port: 7878
```

## Issue files

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

## Workflow prompt (`WORKFLOW.md`)

Markdown body rendered per issue with `minijinja` (strict — an unknown variable
fails the attempt) against `{{ issue.* }}`. It must tell the agent to **transition
the issue itself** when done — the orchestrator won't.

Use `agentropy init-workflow` to scaffold the canonical default body, which
encodes the full worker procedure: claim the Linear issue, move to In Progress,
keep one comment updated, work in the issue workspace, use a git worktree for
code changes, spawn a reviewer subagent, commit only after a clean review, create
or update a PR with `gh`, link it to Linear, move to In Review, and move to
Needs Human when blocked.

Minimal example:

```markdown
You are working on issue {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}

When the task is complete, edit `../../issues/{{ issue.identifier }}.md` so its
frontmatter `state:` becomes `done`. Then stop.
```

The runner spawns `claude -p --permission-mode bypassPermissions --add-dir <agent-folder>`
with the workspace as cwd and the rendered prompt piped to stdin. The
`bypassPermissions` + `--add-dir` flags let the headless agent edit its issue
file (which lives outside the workspace cwd) without a human approving prompts.

## Dashboard

`http://127.0.0.1:<port>/` — single page, self-polls every 2s. No auth.

- **Agent**: id, folder, polling state, tracker, runner.
- **Active run**: issue, state, workspace, PID, started, elapsed, last event.
- **Queue** / **Retry queue** / **Recent events** (last 50 lines).
- **Controls** (mutate run state only, never issue state):
  - **Stop** — kill the active child (SIGTERM → 5s grace → SIGKILL), run state
    `Cancelled`. The issue file is untouched, so the next tick may re-dispatch it.
  - **Pause** — stop picking up new issues; the current run keeps going.
  - **Resume** — resume polling.

Same actions over HTTP:

```bash
curl -X POST http://127.0.0.1:7878/control/stop
curl -X POST http://127.0.0.1:7878/control/pause
curl -X POST http://127.0.0.1:7878/control/resume
```

## Logging

Single file at `logs/agent.log` (also streamed to terminal stderr):

```
time level issue=ID event=... msg=...
child[ISSUE-1]: <line of the child's stdout/stderr>
```

Finished runs are also appended to `logs/history.jsonl` (one JSON object per
line). The dashboard's **Run history** panel loads the last 50 on startup, so it
survives a restart — unlike the rest of the in-memory run state.

## Not in v0

Channels, chat UI, WASM/subprocess extensions, scheduler, hot reload, hub
registration, multi-platform builds, dashboard auth, persistence beyond the log,
and any tracker other than files. The `Tracker` trait is read-only by design —
write verbs arrive later as agent-facing tools, not orchestrator calls.
```
