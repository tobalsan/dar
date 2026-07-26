# Runners

The runner backend spawns the child process that does the actual work for an issue; `runner.use` in `agent.yaml` picks which one.

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
