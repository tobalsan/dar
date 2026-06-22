# Authoring extensions

How to build any kind of agentropy extension. Read this top-to-bottom once;
`extensions/example/src/lib.rs` is the living reference. For enabling and
configuring an already-written extension, see the README "Adding an extension"
and "Enabling & configuring extensions" sections — this guide is about
*authoring*.

## What an extension is

An extension is one crate under `extensions/`. It depends on `host-api` plus at
most one *capability* crate (`cap-tracker`, `cap-runner`, `cap-chat`, ...);
shared bus-payload crates like `orchestrator-api` don't count against that limit
(the dashboard and `tui` both import it alongside their other deps). It reads
zero host internals — all integration goes through the contracts in
`crates/host-api/src/lib.rs`. A crate is *linked* when it's in the `plugins![]`
list (`dist/src/main.rs`); that is not the same as *enabled* — tracker/runner
services only run when `agent.yaml` `tracker.use` / `runner.use` selects them,
and the foreground only runs when `foreground:` selects it.

## The three kinds

- **background** — registers bus topics and spawns a long-running task in `start`
  (e.g. orchestrator). Use for anything that observes/produces events over time.
- **service** — registers a named `Arc<dyn Trait>` in the registry for others to
  consume (e.g. runners, trackers). Use to provide a swappable capability.
- **foreground** — registers a `Foreground` provider that may own the terminal.
  Use for the one extension that renders to the TTY (e.g. `frontend-log`).

These are lifecycle shapes, not exclusive categories — one extension can do more
than one.

## Quickstart

```bash
cargo agentropy new my-extension --kind background   # or service | foreground
```

This writes `extensions/my-extension/{Cargo.toml,src/lib.rs}` with a compiling
skeleton for the chosen kind, then prints the two wiring lines. Wire it into the
shipped binary (full detail in README):

1. Add `my-extension = { path = "../extensions/my-extension" }` to `dist/Cargo.toml`.
2. Add `my_extension::MyExtension,` to the `plugins![]` list in `dist/src/main.rs`.
3. `cargo build --release`.

Removing an extension is the reverse.

## Anatomy

Implement the `Extension` trait (`crates/host-api/src/lib.rs`). Both methods have
default no-op impls, so override only what you need:

```rust
pub trait Extension: Send + Sync {
    fn id(&self) -> &'static str;
    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> { ... }
    fn start<'a>(&'a self, ctx: StartCtx) -> BoxFuture<'a, Result<()>> { ... }
}
```

- `id()` is your stable name. It keys your config section and is the conventional
  service/topic prefix.
- `register(&mut RegisterCtx)` — declare what you offer: register bus topics,
  services, foreground providers, HTTP routes; read your config. The bus and
  registry are still mutable here.
- `start(StartCtx)` — wiring is frozen; consume what others registered
  (subscribe to topics, `get` services) and spawn your task. `StartCtx` is `Clone`.

Boot lifecycle order (`crates/agentropy-host/src/lib.rs`, `boot_inner`):
**all `register` (list order) → foreground `select` → all `start` (list order) →
the selected foreground's `run`.** So every topic/service you need to consume in
`start` must have been registered by *someone* in a `register` pass — never
depend on another extension's `start` having run first.

`RegisterCtx` fields: `bus`, `http`, `foreground`, `services`, `paths`, `config`,
`shutdown`. `StartCtx` fields: `shutdown`, `paths`, `config`, `host`
(`host.bus`, `host.router`, `host.services` — the frozen `StartServices`).

## Integration surfaces

### Typed services

Register an `Arc<dyn Trait>` under a string id; consumers `get` it by id + type.
Id and Rust type together form the key, so the same id can host different traits.

```rust
// register (in register())
ctx.services.service::<dyn Runner>("fake", Arc::new(FakeRunner))?;

// consume (in start(), via the frozen registry)
let runner = ctx.host.services.get::<dyn Runner>("fake")?;
```

Real ids in the shipped mix: runners `dyn Runner` under `pi` / `codex` /
`cli` / `fake`; trackers `dyn TrackerFactory` under
`files` / `linear`; chat backends `dyn ChatBackend` under `pi`. Id and type
form the key, so `dyn ChatBackend @ "pi"` coexists with `dyn Runner @ "pi"`.
(`register` is an alias for `service`.)

### Dashboard tab

Any extension can contribute a tab to the web dashboard via the cap-style
contract crate `cap-dashboard-tab`. Both the dashboard and a registering
extension depend only on that crate plus `host-api` — no dashboard internals,
no cross-extension imports.

Discovery is service-based: a single shared `DashboardTabs` registry lives in
the host `ServiceRegistry` under `cap_dashboard_tab::DASHBOARD_TABS_SERVICE`.
Rendering is *pull* — a tab returns an HTML **fragment** (a `String`); the
dashboard owns one dynamic route and dispatches `GET /tabs/{id}` to the matching
provider, splicing the fragment into its existing `#content` element via an
`innerHTML`-swap (never a `<body>` swap). The orchestrator run view stays the
default "Runs" tab; with zero registered tabs the dashboard looks exactly as
before (no tab nav is rendered).

Implement `DashboardTab` and add it to the shared registry in `register`
(get-or-create is idempotent and order-independent across extensions):

```rust
use std::sync::Arc;
use cap_dashboard_tab::{DashboardTab, DashboardTabs};

struct MyTab;
impl DashboardTab for MyTab {
    fn id(&self) -> &str { "my-tab" }       // URL-safe; path segment in /tabs/{id}
    fn title(&self) -> &str { "My Tab" }    // label in the tab nav
    fn render(&self) -> anyhow::Result<String> {
        // HTML fragment only — no <html>/<body>. May include its own htmx
        // attributes for in-place polling inside #content.
        Ok("<main><section class=\"panel\"><h2>Hello</h2></section></main>".into())
    }
}

// in register():
DashboardTabs::shared(&mut ctx.services)?.add(Arc::new(MyTab))?;
```

Live updates: while a tab is active the dashboard's `#content` poller already
re-fetches `/tabs/{id}` on the shared cadence, so the whole fragment refreshes
for free — a static `render()` is enough for simple tabs. Declare your own inner
htmx polling only for a finer-grained sub-target (e.g. appending rows to an inner
list with `hx-swap="beforeend"`), the way the run-detail drawer streams events;
otherwise the outer poll would tear down and recreate an inner whole-fragment
poller every cycle. The contract imposes no cadence; it only guarantees the
fragment is composed into `#content`. The `example` extension ships a reference
tab.

### Event bus

Two topic classes (semantics documented at the top of `crates/host-api/src/lib.rs`):

- **broadcast** — bounded fan-out ring; subscribers may observe `Lagged`; values
  before subscription are not replayed. For events.
- **retained** — keeps exactly one current value; new subscribers see the latest
  immediately; `read_retained` reads without subscribing. For state snapshots.

Every topic has exactly one owner — the extension that registers it; everyone
else is a consumer. E.g. `frontend-log` owns `host.log-events`,
`host.app-done`, and `host.startup-banner`; the `tui` foreground registers no
topics at all and only consumes those (plus the orchestrator's two).

Register topics in `register`, subscribe/publish in `start`:

```rust
// register
ctx.bus.register_broadcast::<ExampleEvent>(EXAMPLE_EVENTS_TOPIC, 16)?;
ctx.bus.register_retained(EXAMPLE_STATE_TOPIC, ExampleState::default())?;

// start
let mut events = ctx.host.bus.subscribe::<ExampleEvent>(EXAMPLE_EVENTS_TOPIC)?;
ctx.host.bus.publish(EXAMPLE_STATE_TOPIC, new_state)?;
```

Orchestration payloads live in `crates/orchestrator-api`. To react to run state,
read the retained `RunSnapshot`; the dashboard does exactly this:

```rust
let snapshot = bus
    .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
    .unwrap_or_else(|_| RunSnapshot::empty());
```

To affect a run, publish a `ControlMsg` on `CONTROL_TOPIC` (the orchestrator
bridges it internally) — the dashboard never mutates run state directly:

```rust
bus.publish(CONTROL_TOPIC, ControlMsg::Pause)?;
```

### Foreground slot

At most one extension owns the terminal. Register a provider factory in
`register`; the host calls `run` for the one selected by `agent.yaml`
`foreground:` (default `"logs"`). `frontend-log` is the reference:

```rust
// register
ctx.foreground.foreground("logs", Arc::new(|| Box::new(FrontendLogForeground)))?;

// the Foreground impl owns stdout via ExclusiveTerminal until shutdown
impl Foreground for FrontendLogForeground {
    fn run<'a>(&'a mut self, ctx: StartCtx, mut terminal: ExclusiveTerminal)
        -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            writeln!(terminal.writer(), "...")?;
            ctx.shutdown.clone().cancelled().await;
            terminal.restore();
            Ok(())
        })
    }
}
```

If multiple foregrounds are registered and `foreground:` is unset, boot fails;
an unknown configured id also fails boot.

### Config

Each extension reads its own section of `agent.yaml`'s `extensions:` map, keyed
by `id()`, via `ConfigStore`. Missing section = `None`:

```rust
let cfg = match ctx.config.get(self.id()) {
    Some(value) => serde_json::from_value::<ExampleConfig>(value.clone())?,
    None => ExampleConfig::default(),
};
```

Validate in `register` and `bail!` on bad config — that surfaces as a clean boot
error (and pages the operator via the startup-error hook).

### Paths and containment

`HostPaths` (in both `RegisterCtx` and `StartCtx`) gives the agent root and a
per-extension data dir. Never write outside the agent root: any
externally-derived path must pass `assert_contained`, which canonicalizes and
rejects `..` / symlink escapes.

```rust
let data_dir = ctx.data_dir(self.id())?;            // <root>/data/<id>, contained
let safe = ctx.paths.assert_contained(some_path)?;  // errors if it escapes root
```

### Shutdown

Long-running tasks must respect the `ShutdownToken`. The bus does not drain on
shutdown — stop producing and finish in-flight work when cancelled:

```rust
let mut shutdown = ctx.shutdown.clone();
tokio::spawn(async move {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            msg = rx.recv() => { /* handle */ }
        }
    }
});
```

## Stock extensions

Most stock extensions (orchestrator, dashboard, trackers, runners, chats, tui)
are baseline and always linked. A few are **opt-in**: they link into a composed
per-agent binary only when their `extensions.<id>` section is present in
`agent.yaml`, so a binary without the section behaves exactly as before.

### `scheduler`

Fires per-agent cron jobs and writes their output as markdown. On boot it loads
`cron/jobs.json`, computes each enabled job's next fire from its cron expression
+ IANA timezone (+ optional `startAt` anchor, anchored at `max(now, startAt)`),
arms a single timer for the earliest, and when due spawns the agent's default
runner (`runner.use`) with the job's `payload.message` as the prompt. The
captured response is written to `cron/output/<job_id>/<timestamp>.md` with
aihub-shape frontmatter (job id, run type, fired/finished, status, duration,
schedule) plus a readable prompt/response body, for both ok and error runs. All
jobs due at one tick run concurrently in their own tasks; the timer re-arms after
every tick — including after a skipped, hung, erroring, or panicking job — so one
bad job never wedges the schedule loop. A malformed `cron/jobs.json` logs one
warning and is treated as empty; a missing file is empty.

#### Execution guards

The scheduler enforces three safety semantics so it is trustworthy unattended:

- **Overlap-skip.** A scheduled fire of a job whose previous run is still in
  flight is skipped: a warning is logged, the skip is bookmarked (so later
  run-now logic can tell a skip from a normal completion), and the job's next
  fire is recomputed so the timer re-arms forward. The same job never overlaps
  itself.
- **Timeout.** Every run is bounded by a timeout. The effective timeout is the
  per-job `timeoutMs`, else `extensions.scheduler.jobTimeoutMs`, else a
  10-minute default. On timeout the runner child is killed and the run is
  recorded as an `error` output file with a timeout message; the job's next fire
  is still computed.
- **Kill switch (boot-time).** `extensions.scheduler.enabled: false` prevents
  arming any timers — nothing fires — while `cron/jobs.json` stays
  readable/writable. Because the host freezes the whole `extensions.*` config map
  after boot (it is not live-reloaded), flipping `enabled` or changing
  `jobTimeoutMs` takes effect only after a host **restart**. Per-job `enabled:
  false` inside `cron/jobs.json`, by contrast, is read from the (hot-reloaded)
  jobs file: a disabled job never fires and never gets a next-run.

Invalid `extensions.scheduler` config (unknown field, wrong type, or a zero
`jobTimeoutMs`) fails boot with a clean error naming the problem, rather than
surfacing at first fire.

Enable it by adding the section (presence selects it). `pollIntervalMs`
(default `2000`, floored at `250`) tunes how quickly external edits to
`cron/jobs.json` are hot-reloaded:

```yaml
extensions:
  scheduler:
    enabled: true        # `false` = boot-time kill switch (CRUD stays live, no fires)
    jobTimeoutMs: 600000 # optional; default per-run timeout in ms (10 min)
    pollIntervalMs: 2000 # optional; hot-reload poll cadence in ms (floored at 250)
```

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

A per-job timeout override is set with `timeoutMs` on the job (milliseconds),
taking precedence over `extensions.scheduler.jobTimeoutMs` and the default.

#### Self-service hot reload (the file is the API)

The scheduler polls `cron/jobs.json` on `pollIntervalMs` and refreshes its
in-memory job set whenever the file changes (detected by modified-time + length).
This makes the jobs file itself the self-service surface: a child agent — or a
human — can add, remove, or edit a job by writing the file, and the schedule
change is applied within the poll interval, **without restarting the host**. The
file shape shown above *is* the API; there is no separate tool call to register
a job. The same job set is also exposed over the Scheduler HTTP API (below);
file edits and HTTP mutations feed the same in-memory state.

Reload semantics:

- **Add / change / remove** a job → reflected within the poll interval and the
  timer is re-armed to the new earliest fire.
- **Per-job `enabled: false`** inside `cron/jobs.json` is live-reloaded: flip it
  and the job drops out (or back in) on the next poll. This is distinct from the
  boot-time `extensions.scheduler.enabled` switch above, which is immutable at
  runtime.
- **Malformed edit** → one warning, the file is treated as empty (no jobs armed),
  no crash; the scheduler recovers automatically on the next valid write.
- **In-flight runs survive a reload.** Each job carries an overlap guard; a job
  edited while one of its runs is in flight keeps that guard, so overlap-skip
  still applies (a second fire is skipped until the first run returns). A job
  deleted while running is dropped from the armed set but its in-flight run owns
  its own guard handle and completes (and writes output) normally — it is never
  orphan-tracked twice.
- The poll loop selects on the shutdown token, so a shutdown during polling or
  between fires stops the scheduler promptly.

Parity gaps vs the aihub scheduler (tracked in follow-up slices): no per-job
model override, no `sessionId` continuity, and no CLI. The captured response is
the runner's last assistant-side output line, a best-effort proxy for plain
runners. Job ids are validated to a single safe path segment at load (ids with
`/`, `\`, `..`, or a leading dot are skipped with a warning) so output paths
stay under the agent root.

#### Scheduler HTTP API

When the scheduler is enabled it mounts a job-management API on the host HTTP
server under the `/scheduler` namespace. These are **single-agent** paths: the
agent is implied by the host process, so there are no `agentId` segments (unlike
the aihub multi-agent API). Mutations validate the request, persist the new job
set atomically to `cron/jobs.json` (temp file + rename), and re-arm the timer
immediately so a sooner schedule fires without waiting for the current sleep.
Runtime state (next/last run, last status, running-for) lives in memory and is
never written to disk; it is merged into list/create/update responses.

| Method   | Path                   | Description                                                                  |
| -------- | ---------------------- | ---------------------------------------------------------------------------- |
| `GET`    | `/scheduler/jobs`      | List jobs, each merged with runtime state.                                   |
| `POST`   | `/scheduler/jobs`      | Create a job. The id is generated server-side; `enabled` defaults to `true`. Returns `201` with the new job. |
| `PATCH`  | `/scheduler/jobs/{id}` | Patch any of `name`, `enabled`, `schedule`, `payload`, `timeoutMs`. Re-arms when the schedule changes. |
| `DELETE` | `/scheduler/jobs/{id}` | Remove a job. Returns `204`.                                                 |
| `POST`   | `/scheduler/jobs/{id}/run-now` | Fire the job immediately without disturbing its schedule. See below. |
| `GET`    | `/scheduler/jobs/{id}/tail`    | Return the newest output file for the job (path + content). |

Request/response job shape (runtime fields are read-only and present only on
responses):

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

Validation: a bad cron expression, a missing or unknown `tz`, an out-of-range
`startAt`, or an empty `payload.message` returns `400` with an `{ "error": ... }`
body; an unknown job id on update/delete returns `404`.

##### Run now and tail (operator test-and-inspect loop)

`POST /scheduler/jobs/{id}/run-now` fires a job **immediately** through the same
path as a scheduled fire — the output file is written to
`cron/output/<job_id>/<timestamp>.md` like any scheduled run — **without
disturbing the schedule**. The job's previously computed next fire is restored
after the manual run, _unless_ a scheduled fire was overlap-skipped while the
manual run was in flight; in that case the loop's recomputed next fire stands
(the skipped occurrence is consumed, not replayed — aihub's skipped-fire
bookkeeping). The response carries the run result:

| Outcome                          | Status | Body                                            |
| -------------------------------- | ------ | ----------------------------------------------- |
| Ran ok                           | `200`  | `{ status: "ok", firedAt, finishedAt, outputPath, error: null, job }` |
| Ran with error                   | `500`  | `{ status: "error", ..., error: "<message>", job }` |
| Job disabled (inactive)          | `500`  | `{ status: "inactive", error, job: null }`      |
| Fire skipped before running      | `202`  | `{ status: "skipped", error, job: null }`       |
| Job already running              | `409`  | `{ error: "job ... is already running" }`       |
| Unknown job id                   | `404`  | `{ error: ... }`                                |

`GET /scheduler/jobs/{id}/tail` returns the **newest** output file for the job —
the lexicographically-greatest entry in `cron/output/<job_id>/` (filenames are
`YYYY-MM-DD_HH-mm-ss.md`, so lexicographic order is timestamp order):

| Outcome             | Status | Body                                  |
| ------------------- | ------ | ------------------------------------- |
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

#### Cron dashboard tab (read-only)

When the scheduler is enabled it contributes a read-only **"Cron"** tab to the
web dashboard through the [dashboard-tab contract](#dashboard-tab). The tab lists
each job with its schedule + timezone, enabled flag, next/last run times, last
status (with the error message when the last run failed), running-for, and the
most recent output files per job. It refreshes with the dashboard's existing
self-poll: when the Cron tab is active the dashboard re-fetches its fragment
into `#content` on the shared cadence (the same poller the orchestrator run view
uses), so the fragment carries no inner poll of its own.

The tab is **read-only** — there are no mutation controls. All mutation stays
with the Scheduler HTTP API above and direct edits to `cron/jobs.json`. The tab
is present only when the scheduler is linked and enabled: in the shipped `dist`
binary that means an `extensions.scheduler` section that is not `enabled: false`;
in an FSC per-agent binary it means the composer selected the scheduler crate
(driven by the `agent.yaml` section). When the scheduler is absent or
kill-switched, no tab is registered and the dashboard renders exactly as before.

Cron activity is surfaced at the scheduler's own level: scheduled runs fire the
runner service directly and **never** appear in the orchestrator's run list or
its retained `RunSnapshot`. The orchestrator "Runs" view stays the default tab.

## Rules and invariants

- **Never write issue state.** Issue `state:` is owned by the tracker and changed
  only by the agent or a human (the two-state invariant). The `Tracker` trait is
  read-only by design. Control messages mutate *run* state only.
- **One foreground owner.** Don't claim the terminal unless you are the
  foreground; registering a second provider without selection fails boot.
- **Don't block `register`/`start`.** They run inline in the boot sequence; do
  long work in a spawned task, not in the method body.
- **No host internals.** Depend on `host-api` (+ at most one capability crate;
  bus-payload crates like `orchestrator-api` don't count) only; integrate
  through services, the bus, and the registries.

## Testing

Existing extensions use two layers:

- **In-crate unit tests** that build a throwaway `RegisterCtx`, call `register`,
  then `start`, then drive the bus and assert. See the `#[tokio::test]` in
  `extensions/example/src/lib.rs` (`smoke_register_start_publish_subscribe_and_shutdown`)
  and `extensions/frontend-log/src/lib.rs` (`registers_logs_foreground_and_topic`).
- **Boot-level tests** in `crates/agentropy-host/src/lib.rs` that boot a list of
  fake extensions and assert lifecycle ordering and foreground selection.

Capability-contract behavior (the `Runner` / `Tracker` / `ChatBackend` builders
your service must satisfy) is exercised in `crates/cap-runner/tests/builder.rs`,
`crates/cap-tracker/tests/builder.rs`, and `crates/cap-chat/tests/builder.rs` —
mirror those when implementing a service.

Run a single test by name:

```bash
cargo test --release smoke_register_start_publish_subscribe_and_shutdown
```
