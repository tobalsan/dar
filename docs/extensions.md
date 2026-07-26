# Authoring extensions

How to build any kind of dar extension. Read this top-to-bottom once;
`extensions/example/src/lib.rs` is the living reference. For enabling and
configuring an already-written extension, see
[Enabling & configuring extensions](#enabling--configuring-extensions) — this
guide is about *authoring*.

## Enabling & configuring extensions

The codebase is a cargo workspace: a domain-free host (`crates/dar-host`)
plus small contract crates (`crates/host-api`, `crates/cap-tracker`,
`crates/cap-runner`, `crates/cap-chat`, `crates/orchestrator-api`), with
features living as one crate each under `extensions/`. The binary is assembled
from an explicit plugin list in the composition root (`dist/`). Extensions
import `host-api` (and optionally one cap/api crate) and read zero host
internals.

Editing `dist/` is the **build A** path (see the [README](../README.md)
quickstart). In **build B**, each agent
gets its own composition crate (`.dar/`) and can add agent-local
extensions under its own `extensions/` folder — scaffold one with
`cargo dar new my-extension --kind background` (or `service` /
`foreground`). See [Self-contained agents (build B)](self-contained-agents.md).

Linked is not the same as enabled. Runner and tracker extensions only
*register* named services. `runner.use` is always required and selects the
agent harness/model backend:

```yaml
runner:
  use: pi               # pi | codex | cli | fake
```

The issue loop itself is a `WORKFLOW.md` concern, not `agent.yaml`: tracker
(`tracker.kind`, `tracker.projects`, states, …), polling, and workspace config
all live in `WORKFLOW.md` frontmatter — see [WORKFLOW.md](workflows.md) below.
Background extensions in `plugins![]` (orchestrator, dashboard, frontend-log)
start unconditionally. With no resolved `WORKFLOW.md`, or one whose
frontmatter is missing `tracker.kind` or non-empty `active_states`/
`terminal_states`, the orchestrator starts in passive mode: no issue loop, no
tracker required. The foreground extension — the one that owns terminal
output — is selected per agent via the top-level `foreground:` key in
`agent.yaml` (default `"logs"`, the frontend-log extension; `"tui"` selects the
[terminal UI](chat.md)). An unknown id causes a clean boot
error and exit 1.

Per-extension config is passed via the top-level `extensions:` map in
`agent.yaml`, keyed by extension id. The host delivers each value to the
matching extension via `ConfigStore`. Missing section = empty config.

```yaml
foreground: logs          # optional; default "logs"

extensions:
  dashboard:
    port: 7878
```

## What an extension is

An extension is one crate under `extensions/`. It depends on `host-api` plus at
most one *capability* crate (`cap-tracker`, `cap-runner`, `cap-chat`, ...);
shared bus-payload crates like `orchestrator-api` don't count against that limit
(the dashboard and `tui` both import it alongside their other deps). It reads
zero host internals — all integration goes through the contracts in
`crates/host-api/src/lib.rs`. A crate is *linked* when it's in the `plugins![]`
list (`dist/src/main.rs`); that is not the same as *enabled* — runner services
are selected by required `agent.yaml` `runner.use`, tracker services are selected
only for issue-loop agents that configure the optional orchestrator/tracker/workspace trio, and the foreground only runs when `foreground:` selects it.

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
cargo dar new my-extension --kind background   # or service | foreground
```

This writes `extensions/my-extension/{Cargo.toml,src/lib.rs}` with a compiling
skeleton for the chosen kind, then prints the two wiring lines. Wire it into the
shipped binary (full detail in
[Enabling & configuring extensions](#enabling--configuring-extensions) above):

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

Boot lifecycle order (`crates/dar-host/src/lib.rs`, `boot_inner`):
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

### Chat surfaces (share the agent identity)

Any extension that opens a `ChatBackend` session for **agent-facing** chat —
IRC, Telegram, a future web/Discord bridge, and the stock TUI — must talk to the
same agent identity the TUI uses. Do not hand-build `ChatSessionParams`: model,
provider, the agent's system prompt, and the host tool bridge all come from
retained bus state, and copying that wiring by hand is how a surface silently
drifts (the bug that left IRC replying `(no response)`: it opened sessions
without the retained `system.context` and the provider rejected them).

Go through the two shared SDK helpers in `dar_extension_sdk::chat`:

```rust
use dar_extension_sdk::chat::{agent_session_params, resolve_agent_backend};

async fn open_session(ctx: &StartCtx, session_dir: &Path, configured: Option<&str>)
    -> anyhow::Result<Box<dyn ChatSession>>
{
    // Same backend precedence as the TUI: an explicit config override wins
    // (and fails at open time if misspelled); else follow the orchestrator's
    // selected runner when it is registered as a chat backend; else the stock
    // "pi" fallback.
    let backend_id = resolve_agent_backend(ctx, configured);
    let backend = ctx.host.services.get::<dyn ChatBackend>(&backend_id)?;

    // Builds ChatSessionParams from retained state: model/provider from the
    // RunSnapshot, the retained `system.context` as system_prompt, the host
    // tool bridge, and the agent root cwd. Returns a builder so a surface can
    // layer on its own bits (e.g. `.command(..)`, `.resume_session_id(..)`)
    // before `.build()`.
    let params = agent_session_params(ctx, session_dir).build();

    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    Ok(backend.open(params, tx).await?)
}
```

`agent_session_params` degrades gracefully: an absent or empty `system.context`
topic yields no system prompt (the session opens exactly as before), and a
missing `RunSnapshot` leaves model/provider unset. The retained bus contract
(`SystemContext` + `SYSTEM_CONTEXT_TOPIC`) is published by the substrate
`system-context` extension and re-exported from `dar_extension_sdk::chat`, so a
surface never needs to depend on a `publish = false` in-tree crate to read it.

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
`agent.yaml`, so a binary without the section behaves exactly as before. The
scheduler is the stock opt-in example — see [Scheduler](scheduler.md).

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
- **Boot-level tests** in `crates/dar-host/src/lib.rs` that boot a list of
  fake extensions and assert lifecycle ordering and foreground selection.

Capability-contract behavior (the `Runner` / `Tracker` / `ChatBackend` builders
your service must satisfy) is exercised in `crates/cap-runner/tests/builder.rs`,
`crates/cap-tracker/tests/builder.rs`, and `crates/cap-chat/tests/builder.rs` —
mirror those when implementing a service.

Run a single test by name:

```bash
cargo test --release smoke_register_start_publish_subscribe_and_shutdown
```
