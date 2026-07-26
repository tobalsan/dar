# Scheduler (cron jobs)

The scheduler is an opt-in stock extension that fires per-agent cron jobs from `cron/jobs.json` and writes each run's output as markdown.

## Enabling & config

Most stock extensions (orchestrator, dashboard, trackers, runners, chats, tui) are baseline and always linked. A few are **opt-in**: they link into a composed per-agent binary only when their `extensions.<id>` section is present in `agent.yaml`, so a binary without the section behaves exactly as before. The **`scheduler`** extension is opt-in this way.

Enable it by adding the section (presence selects it). `pollIntervalMs` (default `2000`, floored at `250`) tunes how quickly external edits to `cron/jobs.json` are hot-reloaded:

```yaml
extensions:
  scheduler:
    enabled: true        # `false` = boot-time kill switch (CRUD stays live, no fires)
    jobTimeoutMs: 600000 # optional; default per-run timeout in ms (10 min)
    pollIntervalMs: 2000 # optional; hot-reload poll cadence in ms (floored at 250)
```

Invalid `extensions.scheduler` config (unknown field, wrong type, or a zero `jobTimeoutMs`) fails boot with a clean error naming the problem, rather than surfacing at first fire.

See [Configuration](configuration.md) for the full `agent.yaml` reference.

## Job file (`cron/jobs.json`)

On boot the scheduler loads `cron/jobs.json`, computes each enabled job's next fire from its cron expression + IANA timezone (+ optional `startAt` anchor, anchored at `max(now, startAt)`), arms a single timer for the earliest, and when due spawns the agent's default runner (`runner.use`) with the job's `payload.message` as the prompt. The captured response is written to `cron/output/<job_id>/<timestamp>.md` with aihub-shape frontmatter (job id, run type, fired/finished, status, duration, schedule) plus a readable prompt/response body, for both ok and error runs. All jobs due at one tick run concurrently in their own tasks; the timer re-arms after every tick — including after a skipped, hung, erroring, or panicking job — so one bad job never wedges the schedule loop. A malformed `cron/jobs.json` logs one warning and is treated as empty; a missing file is empty.

```jsonc
// cron/jobs.json
{
  "version": 1,
  "jobs": [
    {
      "id": "morning-digest",
      "name": "Morning digest",
      "enabled": true,
      "schedule": { "cron": "0 8 * * *", "tz": "Europe/Paris", "startAt": "2026-05-19T07:00:00.000Z" },
      "payload": { "message": "Summarize overnight events." },
      "timeoutMs": 300000
    }
  ]
}
```

A per-job timeout override is set with `timeoutMs` on the job (milliseconds), taking precedence over `extensions.scheduler.jobTimeoutMs` and the default.

Jobs have three shapes, chosen by fields rather than a `kind` value:

- **Agent job:** `message` only. The runner is started for every fire.
- **Script-only job:** `script` with `noAgent: true`. The scheduler runs the script and never starts a runner; exit code 0 is success, while a non-zero exit or timeout is recorded mechanically as an error.
- **Gated agent job:** `script` plus `message`. The script is a cheap gate that decides whether to start the runner. Its final stdout line is parsed as JSON: `{"wakeAgent":false}` is a successful silent tick; `{"wakeAgent":true, "context":{...}}` starts the runner with the serialized context appended to the message. A missing `wakeAgent` field or a non-JSON final line defaults to waking the runner.

This complete `cron/jobs.json` contains one job of each shape:

```json
{
  "version": 1,
  "jobs": [
    {
      "id": "daily-digest",
      "schedule": { "cron": "0 8 * * *", "tz": "UTC" },
      "payload": { "message": "Write the daily digest." }
    },
    {
      "id": "rotate-token",
      "schedule": { "cron": "0 * * * *", "tz": "UTC" },
      "payload": {
        "script": "cron/scripts/rotate-token.sh",
        "noAgent": true,
        "quietOutput": true
      }
    },
    {
      "id": "changed-files",
      "schedule": { "cron": "*/5 * * * *", "tz": "UTC" },
      "payload": {
        "script": "cron/scripts/changed-files.sh",
        "message": "Review the changed files and report the important changes."
      }
    }
  ]
}
```

For example, create `cron/scripts/changed-files.sh`, make it executable, and use it as the gate above:

```bash
#!/usr/bin/env bash
set -euo pipefail

state_file="cron/changed-files.last"
current="$(git rev-parse HEAD)"
previous="$(test -f "$state_file" && cat "$state_file" || true)"
printf '%s\n' "$current" > "$state_file"

if [ "$current" = "$previous" ]; then
  printf '%s\n' '{"wakeAgent":false}'
else
  printf '{"wakeAgent":true,"context":{"previous":"%s","current":"%s"}}\n' \
    "$previous" "$current"
fi
```

Scripts must be relative to and resolve inside the agent root: absolute paths, `..` escapes, and symlink escapes are rejected when jobs load. `.sh` and `.bash` scripts run with `bash`; every other extension must be executable. The default working directory is the agent root.

## Output files

Every run normally writes `cron/output/<job_id>/<timestamp>.md`, with a `Status:` header of `ok`, `ok (silent tick)`, `woke agent`, or `script failed (exit N)`. `quietOutput: true` is for high-frequency script jobs: it omits only an empty-output successful script-only run or a gated silent tick. Errors, non-empty script output, and runs that wake the agent always keep an output file; runtime status still updates on every tick.

## Delivering results

Jobs keep their local output files as the canonical record. Add `deliver` to push a completed result from the runtime (not from the job's agent) to a registered communication extension:

```json
{
  "version": 1,
  "jobs": [{
    "id": "status",
    "name": "Status report",
    "enabled": true,
    "schedule": { "cron": "0 9 * * *", "tz": "Europe/Paris" },
    "payload": { "message": "Report current status." },
    "deliver": [
      { "target": "slack", "channel": "#alerts" },
      { "target": "telegram", "user": "12345" },
      { "target": "discord", "channel": "ops" }
    ]
  }]
}
```

Agent responses and non-empty script stdout are delivered. Silent gate ticks and empty script stdout are not; errors are always sent. A missing sink or send failure is recorded as a warning and does not change the job run's status.

## Execution guards

The scheduler enforces three safety semantics so it is trustworthy unattended:

- **Overlap-skip.** A scheduled fire of a job whose previous run is still in flight is skipped: a warning is logged, the skip is bookmarked (so later run-now logic can tell a skip from a normal completion), and the job's next fire is recomputed so the timer re-arms forward. The same job never overlaps itself.
- **Timeout.** Every run is bounded by a timeout. The effective timeout is the per-job `timeoutMs`, else `extensions.scheduler.jobTimeoutMs`, else a 10-minute default. On timeout the runner child is killed and the run is recorded as an `error` output file with a timeout message; the job's next fire is still computed.
- **Kill switch (boot-time).** `extensions.scheduler.enabled: false` prevents arming any timers — nothing fires — while `cron/jobs.json` stays readable/writable. Because the host freezes the whole `extensions.*` config map after boot (it is not live-reloaded), flipping `enabled` or changing `jobTimeoutMs` takes effect only after a host **restart**. Per-job `enabled: false` inside `cron/jobs.json`, by contrast, is read from the (hot-reloaded) jobs file: a disabled job never fires and never gets a next-run.

## Hot reload (the file is the API)

The scheduler polls `cron/jobs.json` on `pollIntervalMs` and refreshes its in-memory job set whenever the file changes (detected by modified-time + length). This makes the jobs file itself the self-service surface: a child agent — or a human — can add, remove, or edit a job by writing the file, and the schedule change is applied within the poll interval, **without restarting the host**. The file shape shown above *is* the API; there is no separate tool call to register a job. The same job set is also exposed over the Scheduler HTTP API (below); file edits and HTTP mutations feed the same in-memory state.

Reload semantics:

- **Add / change / remove** a job → reflected within the poll interval and the timer is re-armed to the new earliest fire.
- **Per-job `enabled: false`** inside `cron/jobs.json` is live-reloaded: flip it and the job drops out (or back in) on the next poll. This is distinct from the boot-time `extensions.scheduler.enabled` switch above, which is immutable at runtime.
- **Malformed edit** → one warning, the file is treated as empty (no jobs armed), no crash; the scheduler recovers automatically on the next valid write.
- **In-flight runs survive a reload.** Each job carries an overlap guard; a job edited while one of its runs is in flight keeps that guard, so overlap-skip still applies (a second fire is skipped until the first run returns). A job deleted while running is dropped from the armed set but its in-flight run owns its own guard handle and completes (and writes output) normally — it is never orphan-tracked twice.
- The poll loop selects on the shutdown token, so a shutdown during polling or between fires stops the scheduler promptly.

## HTTP API

When the scheduler is enabled it mounts a job-management API on the host HTTP server under the `/scheduler` namespace. These are **single-agent** paths: the agent is implied by the host process, so there are no `agentId` segments (unlike the aihub multi-agent API). Mutations validate the request, persist the new job set atomically to `cron/jobs.json` (temp file + rename), and re-arm the timer immediately so a sooner schedule fires without waiting for the current sleep. Runtime state (next/last run, last status, running-for) lives in memory and is never written to disk; it is merged into list/create/update responses.

A create/update/delete re-arms the timer in-process so a sooner schedule fires immediately.

| Method   | Path                   | Description                                                                  |
| -------- | ---------------------- | ---------------------------------------------------------------------------- |
| `GET`    | `/scheduler/jobs`      | List jobs, each merged with runtime state.                                   |
| `POST`   | `/scheduler/jobs`      | Create a job. The id is generated server-side; `enabled` defaults to `true`. Returns `201` with the new job. |
| `PATCH`  | `/scheduler/jobs/{id}` | Patch any of `name`, `enabled`, `schedule`, `payload`, `timeoutMs`. Re-arms when the schedule changes. |
| `DELETE` | `/scheduler/jobs/{id}` | Remove a job. Returns `204`.                                                 |
| `POST`   | `/scheduler/jobs/{id}/run-now` | Fire the job immediately without disturbing its schedule. See below. |
| `GET`    | `/scheduler/jobs/{id}/tail`    | Return the newest output file for the job (path + content). |

Request/response job shape (runtime fields are read-only and present only on responses):

```jsonc
{
  "id": "job-1750000000000",      // server-generated on create
  "name": "Morning digest",
  "enabled": true,                  // defaults to true on create
  "schedule": { "cron": "0 8 * * *", "tz": "Europe/Paris", "startAt": "2026-05-19T07:00:00.000Z" },
  "payload": { "message": "Summarize overnight events." },
  "timeoutMs": 60000,              // optional; falls back to runner.max_run_timeout_ms
  // runtime-only (responses):
  "nextRunAtMs": 1750000000000,
  "lastRunAtMs": null,
  "lastStatus": null,             // "ok" | "error" | null
  "lastError": null,              // error message when lastStatus == "error", else null
  "runningForMs": null
}
```

Validation: a bad cron expression, a missing or unknown `tz`, an out-of-range `startAt`, or an empty `payload.message` returns `400` with an `{ "error": ... }` body; an unknown job id on update/delete returns `404`.

### Run now and tail (operator test-and-inspect loop)

`POST /scheduler/jobs/{id}/run-now` fires a job **immediately** through the same path as a scheduled fire — the output file is written to `cron/output/<job_id>/<timestamp>.md` like any scheduled run — **without disturbing the schedule**. The job's previously computed next fire is restored after the manual run, _unless_ a scheduled fire was overlap-skipped while the manual run was in flight; in that case the loop's recomputed next fire stands (the skipped occurrence is consumed, not replayed — aihub's skipped-fire bookkeeping). The response carries the run result:

| Outcome                          | Status | Body                                            |
| --------------------------------- | ------ | ----------------------------------------------- |
| Ran ok                           | `200`  | `{ status: "ok", firedAt, finishedAt, outputPath, error: null, job }` |
| Ran with error                   | `500`  | `{ status: "error", ..., error: "<message>", job }` |
| Job disabled (inactive)          | `500`  | `{ status: "inactive", error, job: null }`      |
| Fire skipped before running      | `202`  | `{ status: "skipped", error, job: null }`       |
| Job already running              | `409`  | `{ error: "job ... is already running" }`       |
| Unknown job id                   | `404`  | `{ error: ... }`                                |

`GET /scheduler/jobs/{id}/tail` returns the **newest** output file for the job — the lexicographically-greatest entry in `cron/output/<job_id>/` (filenames are `YYYY-MM-DD_HH-mm-ss.md`, so lexicographic order is timestamp order):

| Outcome             | Status | Body                                  |
| ------------------- | ------ | -------------------------------------- |
| Newest output found | `200`  | `{ "path": "...", "content": "..." }` |
| Job has no outputs  | `404`  | `{ "error": ... }`                    |
| Unknown job id      | `404`  | `{ "error": ... }`                    |

Example lifecycle with `curl` (replace `$PORT` with the agent's bound HTTP port):

```bash
# Create
curl -sS -XPOST localhost:$PORT/scheduler/jobs \
  -H content-type:application/json \
  -d '{"name":"digest","schedule":{"cron":"0 8 * * *","tz":"UTC"},"payload":{"message":"hi"}}'
# List
curl -sS localhost:$PORT/scheduler/jobs
# Update (patch)
curl -sS -XPATCH localhost:$PORT/scheduler/jobs/<id> \
  -H content-type:application/json -d '{"enabled":false}'
# Run now, then read the fresh output
curl -sS -XPOST localhost:$PORT/scheduler/jobs/<id>/run-now
curl -sS localhost:$PORT/scheduler/jobs/<id>/tail
# Delete
curl -sS -XDELETE localhost:$PORT/scheduler/jobs/<id>
```

## Cron dashboard tab (read-only)

When the scheduler is enabled it contributes a read-only **"Cron"** tab to the web dashboard through the [dashboard-tab contract](extensions.md#dashboard-tab). The tab lists each job with its schedule + timezone, enabled flag, next/last run times, last status (with the error message when the last run failed), running-for, and the most recent output files per job. It refreshes with the dashboard's existing self-poll: when the Cron tab is active the dashboard re-fetches its fragment into `#content` on the shared cadence (the same poller the orchestrator run view uses), so the fragment carries no inner poll of its own.

The tab is **read-only** — there are no mutation controls. All mutation stays with the Scheduler HTTP API above and direct edits to `cron/jobs.json`. The tab is present only when the scheduler is linked and enabled: in the shipped `dist` binary that means an `extensions.scheduler` section that is not `enabled: false`; in an FSC per-agent binary it means the composer selected the scheduler crate (driven by the `agent.yaml` section). When the scheduler is absent or kill-switched, no tab is registered and the dashboard renders exactly as before.

Cron activity is surfaced at the scheduler's own level: scheduled runs fire the runner service directly and **never** appear in the orchestrator's run list or its retained `RunSnapshot`. The orchestrator "Runs" view stays the default tab.

## Parity gaps

Parity gaps vs the aihub scheduler (tracked in follow-up slices): no per-job model override, no `sessionId` continuity, and no CLI. The captured response is the runner's last assistant-side output line, a best-effort proxy for plain runners. Job ids are validated to a single safe path segment at load (ids with `/`, `\`, `..`, or a leading dot are skipped with a warning) so output paths stay under the agent root.
