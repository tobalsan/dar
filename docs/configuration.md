# Configuration (agent.yaml)

This covers `agent.yaml` identity/host config, system files, exported child environment variables, and the HITL notifier.

`agent.yaml` is agent **identity and host config only**: `id`, `name`,
`runner`, `hitl`, `dashboard`, `foreground`, `providers`, `extensions`,
`system_files`. The issue-loop config — tracker, polling, workspace — lives
entirely in `WORKFLOW.md` frontmatter (see [WORKFLOW.md](workflows.md) below);
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
  # thinking: high            # reasoning level (alias: effort); see [Thinking / reasoning level](runners.md#thinking--reasoning-level)
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

## System files

`AGENTS.md` in agent folder loads automatically, first. `system_files` adds
agent-folder-relative files in listed order. Entries are optional unless
`required: true`; missing optional files warn and skip. Paths cannot escape
agent folder. Do not list `AGENTS.md` again.

System context resolves once when the host boots; these files are not watched.
A successful live self-rebuild restarts the host and reloads the current
`agent.yaml`, `AGENTS.md`, declared `system_files`, and workspace skills. An
offline `dar self rebuild --dir ...` only swaps the binary, so a running host
keeps its existing context until manually restarted.

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

## HITL notifier

Fires on stall, safety park, and startup errors. Backends:

- `stdout` (default) — log line only.
- `webhook` — HTTP POST to `webhook_url` with a JSON batch.
- `cli` — pipe JSON batch to `command`.
- `none` — silent.

Burst-dedup: notifications are batched per `window_secs`; duplicate events
within the window are collapsed. Max `max_items` unique items per batch.
</content>
</invoke>
