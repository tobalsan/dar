# Trackers

Dar supports three tracker backends for reading issue state: the local issue-files tracker, Linear, and Plane.

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

Two separate switches. agent.yaml `tracker: {use: linear}` links the
`tracker-linear` extension into the binary (build B only — see
[self-contained agents](self-contained-agents.md)). `tracker.kind: linear` in
`WORKFLOW.md` frontmatter enables the issue loop against Linear. Requires:

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

Two separate switches. agent.yaml `tracker: {use: plane}` links the
`tracker-plane` extension into the binary (build B only). `tracker.kind: plane`
in `WORKFLOW.md` frontmatter enables the issue loop against Plane. Scope the
tracker to one Plane workspace + zero or more projects, and provide a Plane
auth token in the environment or `<agent-folder>/.env`:

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

## Tracker tools without the issue loop

Linking a tracker via agent.yaml `tracker.use` is enough on its own — no
`WORKFLOW.md` is needed. A tracker extension registers its host tool
(`linear_graphql` for Linear, `plane_api` for Plane) unconditionally at boot,
independent of whether the orchestrator's issue loop is active, so a passive
agent (no `WORKFLOW.md`, or one without a valid `tracker.kind`/states) still
gets the tool in its agent toolset. Credentials stay host-side: the tool reads
`LINEAR_API_KEY` / `PLANE_API_KEY` (or the other supported token env vars) via
the host's env provider at call time, values are redacted from logs, and
`.env`-loaded keys are never exported into the child process environment.
