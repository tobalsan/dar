# Runners

The runner backend spawns the child process that does the actual work for an issue; `runner.use` in `agent.yaml` picks which one.

| `use` value | Binary | Protocol |
|---|---|---|
| `pi` (default) | `pi` | JSON-RPC turn request over stdin |
| `codex` | `codex` | `codex app-server` + JSON-RPC turn request |
| `opencode` | `opencode` | `opencode serve` (local HTTP API + SSE event stream) |
| `cli` | `sh` (configurable) | No stdin; reads from `AGENT_*` env vars |
| `fake` | `sh` | Echoes `$AGENT_PROMPT`; test shim only |
| `builtin` | none (in-process) | OpenAI-compatible streaming chat completions over HTTP |

All runners spawn in their own process group so SIGTERM reaches the whole
subprocess tree. `pi` persists per-issue session dirs under
`pi-sessions/ISSUE-N/`; `opencode` under `opencode-sessions/ISSUE-N/` (isolated
XDG dirs + config, with host opencode credentials copied in).

## Builtin runner

`runner.use: builtin` runs the agent loop in-process — no `pi`/`codex`/`opencode`
helper binary on the host. It streams OpenAI-compatible chat completions
directly from a configured provider endpoint and executes tool calls through
the host tool bridge (MCP), capped at 8 tool-call iterations per run.

Configuration:

```yaml
runner:
  use: builtin
  provider: requesty            # required: key into the providers map
  # model: openai/gpt-4o-mini   # provider model id (this is the default)

providers:
  requesty:
    api_url: https://router.requesty.ai/v1
    api_key: $env:REQUESTY_API_KEY   # literal value or $env:VAR indirection
```

`runner.provider` is required and must name a `providers` entry with both
`api_url` and `api_key`; config load / `dar doctor` fails otherwise.

The extension also registers native Pi-compatible coding tools (`read`,
`write`, `edit`, `bash`, root-contained) into the host tool registry, and a
`builtin` chat backend, so a `runner.use: builtin` agent gets TUI/web chat
against the same provider.

Availability: the stock `dar` binary in `dist/` does **not** link
`runner-builtin`; its globally registered tools would collide with pi's. Agent
binaries composed by `dar build` link it only when `agent.yaml` says
`runner.use: builtin`.

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
| `opencode` | ignored (`reasoningEffort` mapping deferred under ALG-226) | — |
| `cli` / `fake` / `builtin` | ignored | — |

> **Breaking change:** the previous WORKFLOW.md `agent.thinking` semantics — a
> pi token-budget string like `"8000"` — have been removed. Only level words are
> accepted; a numeric value is a validation error. The OpenCode runner mapping
> (`reasoningEffort`) is a follow-up tracked under ALG-226.
