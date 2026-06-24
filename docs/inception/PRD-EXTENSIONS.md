# PRD — Extension Architecture Refactor

> Status: draft (council-revised) · needs-triage
> Companion visuals: `extension-design-visual.html`, `extension-architecture.html`
> Supersedes the monolith layout in `PRD.md` (behavior contracts in `PRD.md` still hold; only the packaging changes).
> Revision note: this draft incorporates a /council review (two independent synthesis rounds, consistent verdict). Key corrections vs the first draft: a **generic typed service registry** replaces the domain-aware `Role` enum; an **exclusive `foreground` slot** is added so an extension can *replace* top-level terminal behavior (the TUI requirement); today's log streaming becomes the `frontend-log` extension; an **`orchestrator-api`** contract crate exposes run snapshots/control so dashboard and TUI integrate without importing the orchestrator; `cap-channel` and the host channel registry are **dropped** for now; `inventory` is demoted in favor of an explicit `dist` plugin list; nothing is "frozen."

## Problem Statement

Dar today is one Rust binary. Every capability — the Linear/file trackers, the claude/codex/pi/cli runners, the orchestrator tick loop, the dashboard, the terminal log stream — lives in `src/` and compiles together. That blocks the next phase:

- I cannot develop a new runner, tracker, or a brand-new feature (scheduler, Discord channel, TUI) **in its own crate/repo** without touching core.
- Adding a backend means editing core dispatch (`tracker::build`, `RunnerKind::parse`) — a `match` arm per backend. Core knows every concrete backend by name.
- There is no seam for feature types that aren't tracker/runner: a scheduler, a channel, a TUI. The architecture only has slots for what v0 happened to need.
- **The critical gap:** an extension cannot *replace* a top-level behavior. Today the binary streams logs to the terminal. A TUI must be able to make the binary display a full TUI **instead** — own raw mode, the keyboard, the alternate screen, panic cleanup. An additive plugin called by the host cannot do that.
- The core conflates "plumbing every feature needs" with "the orchestration domain." The binary *is* the orchestrator; there's no way to ship a build that does something else.

I want the core stripped to a **raw, domain-free host** that knows zero domain concepts (not even "Issue"), every current feature re-expressed as an extension, and the act of writing the *next* extension (scheduler, channels, TUI) to be simple. Design should stay open — let contracts emerge as real implementations prove them — while the **initial set ships fully functional**: orchestrator loop, Linear tracker, the runners.

## Solution

A **cargo workspace** (monorepo for now) in three layers.

### Layer 1 — Contract crates (tiny, additive, `0.x`, never "frozen")
- `host-api` — generic plumbing only: extension lifecycle, config access by id, a **typed service registry**, a **typed event bus**, HTTP mount, paths/containment + per-extension data dir, a shutdown/cancellation token, and **exclusive slots** (the `foreground` owner). Knows no domain — no `Issue`, no `Tracker`, no `Role`.
- `cap-tracker` — the `Tracker` trait (only the methods the orchestrator calls) + the `Issue` model (`#[non_exhaustive]`, builder, `metadata` map). Owns tracker provider registration.
- `cap-runner` — the `Runner` trait + `SpawnParams`/`RunnerHandle`/`ExitKind`/`KillReason` + normalized output events + `AGENT_*` constants. Contract types only.
- `orchestrator-api` — the orchestrator's *public* contract: `RunSnapshot`, `RunStatus`, `ControlMsg`, `RunRequested`/`DispatchRequested`, normalized log rows. Lets dashboard/TUI name run state without importing the orchestrator impl.
- `runner-core` — shared (non-contract) helper crate: process-group supervision, line pump, `term_then_kill`, protocol-line classification. Backends call it; not part of the public contract.

**No `cap-channel` in this PRD.** No `dar-events` mega-crate — event payloads live in the API crate that *owns* them (`orchestrator-api` owns run/dispatch events; `cap-runner` owns runner output events; a future `chat-api` will own chat events).

### Layer 2 — The host (`dar-host`)
Minimal binary plumbing implementing the `host-api` services. Two-phase boot:
1. construct registered extensions;
2. `register()` on all (they contribute services, bus subscriptions, HTTP routes, foreground candidates);
3. select **exactly one** foreground provider from config;
4. `start()` background extensions;
5. hand the **main thread + terminal** to the selected foreground owner;
6. on shutdown: cancel background tasks, restore the terminal.

The host stops writing to stdout/stderr directly after foreground handoff; logs flow to a tracing layer / event stream the active foreground consumes.

### Layer 3 — Extensions (`./extensions/<name>`)
One crate per feature. Current features become: `tracker-linear`, `tracker-files`, `runner-claude`, `runner-codex`, `runner-pi`, `runner-cli`, `runner-fake`, `orchestrator`, `dashboard`, and **`frontend-log`** (today's terminal log stream, extracted). Plus `extensions/example` kept green in CI.

### Assembly — `dist/`
The composition root. An **explicit plugin list** names the mix:
```rust
dar_host::run(plugins![
    tracker_linear::extension(),
    tracker_files::extension(),
    runner_claude::extension(),
    runner_codex::extension(),
    runner_pi::extension(),
    orchestrator::extension(),
    dashboard::extension(),
    frontend_log::extension(),
]).await
```
Changing the mix = edit this list + `dist/Cargo.toml`, rebuild. That recompile step is **accepted** (separate-crate development matters; runtime drop-in does not). `inventory` is **not** foundational; if ever reintroduced it must be domain-free (`{ id, construct }` only, no role enum) and hidden behind an SDK macro.

### Roles — descriptive, not encoded
"Capability extension" (registers a typed service: tracker, runner) vs "plain extension" (self-drives on bus + http + foreground) vs "host" remains a useful *mental* model, but it is **not a type in `host-api`**. The host knows only: extensions register services, subscribe to the bus, mount routes, and may provide a foreground. The orchestrator — itself an extension — resolves `tracker.use`/`runner.use` against the service registry.

## User Stories

### Extension authoring (the core driver)
1. As an extension author, I want to create a new extension by touching only its own crate plus the `dist` plugin list, importing `host-api` (and optionally one cap/api crate), so that I read zero host internals.
2. As an extension author, I want a `cargo dar new <name> --kind background|service|foreground` scaffold that compiles immediately, so that the cost to start is near zero.
3. As an extension author, I want a living `extensions/example` (config validation, `register()`, `start()`, typed publish/subscribe, graceful shutdown, data dir) kept green in CI, so that I copy a working reference, not folklore.
4. As an extension author, I want to register a tracker/runner as a typed service (`registrar.service::<dyn Tracker>("linear", …)`) with no `match` arm anywhere in shared code, so that core has zero per-backend knowledge.
5. As an extension author, I want the contract crates (`host-api`, `cap-*`, `orchestrator-api`) small, `0.x`, and additive (`#[non_exhaustive]`, defaulted trait methods), so that my extension rarely breaks on a bump.
6. As an extension author, I want to integrate via **typed** bus events (`bus.publish(DispatchRequested{…})`, `bus.subscribe::<RunSnapshot>()`), not stringly topics, so that the compiler catches my mistakes.
7. As an extension author, I want documented bus delivery semantics (bounded/unbounded, lossy/backpressured, ordering, slow-subscriber, shutdown), so that I don't reverse-engineer the orchestrator.
8. As an extension author, I want `ctx.data_dir(ext_id)` for my local state, so that persistence is a one-liner and stays inside the agent folder.

### Build & assembly
9. As an operator, I want a `dist` plugin list naming exact extensions, so that the binary contains only what I picked.
10. As an operator, I want to remove an extension by deleting one list line + its dep and rebuilding, so that it's gone and uncompiled.
11. As an operator, I want linked ≠ started: an extension compiled into the binary starts only when enabled/selected by config, so that a linked-but-unconfigured Discord ext with no token can't break startup.
12. As an operator, I want footprint held at the ALG-186 gate (~8.2 MB stripped binary, ~14.9 MB idle RSS, 0% idle CPU), so that the refactor doesn't regress.
13. As a maintainer, I want each crate to build and test independently in CI, so that a break is localized.

### Service registry & boot
14. As the host, I want to expose a generic typed service registry and know nothing about what a service *means*, so that new capability kinds never require editing `host-api`.
15. As the orchestrator, I want to resolve `tracker.use = linear` / `runner.use = claude` via `ctx.services().get_named::<dyn Tracker>(id)`, so that selection lives with the consumer, not the host.
16. As the host, I want two-phase boot (`register()` all → select foreground → `start()` background → hand off main thread), so that subscriptions and slots are resolved before any timer or task fires.
17. As an operator, I want `doctor` to validate only enabled/selected extensions (and assert configured ids resolve to a registered provider), so that the preflight is meaningful and fails loudly on a typo.

### Foreground ownership (the TUI requirement)
18. As an operator, I want a `foreground:` config selecting exactly one top-level owner of the terminal, so that I choose `logs` or `tui` declaratively.
19. As a foreground extension, I want a `Foreground::run(self, ctx, ExclusiveTerminal)` handoff giving me sole ownership of stdout/raw-mode/alt-screen/keyboard, so that I can render a TUI without the host corrupting it.
20. As the host, I want to stop writing logs directly to the terminal after handoff and route them to a tracing layer / log-event stream, so that the active foreground decides how logs appear.
21. As today's behavior, I want terminal log streaming extracted into `frontend-log` (the default foreground), so that the acid test passes: if the current log stream can't be an extension, the seam isn't real.
22. As a TUI author, I want to be just another `foreground` provider that replaces `frontend-log`, so that building the TUI needs no host change.
23. As the host, I want startup to fail if two extensions claim the foreground slot and none is selected-unique, so that ownership conflicts surface immediately.
24. As any extension, I want raw mode / alternate screen restored on shutdown **and panic**, so that a crash doesn't wreck the operator's terminal.
25. As an operator with no TTY (CI/daemon), I want explicit non-interactive foreground behavior, so that headless runs are well-defined.

### Tracker capability
26. As the orchestrator, I want to call `poll_candidates` / `fetch_states` / `fetch_terminal` / `fetch_one` on a `Tracker` without knowing the backend, so that I never grow a "which tracker?" branch.
27. As a tracker author, I want to translate my native API into the `Issue` shape and construct it via a builder (it's `#[non_exhaustive]`), so that the model can gain fields without breaking me.
28. As the orchestrator, I want `Tracker` read-only for issue state (only the narrow defaulted `park_issue_needs_human` safety write, erroring by default), so that the two-state invariant holds: the orchestrator NEVER writes issue state.
29. As a tracker author, I want `sort_candidates_locally()` defaulted false, so that API-ordered trackers (Linear) keep native order.
30. As a tracker author, I want optional metadata (rate-limit remaining, tracker-native fields via the `metadata` map) without the trait requiring it, so that the contract stays minimal.

### Runner capability
31. As the orchestrator, I want to spawn via the `Runner` trait without knowing claude/codex/pi/cli, so that dispatch is backend-agnostic.
32. As a runner author, I want my command/args/stdin/env/session-dir construction inside my crate, calling `runner-core` for supervision/line-pump/kill, so that backend quirks (claude `bypassPermissions --add-dir`, codex `app-server` sandbox flags) live with the backend and I don't re-implement process control.
33. As the orchestrator, I want exactly one runner active at a time, so that the cardinality rule holds.
34. As the dashboard/TUI, I want runner output normalized into log-row types (assistant/thinking/tool_call/tool_output/error/user) regardless of backend, so that any renderer is uniform.
35. As a runner author, I want the standard `AGENT_*` env contract, so that CLI/script runners keep working unchanged.

### Orchestrator (extension + public api crate)
36. As the orchestrator, I want to own the entire run-state model (`RunStatus`, history ring, SQLite run persistence, `logs/history.jsonl`), so that the host stays domain-free.
37. As the orchestrator, I want to expose `RunSnapshot`/`RunStatus`/`ControlMsg`/`RunRequested` via `orchestrator-api`, so that dashboard and TUI name them without importing my impl crate (no sibling-impl import).
38. As the orchestrator, I want to keep the tick order `reconcile → collect_finished → dispatch → publish_snapshots`, so that a just-finished run isn't stolen/mis-recorded.
39. As the orchestrator, I want to remain the **sole mutator** of run state (incl. `paused`), so that single-writer discipline survives.
40. As the orchestrator, I want to receive `ControlMsg` (Pause/Resume/Stop{run_id}/Cancel{run_id}) over one typed path (the bus) and apply to run state only — never issue state — so that control never crosses the two-state boundary.
41. As the orchestrator, I want retry classification preserved (normal+terminal → Succeeded; normal+active → 1s continuation, not counting vs max_retries; abnormal → exponential backoff capped 30 min up to max_retries → Failed), so that behavior is unchanged.
42. As the orchestrator, I want to publish `RunSnapshot` as a **retained** (watch-like) value, so that a dashboard/TUI starting late gets current state immediately, then updates.
43. As the orchestrator, I want to subscribe to `DispatchRequested`/`RunRequested`, so that a scheduler or channel can drive me without importing me.
44. As the orchestrator, I want finished runs persisted and reloaded on startup, so that cold-start-trusts-issue-files is unchanged.

### Dashboard (plain extension)
45. As the dashboard, I want to mount routes on the host HTTP server (not own the listener), so that the host owns the port and other extensions can mount too.
46. As the dashboard, I want to read retained `RunSnapshot` and send `ControlMsg` over the bus, so that I render read-only and request control without mutating.
47. As the dashboard, I want my askama templates compiled in my own crate, so that a template/field mismatch is a build error in the dashboard crate.
48. As the dashboard, I want HTMX self-polling into `#content` (not `<body>`) preserved, so that the UI keeps working.

### HTTP surface
49. As the host, I want a defined route-namespace + collision rule (and whether one extension may claim `/`, and whether HTTP can be disabled entirely), so that two extensions can't silently conflict.

### Future extensions enabled (not built here)
50. As a scheduler author, I want to publish either `DispatchRequested{reason: Schedule}` (wake the poll pass) or `RunRequested{source: Scheduler, prompt, …}` (create ad-hoc work), so that cron can either trigger normal dispatch or inject work — orchestrator stays sole run-state mutator.
51. As a scheduler author, I want `ctx.data_dir("scheduler")` for last-fired state, a cancellation token for my loop, and defined paused/catch-up/timezone semantics, so that cron behaves across restarts.
52. As a channel author (Discord/Telegram), I want to be a plain long-running extension that loads its own secrets, runs its websocket/webhook/poll loop, publishes typed `ChatInbound`, subscribes to `ChatOutbound`, and shuts down via the cancellation token, so that no channel logic leaks into the host.
53. As a channel author, I want a defined bridge from chat stimulus to work (publish `RunRequested` / `DispatchRequested` / `ControlMsg`), so that an inbound message actually causes useful behavior.

### Preserved invariants
54. As the host, I want all paths derived from one canonical agent root with `assert_contained` rejecting `..`/symlinks, so that a child cwd cannot escape the workspace root.
55. As a runner, I want containment asserted before spawn, so that the safety guarantee is unchanged.
56. As an operator, I want the agent self-contained (agent = folder; move folder = move agent), so that the tenet survives.
57. As an operator, I want startup errors from any extension surfaced via the existing HITL notifier, so that a misconfigured extension still pages me.

## Implementation Decisions

### Workspace layout (monorepo)
- Workspace members:
  - `crates/host-api`, `crates/cap-tracker`, `crates/cap-runner`, `crates/orchestrator-api`, `crates/runner-core`.
  - `crates/dar-host` (the host lib + `run()`).
  - `extensions/{tracker-linear,tracker-files,runner-claude,runner-codex,runner-pi,runner-cli,runner-fake,orchestrator,dashboard,frontend-log,example}`.
  - `dist/` — bin crate (`name = "dar"`), the composition root.
- The dependency graph equals the eventual multi-repo split, so extracting any crate to its own repo later is a `Cargo.toml` path change.

### host-api surface (domain-free)
- **Extension trait**: `id()`, `register(&self, &mut Registrar) -> Result<()>` (defaulted no-op), `start(&self, HostCtx) -> Result<RunningExtension>` (defaulted no-op).
- **Typed service registry**: `registrar.service::<dyn Trait>(id, Arc<impl Trait>)`; `ctx.services().get_named::<dyn Trait>(id)`. Type-erased internally; host never names a concrete service type.
- **Typed event bus**: publish/subscribe over typed payloads; **retained** topics (watch-like, latest-value + updates) vs broadcast. Delivery semantics documented in `host-api` docs (bounded, backpressure policy, ordering per-topic, slow-subscriber behavior, shutdown drain).
- **Foreground slot**: `registrar.foreground(id, factory)`; config selects one; `Foreground::run(self, ctx, ExclusiveTerminal)`. At most one; conflict / unresolved selection fails at boot.
- **HTTP mount**: extensions contribute routers; defined namespace + collision rule; optional `/` claim; HTTP disable-able.
- **Lifecycle**: shutdown/cancellation token; two-phase boot.
- **Paths**: canonical root + `assert_contained`; `ctx.data_dir(ext_id)`.
- **Config**: typed access by extension id; each extension declares + validates its own schema. `dotenv` loading stays host behavior; extensions read env/config via `ctx`.
- **NOT in host-api**: `Issue`, `RunStatus`, `Tracker`/`Runner`/`Channel`, any `Role` enum, raw `inventory` types, a shared "terminal service".

### cap-tracker
- `Tracker` trait = only orchestrator-called methods + defaulted `park_issue_needs_human` (errors by default) + defaulted `sort_candidates_locally`.
- `Issue` `#[non_exhaustive]` with builder/constructors (external crates can't struct-literal a non-exhaustive type — builder is mandatory), required fields limited to what orchestration/prompting/sorting use, everything else `Option` + a `metadata` map for tracker-native fields.
- Owns tracker provider registration helper.

### cap-runner / runner-core
- `cap-runner`: `Runner::spawn(SpawnParams) -> RunnerHandle`, `SpawnParams` (non-exhaustive/builder), `RunnerHandle`, `ExitKind`, `KillReason`, normalized output/log-row event types, `AGENT_*` constants.
- `runner-core`: process-group setup, `supervise`, line pump, `term_then_kill`, `wait_for_pids_dead`, ANSI strip, `classify_protocol_line`/`map_event_type`/`normalize_log_row`. Backend crates own only their command/args/stdin/env/session-dir + backend-specific tests.

### orchestrator-api
- Public payload/contract types: `RunSnapshot`, `RunStatus`, `ControlMsg` (Pause/Resume/Stop/Cancel), `RunRequested`, `DispatchRequested`, normalized log rows, history-row shape. No logic. This is how dashboard/TUI/scheduler/channels integrate without importing the orchestrator impl.

### Run-state ownership
- Host stays fully domain-free. `RunStatus`, history ring, SQLite run/event rows, `logs/history.jsonl` live in the **orchestrator** crate. Runner output flows over the bus; orchestrator (and any renderer) subscribe and persist. (`src/store.rs`, `src/state.rs` move into the orchestrator crate.)

### Registration & boot
- Explicit `plugins![…]` list in `dist`. No central backend `match`; `tracker::build` and `RunnerKind::parse` are deleted — selection is a registry lookup by configured id, with a boot assertion that configured ids resolved.
- Linked ≠ started: capability providers instantiate only when selected by a consumer; background/plain extensions start only when enabled; foreground starts only when selected.

### Foreground / logging
- `frontend-log` extension = default foreground, renders the current log stream. TUI = a future `frontend-tui` foreground provider. Host routes logs to a tracing layer / log-event stream the active foreground consumes; host does not print to the terminal post-handoff. Panic hook + shutdown restore terminal state.

### CLI
- `dar run --dir` boots the host. `doctor --dir` validates enabled/selected extensions via their config hooks + asserts id resolution. `init-workflow`/`export` (Linear-adjacent) register as host subcommands from the Linear tracker (or a small CLI extension).

### Channels — explicitly deferred
- No `cap-channel`, no host channel registry in this PRD. First channels ship as plain extensions over typed `ChatInbound`/`ChatOutbound` events; a `chat-api` contract is extracted only after one or two real bridges prove the shape.

## Testing Decisions

A good test asserts **external behavior through the public contract**, not internals — the trait, the registry, parsed output. Most existing tests port over with their crate (they already test arg/turn-request shape, classification, sort/backoff — behavior, not implementation).

Modules to test (all four groups, per decision):

1. **Contract crates**
   - `cap-runner` + `runner-core`: port existing `runner.rs` tests — claude arg construction, codex `app-server` + approval/sandbox/model/provider/effort/thinking flags + JSON-RPC turn request, pi turn request, CLI `AGENT_*` env, ANSI strip + `classify_protocol_line`/`normalize_log_row`, JSON-RPC `result` unwrap, stderr-always-error. Backend-specific arg tests live in the backend crates, not `cap-runner`.
   - `cap-tracker`: `Issue` builder + round-trip; defaulted `park_issue_needs_human` errors; `sort_candidates_locally` defaults false; `#[non_exhaustive]` construction via builder works from an external crate.
   - Prior art: current `#[cfg(test)] mod tests` in `src/runner.rs`.

2. **Orchestrator**
   - Tick order, candidate sort (priority null-last → created_at → identifier), `backoff_grows_then_caps`, continuation-vs-backoff classification, reconcile skip-if-finished, terminal-at-reconcile → Succeeded, missing/non-active → Cancelled, retained `RunSnapshot` publish.
   - Prior art: existing orchestrator unit tests.

3. **Host boot + registry**
   - Typed service registry: register `dyn Tracker`/`dyn Runner` and resolve by id; unregistered configured id fails loudly (replaces old `bail!`).
   - Two-phase boot ordering: `register()` completes for all before any `start()`; foreground selected before background tasks run.
   - **Foreground exclusivity**: exactly one selected; conflict/unresolved fails at boot; non-TTY path defined. (Highest-value new test — it guards the TUI seam.)
   - HTTP mount composes multiple routers; collision detected.
   - Fixtures: a fake plugin + `FakeRunner`.

4. **Trackers**
   - `tracker-files`: `./issues/*.md` frontmatter → `Issue`; active/terminal/blocked filtering in `poll_candidates`.
   - `tracker-linear`: GraphQL response → `Issue` mapping; rate-limit tracking; native order preserved (`sort_candidates_locally == false`).
   - Prior art: existing tracker tests.

Also: keep `extensions/example` compiling + a smoke test in CI — the executable proof that "creating an extension is simple."

Out of test scope: askama rendering (compile-time checked), bus transport internals (covered indirectly via orchestrator subscribe/retained-snapshot tests), channels (no implementor), the TUI itself (no implementor — but the `foreground` slot mechanism IS tested).

## Out of Scope

- **Runtime drop-in of extensions** (WASM, dlopen, subprocess). Rejected: separate-crate *development* is the goal; recompile is accepted. Compile-time composition only.
- **`inventory` as the mechanism.** Demoted to explicit `dist` list. (May return later, domain-free + macro-hidden, if the explicit list becomes painful.)
- **Multi-repo split.** Monorepo now; crate graph shaped for later extraction.
- **`cap-channel` and a host channel registry.** Deferred until real channel bridges exist.
- **Building the scheduler, channels, or TUI.** Out of scope as *features*. In scope: the seams that make them clean extensions — typed service registry, typed bus + `orchestrator-api` events (`DispatchRequested`/`RunRequested`/`ControlMsg`/retained `RunSnapshot`), the `foreground` slot, `ctx.data_dir`, scaffold generator, example extension.
- **Multiple concurrent trackers.** Cardinality stays 1 active.
- **A general UI/render framework.** The `foreground` slot is exactly-one top-level owner + an exclusive terminal handle — nothing more.
- **HTTP replacement abstraction.** Mount + collision rules only; full HTTP-surface ownership is a later singleton-slot case, just don't hard-code host ownership in a way that blocks it.
- **Behavior changes.** This is a packaging refactor: tick loop, retry math, sort, containment, permission flags, two-state invariant, dashboard UX must be equivalent. The one intentional behavior move: terminal log output is produced by `frontend-log` instead of inline host code (output should be equivalent in the `foreground: logs` default).
- **Agent-folder on-disk layout.** Unchanged.

## Further Notes

- **The single most important change** (both council rounds agreed): make today's terminal log stream the `frontend-log` extension occupying a `foreground` slot. If the current log stream can't be expressed as an extension, the seam isn't real and the TUI will never be clean.
- **Linchpin:** keep `host-api`, `cap-*`, and `orchestrator-api` tiny and additive. They are depended on by everyone; churn forces rebuilds and breakage. Treat their surface as semver-significant from `0.x` — `#[non_exhaustive]` + builders + defaulted methods so they can grow without breaking. **Do not call anything "frozen"**; stabilize only after scheduler/channels/TUI have been built against the architecture and proven the shapes.
- **Biggest risk to "creating an extension is simple"** is the bus becoming undocumented, stringly-typed integration folklore — authors guessing topic names and payload shapes by reading orchestrator source. Mitigations, cheapest-first: (1) typed events in owning API crates, not raw strings; (2) documented delivery semantics; (3) `extensions/example` green in CI; (4) the `cargo dar new` scaffold.
- **Two-state invariant is the thing most likely to be silently broken** by a careless split. `Tracker` stays read-only for issue state; run state stays orchestrator-owned with the orchestrator as sole mutator; control flows as `ControlMsg` requests, never direct mutation. Call this out in review of every extracted crate.
- **Footprint:** hold the ALG-186 gate. Static composition adds negligible runtime cost; re-run the pilot benchmark runbook after the split. Footprint is dominated by the LLM child, not the runtime.
- **Migration order (each step keeps `cargo test --release` green):**
  1. Extract `cap-tracker`, `cap-runner`, `runner-core` with no behavior change; monolith depends on them.
  2. Introduce `host-api` (services, bus, lifecycle, paths) + `dar-host`; leave a thin `dist` bin.
  3. Add the typed service registry + explicit `plugins![]`; delete `tracker::build` / `RunnerKind::parse`.
  4. Move trackers, then runners, into `extensions/`.
  5. Add the `foreground` slot; extract `frontend-log`; stop host-direct terminal writes; add panic/shutdown terminal restore.
  6. Extract `orchestrator-api`; move the orchestrator + run-state/store into `extensions/orchestrator`; rewire dashboard ↔ orchestrator over the bus with retained `RunSnapshot` + typed `ControlMsg`.
  7. Add `extensions/example` + the scaffold generator; document bus delivery semantics.
- **Scheduler decision to make at build time, not now:** `DispatchRequested` (wake poll) vs `RunRequested` (inject work). Implement the one the first scheduler needs; both event types can live in `orchestrator-api` from the start so the contract is ready.
