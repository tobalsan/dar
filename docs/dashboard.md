# Dashboard

The dashboard is the live control plane for a running agent, served over HTTP.

`http://127.0.0.1:<port>/` — single page, live updates via HTMX + WebSocket.
No auth.

**Panels:** agent identity, active runs with elapsed time and last event,
queue, retry queue, run history (persisted in SQLite across restarts).

**Controls** (mutate run state only, never issue state):

- **Stop** — kill all active children (SIGTERM → 5s grace → SIGKILL). Issue
  files untouched; next tick may re-dispatch.
- **Pause** — stop picking up new issues; current runs keep going.
- **Resume** — resume polling.

## HTTP API

```bash
# Controls
# Replace 7878 if this workflow uses a configured or ephemeral port.
curl -X POST http://127.0.0.1:7878/self-update/rebuild # 202 means accepted
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
