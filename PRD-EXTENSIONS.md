# PRD — Extension Architecture Refactor

> Status: draft · needs-triage
> Companion visuals: `extension-design-visual.html`, `extension-architecture.html`
> Supersedes the monolith layout described in `PRD.md` (the behavior contracts in `PRD.md` still hold; only the packaging changes).

## Problem Statement

Agentropy today is one Rust binary. Every capability — the Linear/file trackers, the claude/codex/pi/cli runners, the orchestrator tick loop, the dashboard — lives in `src/` and compiles together. That was right for v0, but it now blocks the next phase:

- I cannot develop a new runner, tracker, or a brand-new feature (scheduler, Discord channel, web chat) **in its own repo** without touching core.
- Adding a capability means editing core dispatch code (`tracker::build`, `RunnerKind::parse`) — a `match` arm per backend. Core knows every concrete backend by name.
- There is no seam for feature types that aren't tracker/runner at all (scheduler, channels, sinks). The architecture only has slots for what v0 happened to need.
- The core conflates "the plumbing every feature needs" with "the orchestration domain." The binary *is* the orchestrator, so there's no way to ship a build that does something other than orchestrate.

I want the core stripped to the bare minimum: a host that knows **zero** domain concepts (not even "Issue"), and every current feature re-expressed as an extension. New extension types must be addable without re-architecting.

## Solution

Split the monolith into a **cargo workspace** (monorepo for now) with three layers:

1. **Contract crates** — tiny, stable, rarely-changing:
   - `host-api`: generic plumbing contract (lifecycle, config, event bus, http mount, terminal, secrets/env, async runtime handle, the `inventory` registry types). Domain-free.
   - `cap-tracker`: the `Tracker` trait + the frozen `Issue` model.
   - `cap-runner`: the `Runner` trait + spawn/handle/exit types.
   - `cap-channel`: the `Channel` trait (scaffold; no v0 code fills it yet).
2. **The host** — `agentropy-host`: the minimal binary plumbing. CLI, config load, logging, paths, dotenv, the event bus implementation, the http server it mounts extensions onto, and the boot sequence that reads the `inventory` registry and starts every linked extension. Knows no domain.
3. **Extensions** — one crate per feature under `./extensions/`. Each self-registers via `inventory::submit!`. Current features become: `tracker-linear`, `tracker-files`, `runner-claude`, `runner-codex`, `runner-pi`, `runner-cli`, `runner-fake`, `orchestrator`, `dashboard`.

A thin `agentropy-dist` bin crate names the chosen mix in its `Cargo.toml`. `cargo build` links only those extensions; linking auto-registers them; the result is one static binary containing exactly what was picked. Changing the mix = edit `dist/Cargo.toml`, rebuild. That recompile step is **accepted** (explicit user decision — separate-repo development matters, runtime drop-in does not).

**Roles** (the deciding question: *does anything call into you through a typed contract?*):
- **Capability extension** — YES. Implements a `cap-*` trait; the host or a consumer invokes it. `tracker-linear`, `runner-codex`.
- **Plain (feature) extension** — NO. Self-drives on `host-api` only; uses the bus + http mount + terminal. `dashboard`, future `scheduler`, future channels' glue.
- **Host** — the plumbing. Knows no domain, offers services to both.

The **orchestrator is a plain extension** that *consumes* `cap-tracker` + `cap-runner`. Capability crates therefore flow two ways: providers **implement** them, consumers **call** them. Plain features touch neither.

The **event bus is the decoupling spine**: no extension imports a sibling. A future scheduler publishes `schedule.fired`; the orchestrator happens to subscribe. Swap either side, the bus doesn't care.

## User Stories

### Extension authoring (the core driver)
1. As an extension author, I want to write a new tracker in its own crate that depends only on `cap-tracker` + `host-api`, so that I can develop and version it without editing core.
2. As an extension author, I want to register my extension with a single `inventory::submit!` line, so that I never write per-extension wiring in core.
3. As an extension author, I want to add a runner backend (e.g. `runner-gemini`) without touching any `match` arm in shared code, so that core has no per-backend knowledge.
4. As an extension author, I want a brand-new feature type (scheduler, channel, sink) to depend on `host-api` alone when it has no shared call-in signature, so that I'm not forced to invent a fake capability trait.
5. As an extension author, I want the contract crates (`host-api`, `cap-*`) to be small and stable, so that my extension rarely breaks on a core bump.
6. As an extension author, I want my extension to declare which events it publishes and subscribes to, so that I can integrate via the bus without importing other extensions.

### Build & assembly
7. As an operator, I want a `dist` crate whose `Cargo.toml` lists the exact extensions I want, so that the binary contains only what I picked.
8. As an operator, I want to remove an extension by deleting one dependency line and rebuilding, so that it's gone and uncompiled — no dead code in the binary.
9. As an operator, I want the built binary footprint to stay in the ~8–15 MB range with idle RSS near today's ~14.9 MB, so that the refactor doesn't regress the footprint gate.
10. As an operator, I want `cargo build --release` from the workspace root to produce `agentropy` exactly as today, so that my deploy story is unchanged.
11. As a maintainer, I want every contract and extension crate to build and test independently in CI, so that a break is localized to its crate.

### Tracker capability
12. As the orchestrator, I want to call `poll_candidates()` / `fetch_states()` / `fetch_terminal()` / `fetch_one()` on a `Tracker` without knowing whether it's Linear, GitHub, or files, so that I never grow a "which tracker?" branch.
13. As a tracker author, I want to translate my native API into the frozen `Issue` shape (`identifier`, `title`, `state`, `priority`, `assignees`, …), so that all trackers hand back the same container.
14. As the orchestrator, I want the `Tracker` trait to stay **read-only for issue state** (with only the narrow `park_issue_needs_human` safety write), so that the two-state-layer invariant holds: the orchestrator NEVER writes issue state.
15. As a tracker author, I want to declare whether the orchestrator should apply its local candidate sort (`sort_candidates_locally`), so that API-ordered trackers (Linear) keep native order.
16. As a tracker author, I want to optionally report `rate_limit_remaining`, so that the dashboard can surface it without the trait requiring it.

### Runner capability
17. As the orchestrator, I want to spawn a runner via the `Runner` trait without knowing whether it's claude/codex/pi/cli, so that dispatch code is backend-agnostic.
18. As a runner author, I want to own my command/args/stdin-payload/env/session-dir construction inside my crate, so that backend quirks (claude's `--permission-mode bypassPermissions --add-dir`, codex's `app-server` + sandbox flags) live with the backend.
19. As the orchestrator, I want exactly one runner active at a time (claude **or** codex), so that the agent runs a single runner per the cardinality rule.
20. As a runner author, I want my child spawned in its own process group with the SIGTERM→grace→SIGKILL lifecycle handled by shared code, so that I don't re-implement supervision.
21. As the dashboard, I want runner output normalized into log-row types (assistant/thinking/tool_call/tool_output/error/user) regardless of backend, so that the UI renders uniformly.
22. As a runner author, I want to receive the standard `AGENT_*` env contract, so that scripts/CLI runners keep working unchanged.

### Channel capability (scaffold only)
23. As a channel author, I want a `cap-channel` trait so that I can later build Discord/Slack/Telegram bridges that the host treats uniformly.
24. As an operator, I want all configured channels to be **live simultaneously** (N concurrent), so that the agent is reachable from Discord, Slack, and Telegram at once — channels coexist, they do not replace each other.
25. As the host, I want a channel registry that fans inbound channel messages onto the bus and routes outbound replies back to the originating channel by id, so that channels never call each other.

### Orchestrator (plain extension owning run state)
26. As the orchestrator, I want to own the entire run-state model (`RunStatus`: Running/RetryQueued/Cancelled/Failed/Succeeded, the history ring, SQLite run persistence), so that the host stays domain-free.
27. As the orchestrator, I want to keep the tick order `reconcile → collect_finished → dispatch → publish_snapshots`, so that a just-finished run isn't stolen and mis-recorded.
28. As the orchestrator, I want to remain the **sole mutator** of run state (including the `paused` flag), so that single-writer discipline survives the refactor.
29. As the orchestrator, I want to receive `ControlMsg` (Stop/Pause/Resume) from the dashboard over a channel and apply them to run state only — never issue state — so that control never crosses the two-state boundary.
30. As the orchestrator, I want retry classification preserved (normal+terminal → Succeeded; normal+active → 1s continuation not counting against max_retries; abnormal → exponential backoff capped at 30 min up to max_retries → Failed), so that behavior is byte-for-byte the same.
31. As the orchestrator, I want to subscribe to bus events (e.g. a future `schedule.fired`) to trigger a dispatch pass, so that other extensions can drive me without importing me.
32. As the orchestrator, I want to persist finished runs to `logs/history.jsonl` / SQLite and reload on startup, so that the cold-start-trusts-issue-files behavior is unchanged.

### Dashboard (plain extension)
33. As the dashboard, I want to mount my routes on the host's http server rather than owning the listener, so that the host owns the port and other extensions can mount too.
34. As the dashboard, I want to read run-state snapshots published by the orchestrator (read locks / snapshot channel), so that I render without mutating.
35. As the dashboard, I want my askama templates compiled in my own crate, so that a template/field mismatch is a build error in the dashboard crate, not core.
36. As the dashboard, I want to keep self-polling via HTMX into `#content` (not `<body>`), so that the existing UI keeps working.

### Path containment & safety (preserved invariants)
37. As the host, I want all paths to derive from one canonical agent root with `assert_contained` rejecting `..`/symlink escapes, so that a child cwd cannot escape the workspace root.
38. As a runner, I want containment asserted before spawn, so that the safety guarantee is unchanged after the split.

### Operability
39. As an operator, I want `agentropy doctor --dir` to still preflight config/template/tracker, so that the diagnostic survives — implemented by querying the registry for which extensions are linked + their config validation hooks.
40. As an operator, I want the agent to remain self-contained (agent = folder; move folder = move agent), so that the tenet survives the refactor.
41. As an operator, I want startup errors from any extension surfaced through the existing HITL notifier path, so that a misconfigured extension still pages me.

## Implementation Decisions

### Workspace layout (monorepo)
- Cargo workspace at repo root. Members:
  - `crates/host-api`, `crates/cap-tracker`, `crates/cap-runner`, `crates/cap-channel` — contract crates.
  - `crates/agentropy-host` — the host (library + the boot routine).
  - `extensions/tracker-linear`, `extensions/tracker-files`, `extensions/runner-claude`, `extensions/runner-codex`, `extensions/runner-pi`, `extensions/runner-cli`, `extensions/runner-fake`, `extensions/orchestrator`, `extensions/dashboard`.
  - `dist/` — the bin crate (`name = "agentropy"`) that depends on host + the chosen extensions.
- For now all live in one repo. The dependency graph is identical to the eventual multi-repo split, so extracting any crate to its own repo later is a path change in `Cargo.toml`, nothing else.

### Registration mechanism
- `inventory` crate for compile-time auto-registration. Each extension submits a `Plugin` descriptor: `{ id, role, make }` where `role ∈ {Tracker, Runner, Channel, Plain}` and `make` is a constructor closure.
- The host reads the registry at boot, filters/constructs by role + config, and starts each. There is **no central `match` over backend names** anywhere — `tracker::build`/`RunnerKind::parse` are deleted; selection is "find the registered plugin whose `id` matches the configured `use:`".
- Linking an extension (a `dist` dependency) is the entire registration cost.

### host-api surface (domain-free)
- Lifecycle: a `start(ctx) -> Result<RunningExtension>` style hook each extension implements; graceful shutdown via a shared watch signal.
- Config: typed access to the agent folder config + per-extension config sections (extensions declare and validate their own schema; host doesn't know the fields).
- Event bus: publish/subscribe over typed topic strings (`schedule.fired`, `run.finished`, channel inbound/outbound). Producers and consumers never reference each other.
- HTTP mount: extensions register routers onto one host-owned axum server + port.
- Terminal, secrets/env (dotenv load), async runtime handle.
- The `inventory` `Plugin` type + role enum.
- **No `Issue`, no `RunStatus`, no tracker/runner knowledge** in host-api.

### cap-tracker
- Moves `Issue` (from `src/domain.rs`) and the `Tracker` trait (from `src/tracker/mod.rs`) verbatim. Trait stays **read-only for issue state** + the narrow `park_issue_needs_human` default-erroring safety write. Keeps `sort_candidates_locally` and `rate_limit_remaining` default methods.
- `Issue` is the frozen domain model — the single shape all trackers translate into.

### cap-runner
- Moves the `Runner`-facing types: a public `Runner` trait (`spawn(SpawnParams) -> RunnerHandle`), `SpawnParams`, `RunnerHandle`, `ExitKind`, `KillReason`. The per-backend `RunnerSpec` structs move **out** to each runner extension; the shared supervision/line-pump/`term_then_kill`/process-group/timeout logic stays in `cap-runner` (or a small shared `runner-core` helper the extensions call) so backends don't re-implement it.
- The `AGENT_*` env contract and protocol-line classification (`classify_protocol_line`, `map_event_type`, …) are shared helpers, not per-backend.

### cap-channel (scaffold)
- New `Channel` trait: `send(outbound)`, an inbound stream the host fans onto the bus, an `id`. No v0 implementor; exists so the host's channel registry + cardinality-N model is real and future bridges slot in.

### Host run-state decision
- **Host stays fully domain-free.** Run state — `RunStatus`, the history ring, the SQLite run/event rows, `logs/history.jsonl` — lives entirely in the **orchestrator extension**. The host does NOT provide a generic store.
- Runner output events flow over the **bus**; the orchestrator (and dashboard) subscribe and persist. This keeps persistence an orchestrator concern, consistent with "orchestrator owns run state."
- Consequence: `src/store.rs`, `src/state.rs` (AppState/RunStatus/HistoryRing/EventRing) move into the orchestrator crate. The dashboard reads run-state snapshots the orchestrator publishes (read-only).

### Orchestrator extension
- Consumes `cap-tracker` + `cap-runner`. Holds the tick loop, run-state, control-channel handling, HITL notify, SQLite persistence.
- Tick order, sort (priority asc null-last → created_at asc → identifier), retry/backoff/continuation classification, reconcile skip-if-finished rule — all preserved exactly.
- Subscribes to the bus for external dispatch triggers (future scheduler); publishes `run.*` snapshots/events for the dashboard.

### Dashboard extension
- Owns its askama templates + `src/dashboard/` views. Mounts routes on the host http server. Reads orchestrator snapshots; sends `ControlMsg` over the control channel exposed by the orchestrator (via bus or a host-brokered control topic — control mutates run state only).
- Keeps minijinja for `WORKFLOW.md` prompt rendering inside whichever crate renders prompts (orchestrator-side, since prompt rendering feeds dispatch).

### CLI / boot
- `agentropy run --dir` and `agentropy doctor --dir` stay. `run` boots the host, host starts all registered extensions. `doctor` asks the registry which extensions are linked and runs each extension's config-validation hook.
- `init-workflow` / `export` are tracker-Linear-adjacent; they move with the Linear tracker extension or a small CLI extension, exposed as host subcommands the extension registers.

### What core no longer knows
- No backend names, no `Issue`, no `RunStatus`, no askama, no SQLite. Delete `tracker::build` and `RunnerKind::parse` dispatch tables. Selection is registry lookup by configured id.

## Testing Decisions

A good test asserts **external behavior through the public contract**, not internals. Tests target the crate boundary (the trait, the registry, the parsed output), not private structs. Most existing tests port over with their crate; they already test behavior (arg/turn-request shape, classification, sort/backoff) rather than implementation.

Modules to test (all four groups, per decision):

1. **Contract crates**
   - `cap-runner`: port the existing `runner.rs` tests — claude arg construction (`-p --permission-mode bypassPermissions --add-dir [--model]`), codex `app-server` + approval/sandbox/model/provider/effort/thinking flags + JSON-RPC turn request, pi turn request, CLI `AGENT_*` env, ANSI strip + `classify_protocol_line` / `normalize_log_row` row typing, JSON-RPC `result` unwrapping, stderr-always-error. These are pure functions over `SpawnParams` — ideal deep-module tests.
   - `cap-tracker`: `Issue` parse/translation round-trips; default-method behavior (`park_issue_needs_human` errors by default, `sort_candidates_locally` defaults false).
   - Prior art: the current `#[cfg(test)] mod tests` in `src/runner.rs` (already behavior-level).

2. **Orchestrator**
   - Tick-loop order, candidate sort (priority null-last → created_at → identifier), backoff growth-then-cap (`backoff_grows_then_caps`), continuation-vs-backoff classification, reconcile skip-if-finished, terminal-at-reconcile → Succeeded, missing/non-active → Cancelled.
   - Prior art: existing orchestrator unit tests (`cargo test --release backoff_grows_then_caps`).

3. **Host boot + registry**
   - `inventory` registration discovers a test plugin; boot constructs only plugins whose `id` matches config; an unregistered configured id is a clear error (replacing the old `bail!("unsupported tracker.use …")`).
   - Config wiring: per-extension config section parsed + validated; http mount composes multiple routers without collision.
   - New tests (no prior art — this is the new seam). Use a `FakeRunner`/fake plugin as the registry fixture.

4. **Trackers**
   - `tracker-files`: `./issues/*.md` frontmatter → `Issue` shape; active/terminal/blocked filtering in `poll_candidates`.
   - `tracker-linear`: GraphQL response → `Issue` mapping; rate-limit-remaining tracking; native order preserved (`sort_candidates_locally == false`).
   - Prior art: existing tracker tests in `src/tracker/`.

Out of test scope: askama template rendering (compile-time checked), the bus transport itself (covered indirectly by orchestrator subscribe tests), channel extension (no implementor yet).

## Out of Scope

- **Runtime drop-in of extensions** (WASM, dlopen, subprocess plugins). Explicitly rejected: separate-repo *development* is the goal; the recompile step is accepted. Compile-time traits + `inventory` only.
- **Multi-repo split.** Stays a monorepo for now. The crate graph is shaped so extraction is later a path change, but actually splitting repos is not in this PRD.
- **New feature extensions** (scheduler, Discord/Slack/Telegram channels, web chat, TUI, admin dashboard). Out of scope as *features*; the host-api + `cap-channel` + bus seams that make them possible ARE in scope. The scheduler mirroring `~/code/aihub/packages/extensions/scheduler` is the intended first new extension *after* this refactor.
- **Multiple concurrent trackers.** Cardinality stays 1 tracker active (N is designed-for, not built).
- **Behavior changes.** This is a packaging refactor. The tick loop, retry math, sort, containment, permission flags, two-state invariant, and dashboard UX must be byte-for-byte equivalent. Any behavior change is a separate PRD.
- **Changing the agent-folder on-disk layout** (`issues/`, `workspaces/`, `logs/`, `WORKFLOW.md`, `agent.yaml`). Unchanged.

## Further Notes

- **Linchpin:** keep `host-api` and each `cap-*` tiny and stable. They are the crates everyone depends on; churn there forces every extension to rebuild and risks breakage. Treat their public surface as semver-significant from day one.
- **Footprint:** the refactor must hold the ALG-186 gate — ~8.2 MB stripped binary, ~14.9 MB idle RSS, 0% idle CPU. `inventory` + workspace splitting add negligible runtime cost (registration is a static slice walk at boot). Re-run the pilot benchmark runbook after the split to confirm no regression. The footprint is dominated by the LLM child process, not the runtime — this remains true.
- **Migration order (suggested):** (1) extract contract crates `host-api`/`cap-tracker`/`cap-runner` with no behavior change, monolith still depends on them; (2) carve the host out, leaving a thin `dist` bin; (3) move trackers, then runners, then dashboard into `extensions/`; (4) move the orchestrator + run-state/store last (largest blast radius); (5) introduce the bus and rewire dashboard↔orchestrator over it; (6) delete the dead `match` dispatch. Each step keeps `cargo test --release` green.
- **`inventory` caveat:** registration requires the extension crate to actually be linked. A crate listed in `dist/Cargo.toml` but never referenced can be dropped by the linker. Mitigate with the standard `inventory` pattern (the `dist` crate's reachable code path forces linkage), and add a host boot-time assertion that the configured `use:` ids resolved to a registered plugin — otherwise fail loudly (the new equivalent of today's `bail!`).
- **Two-state invariant is the thing most likely to be silently broken** by a careless split. The `Tracker` trait must remain read-only for issue state, and run state must remain orchestrator-owned with the orchestrator as sole mutator. Call this out in review of every extracted crate.
