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

Real ids in the shipped mix: runners `dyn Runner` under `pi` / `claude` /
`claude-code` / `codex` / `cli` / `fake`; trackers `dyn TrackerFactory` under
`files` / `linear`; chat backends `dyn ChatBackend` under `pi`. Id and type
form the key, so `dyn ChatBackend @ "pi"` coexists with `dyn Runner @ "pi"`.
(`register` is an alias for `service`.)

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
