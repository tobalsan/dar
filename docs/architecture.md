# Architecture

How Dar's tick loop, state layers, and agent folder fit together.

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

## Persistence

SQLite at `<agent-folder>/data/store.db` (WAL mode). Tables: `runs`, `events`,
`claims`, `heartbeats`. On startup, any run whose PID was left open from a
previous invocation is killed and marked `crashed`. The in-memory history ring
is seeded from SQLite, so the dashboard's run history survives restarts.

Log file at `logs/agent.log` (also streamed to terminal stderr).
