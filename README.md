# Dar

A self-contained, folder-scoped agent runtime. One Rust binary, run from inside
an agent folder. It polls an issue tracker, dispatches AI coding-agent children
into per-issue workspaces, watches them finish, and loops — with a live
dashboard to observe and control the world.

Supports Pi, Codex, and arbitrary CLI runners. Trackers: local
Markdown files or Linear.

```
                 ┌─────────────── dar run ───────────────┐
   issues/       │  reconcile → collect_finished → dispatch      │   dashboard
  (the truth) ──▶│            (in-memory run state only)        │──▶ :7878 (HTMX + WS)
                 └──────────────────────┬──────────────────────┘
                                        ▼
                        runner child  (cwd = workspaces/ISSUE-N/)
                        edits its own issue file → state: done
```

## Install

Build from source (this is **build A** — one binary with all stock extensions
baked in):

```bash
cargo build --release          # → ./target/release/dar (and cargo-dar)
```

The runner backend named by `agent.yaml` `runner.use` must be installed and
authenticated on the host — it is not bundled.

An agent can instead carry its own composition crate and binary (**build B**),
linking only what it uses and self-updating in place — see
[Self-contained agents](docs/self-contained-agents.md).

## Quick start

```bash
./target/release/dar doctor --dir ./example-agent   # config/template/tracker preflight
./target/release/dar run    --dir ./example-agent   # long-running loop + dashboard
open http://127.0.0.1:7878/
```

`example-agent` ships the `fake` runner, so it has no host dependency.

An agent folder is self-contained — move the folder, move the agent:

```
my-agent/
├── agent.yaml          # base config
├── WORKFLOW.md         # required only when the issue loop is enabled
├── issues/             # required only for the local files tracker
├── workspaces/         # created at runtime by the orchestrator loop
├── data/store.db       # SQLite: runs, events, claims, heartbeats
├── logs/agent.log
├── bin/dar             # agent's own binary (build B only)
└── .dar/               # committed composition crate (build B only)
```

## Documentation

| Doc | What's in it |
|---|---|
| [Architecture](docs/architecture.md) | The two state layers, the tick loop, retries, folder layout, persistence |
| [CLI reference](docs/cli.md) | Every `dar` command and flag |
| [Configuration](docs/configuration.md) | `agent.yaml` reference, system files, `.env` + `AGENT_*` variables, HITL notifier |
| [WORKFLOW.md](docs/workflows.md) | Prompt template, issue-loop frontmatter, `--workflow` (one agent, many hats) |
| [Trackers](docs/trackers.md) | Local issue files, Linear, Plane |
| [Runners](docs/runners.md) | Runner backends and the thinking/reasoning level |
| [Dashboard](docs/dashboard.md) | Panels, controls, and the HTTP API |
| [Chat surfaces](docs/chat.md) | Terminal UI (`foreground: tui`) and web chat |
| [Scheduler](docs/scheduler.md) | Cron jobs, gate scripts, delivery, HTTP API |
| [Self-contained agents](docs/self-contained-agents.md) | Per-agent binary (build B), local extensions, self-update |
| [Extensions](docs/extensions.md) | Enabling/configuring extensions, and the authoring guide |
