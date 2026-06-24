# PRD — Fully Self-Contained Agents (FSC)

> Status: settled (council-revised) · iteration cycle
> Supersedes nothing — PRD-EXTENSIONS.md is fully shipped. This PRD is the next cycle.
> This PRD captures multi-LLM council consensus; decisions are stated, not explored.

## Problem Statement

The extension architecture refactor (PRD-EXTENSIONS.md) shipped: domain-free host, contract crates, explicit `plugins![…]` list in `dist/`, every feature its own crate. The remaining gap is **who owns the binary**.

Today, building *any* agent binary means editing the repo's `dist/Cargo.toml` and `dist/src/main.rs`. Supporting 10 agents with 10 different extension mixes means 10 manual edits to those files, plus the binary lives outside the agent folder. Moving an agent to a new machine means rebuilding from the repo, not from the folder.

Two specific problems:

- **No per-agent composition.** There is no way for an agent to have its own local extension crates and produce its own binary without touching shared repo files.
- **No self-update.** An agent cannot rebuild itself when its `agent.yaml` or local extensions change. A running agent must depend on a human or an external tool to update and restart it.

The decisive new tenet: **an agent must own its own build and lifecycle**. "Move folder = move agent" must extend to "agent updates itself, with no external tool installed after bootstrap."

Footprint scope: binary size only. Runtime RSS is dominated by the LLM child process — accepted. Footprint = "don't bloat the binary."

## Solution

A **persisted per-agent composition crate** inside the agent folder (`.dar/`), plus build and self-update logic **embedded in the shipped binary** via an `dar-cli` library crate. No external build tool is required after the initial bootstrap.

### Layer 1 — `dar-cli` library (unblocking refactor)

Refactor `dist/src/main.rs` into a reusable library crate `crates/dar-cli` that exposes a `run(plugins)` (or `run_with_extra(plugins)`) entry point, holding all boot wiring, HITL notifier setup, `doctor` validation, and CLI subcommand dispatch currently inlined in `dist`. The `dist` bin becomes a thin caller:

```rust
// dist/src/main.rs (after refactor)
#[tokio::main]
async fn main() {
    dar_cli::run(plugins![
        tracker_linear::extension(),
        tracker_files::extension(),
        runner_claude::extension(),
        // ...
    ]).await
}
```

Each agent's `.dar/src/main.rs` becomes equally thin — it calls the same `dar_cli::run`. Self-update, `doctor`, and `dar build` all live in `dar-cli`, so agents get them for free by depending on it.

### Layer 2 — Per-agent composition crate (`.dar/`)

```
my-agent/
├── agent.yaml
├── extensions/
│   └── <local-ext>/          # agent-local extension crate
│       ├── Cargo.toml        # empty [workspace] + metadata marker
│       └── src/lib.rs        # pub fn extension() -> Box<dyn Extension>
├── bin/
│   ├── dar             # current running binary
│   └── dar.prev        # rollback binary (if any)
└── .dar/               # persisted composition crate (committed)
    ├── Cargo.toml            # [dependencies]: stock via git-rev/registry; local via relative path
    ├── Cargo.lock            # committed; cargo build --locked for determinism
    ├── rust-toolchain.toml   # pins rustc channel
    └── src/main.rs           # generated thin composition entrypoint
```

The composer (embedded in `dar-cli`) regenerates `.dar/Cargo.toml` and `.dar/src/main.rs` whenever the extension set changes, then invokes `cargo build --release --locked` out of process.

### Layer 3 — Extension discovery

Each local extension crate declares a metadata marker so the composer can discover its crate identity and factory expression without parsing Rust source:

```toml
# extensions/<name>/Cargo.toml
[package]
name = "my-ext"
version = "0.1.0"
edition = "2021"

[workspace]  # empty — keeps cargo from treating it as a workspace member of the repo

[package.metadata.dar]
factory = "my_ext::extension"

[dependencies]
host-api = { git = "https://github.com/tobalsan/dar", rev = "abc1234" }
```

The scaffold generator (`cargo dar new <name>`) emits this marker automatically. Discovery: scan `extensions/*/Cargo.toml`, read `[package.metadata.dar] factory`, derive the crate ident from `package.name`. No `cargo metadata` invocation required for the common case; `cargo metadata` available as a fallback.

### Generated composition entrypoint

```toml
# .dar/Cargo.toml (generated — do not hand-edit)
[package]
name = "dar-agent"
version = "0.1.0"
edition = "2021"

[workspace]  # isolated from the repo workspace

[dependencies]
dar-cli   = { git = "https://github.com/tobalsan/dar", rev = "abc1234" }
tracker-files   = { git = "https://github.com/tobalsan/dar", rev = "abc1234", optional = true }
runner-claude   = { git = "https://github.com/tobalsan/dar", rev = "abc1234", optional = true }
orchestrator    = { git = "https://github.com/tobalsan/dar", rev = "abc1234", optional = true }
dashboard       = { git = "https://github.com/tobalsan/dar", rev = "abc1234", optional = true }
frontend-log    = { git = "https://github.com/tobalsan/dar", rev = "abc1234", optional = true }
my-ext          = { path = "../extensions/my-ext" }

[features]
default = ["tracker-files", "runner-claude", "orchestrator", "dashboard", "frontend-log"]
```

```rust
// .dar/src/main.rs (generated — do not hand-edit)
#[tokio::main]
async fn main() {
    dar_cli::run(plugins![
        tracker_files::extension(),
        runner_claude::extension(),
        orchestrator::extension(),
        dashboard::extension(),
        frontend_log::extension(),
        my_ext::extension(),
    ]).await
}
```

### Stock extension subsetting

Stock extensions are **optional feature-gated deps** of the composition crate. The `agent.yaml` `runner.use`/`tracker.use`/`foreground` selection is mapped to Cargo features at compose time, so a minimal agent links only what it needs. This is orthogonal to the codegen — features control which crates are compiled in; `plugins![…]` controls which registered extensions are presented to the host.

### No absolute paths

Stock dar crates are referenced via `git = "…", rev = "…"` (pinned), a published registry version, or a vendored `.dar/vendor/` subtree for air-gapped deployments. Agent-local crates are referenced via relative path (`../extensions/<name>`). Absolute repo paths are permitted only in local-dev mode via a `[patch]` block. This is what makes "move folder = move agent" hold off the original build box.

### Shared build cache

`CARGO_TARGET_DIR=~/.cache/dar/target`. Cargo's advisory lock serializes concurrent agents on the same host. The stock host crates compile once and are shared across all 10 agents. A per-agent local extension change triggers ~1–3 s incremental relink. Per-agent `.dar/target/` is available as a fallback if cache isolation is preferred.

## The Self-Update Loop

Embedded in `dar-cli` as `dar self rebuild` (also callable programmatically by the orchestrator or any extension). Steps are ordered strictly:

1. **Lock.** Acquire `data/self-update.lock` (file lock, folder-scoped). Prevents concurrent self-updates.
2. **Compose.** Scan `../extensions/*`; read `[package.metadata.dar] factory` markers. Regenerate `.dar/Cargo.toml` `[dependencies]` block and `.dar/src/main.rs` `plugins![…]` list. If the generated content is bit-for-bit identical to the current files, skip the build.
3. **Build (out of process).** Spawn `cargo build --release --locked --target-dir …` as a child process. Build output goes to a temp path (`bin/dar.new`), never over the live binary. On non-zero exit: abort, log the stderr, keep the old binary running, release lock, return error.
4. **Doctor gate (hard).** Spawn the new binary as a child: `<bin/dar.new> doctor --dir ..`. The doctor pass must parse `agent.yaml`, instantiate every extension listed in `plugins![…]`, and exit 0. On non-zero exit: abort, delete `bin/dar.new`, keep old binary running, release lock, return error. No swap happens.
5. **Atomic swap.** `fs::rename("bin/dar", "bin/dar.prev")` then `fs::rename("bin/dar.new", "bin/dar")`. On Unix, `rename(2)` changes the directory entry atomically without touching the running process's inode — safe, no ETXTBSY. Never overwrite the running executable in place.
6. **Restart.** `execv(argv[0], argv)` replaces the process image in-place: same PID, no external supervisor required. The new binary takes over immediately with the same arguments. Cross-platform fallback: exit 0 and rely on a supervisor (launchd/systemd) to restart. `execv` is Unix-only; Windows is out of scope for this iteration (see Out of Scope).
7. **Crashloop safety net.** The doctor gate is the primary defense — a comprehensive doctor pass makes a runtime panic after swap near-impossible. Secondary: at boot, if the binary's own `--self-check` flag exits non-zero and `bin/dar.prev` exists, `execv` into `.prev`. A tiny external supervisor is optional "paranoid mode," not the default.

## User Stories

### (a) Self-contained agent / per-agent binary

1. As an operator, I want to bootstrap a new agent with `dar init-build --dir my-agent` and get a working `.dar/` composition crate without touching any repo file, so that 10 agents produce 10 independent binaries.
2. As an operator, I want `dar build --dir my-agent` to (re)generate the composition crate and produce `my-agent/bin/dar`, so that "build the agent" is a one-command operation.
3. As an operator, I want to move an agent folder to a different machine and run `dar build --dir .` to get a working binary there, so that no absolute repo paths are embedded and "move folder = move agent" holds.
4. As an operator running 10 agents, I want them to share `~/.cache/dar/target/` so the stock host crates compile once and each agent's rebuild costs only its local-extension delta, so that builds stay fast even at scale.
5. As an operator, I want the composition crate's `Cargo.lock` committed so that `cargo build --locked` produces a bit-for-bit identical binary across machines, so that reproducibility is not optional.

### (b) Self-update lifecycle

6. As a running agent, I want to invoke `dar self rebuild` (or have it triggered by the orchestrator) and have the agent rebuild its binary, swap, and `execv` into the new binary — all without any external tool or human — so that the agent owns its own lifecycle.
7. As a running agent, I want a failed build (non-zero `cargo build` exit) to leave the old binary running and log the failure, so that a bad extension edit never bricks the agent.
8. As a running agent, I want a failed doctor gate to abort the swap and leave the old binary running, so that a self-update that would produce a broken agent is caught before the running agent is ever replaced.
9. As a running agent that has been swapped, I want `bin/dar.prev` preserved, so that I can `execv` back to the previous binary if the new binary's boot-time self-check fails.
10. As an agent author, I want a `data/self-update.lock` file lock preventing concurrent self-update attempts, so that parallel triggers cannot corrupt the `bin/` state.

### (c) Build determinism and portability

11. As an operator on an air-gapped host, I want to vendor all stock dependencies into `.dar/vendor/` and build with `--offline`, so that the agent can rebuild itself without network access.
12. As an operator, I want `rust-toolchain.toml` inside `.dar/` to pin the Rust channel, so that a toolchain upgrade never silently changes the built binary.
13. As an operator, I want `dar doctor --dir .` to check that `cargo` and `rustc` are present and at the pinned version, so that a missing toolchain is caught before a self-update attempt, not during it.
14. As a maintainer updating the pinned `rev` in a stock dependency, I want an explicit `dar lock-refresh --dir .` command that runs `cargo update` without `--locked` and commits the new `Cargo.lock`, separate from the normal `--locked` build path, so that dependency bumps are deliberate, not accidental.

### (d) Authoring a local extension

15. As an extension author, I want `cargo dar new my-ext --kind background` inside an agent folder to emit a compiling local extension crate with the `[package.metadata.dar] factory = "my_ext::extension"` marker and a `pub fn extension()` factory, so that the composer can auto-discover it.
16. As an extension author, I want adding my new local extension crate to `extensions/` and running `dar build --dir .` to regenerate the composition crate and include my extension — no hand-editing of `.dar/Cargo.toml` or `src/main.rs` — so that composition is automatic.
17. As an extension author, I want the scaffold to emit an empty `[workspace]` table in my local crate's `Cargo.toml` so that Cargo does not treat it as a repo workspace member, preserving existing workspace behavior.

### (e) Preserved invariants

18. As the orchestrator, I want the tick loop, retry math (continuation vs backoff, `backoff_grows_then_caps`), candidate sort (priority asc null-last → created_at → identifier), reconcile skip-if-finished, and single-writer run-state discipline to be bit-for-bit equivalent, so that FSC is a packaging and lifecycle change only.
19. As the host, I want `HostPaths::assert_contained` and the orchestrator's `issue_workspace`/`assert_contained` to continue rejecting `..`/symlinks, so that path containment is unchanged.
20. As the runner, I want `--permission-mode bypassPermissions --add-dir <agent-folder>` flags preserved, so that the Claude child can edit `../../issues/ISSUE-N.md` without a permission prompt and without the `--dangerously-skip-permissions` hang.
21. As an operator, I want the two-state invariant (tracker owns issue state; orchestrator owns run state; orchestrator never writes issue state) upheld, so that a self-update does not accidentally change state ownership.

## Implementation Decisions

### Decision 1 — Refactor `dist` into `dar-cli` library

`crates/dar-cli` extracts all of `dist/src/main.rs`: boot wiring, HITL notifier, doctor subcommand, CLI dispatch, self-update logic. The `dist` bin becomes a 5-line caller. Every agent's `.dar/src/main.rs` is equally thin. This is the **unblocking first step** — without it, self-update logic cannot be embedded in the binary agents depend on.

### Decision 2 — Composition mechanism: explicit codegen, not `linkme`

The composer regenerates both the `[dependencies]` block of `.dar/Cargo.toml` and the `plugins![…]` list in `.dar/src/main.rs`. `linkme` was evaluated and rejected (see Further Notes).

### Decision 3 — Discovery via `[package.metadata.dar]`

Each local extension crate declares `factory = "<crate_ident>::extension"` under `[package.metadata.dar]`. Discovery: scan `extensions/*/Cargo.toml`. The `cargo dar new` scaffold emits this marker and a `pub fn extension() -> Box<dyn Extension>` factory. No runtime reflection; no separate manifest file. `cargo metadata` is available as a fallback for workspace-aware scenarios.

### Decision 4 — No absolute paths; git-rev / registry for stock deps

Stock dar crates are pinned via `git = "…", rev = "…"` (or a published registry version). Local extension crates use relative paths (`../extensions/<name>`). Absolute repo paths appear only in optional local-dev `[patch]` blocks, never in committed `.dar/Cargo.toml`. This is the mechanism that makes off-box portability real.

### Decision 5 — Cargo features for stock subsetting (orthogonal knob)

Stock extensions are `optional` deps in the generated `.dar/Cargo.toml`, gated by features. The composer maps `agent.yaml` selection (`runner.use`, `tracker.use`, `foreground`) to feature flags. A minimal agent that uses only `tracker-files` + `runner-claude` + `orchestrator` + `frontend-log` links only those four, keeping binary size proportional to actual use.

### Decision 6 — Determinism: committed lock, `--locked`, toolchain pin

`Cargo.lock` is committed inside `.dar/`. Normal builds use `cargo build --locked`. Dependency bumps go through an explicit `dar lock-refresh` command (runs `cargo update` without `--locked`, then the operator commits the new lock). `rust-toolchain.toml` pins the Rust channel. `doctor` verifies `cargo` and `rustc` present and at the pinned version.

### Decision 7 — Shared build cache via `CARGO_TARGET_DIR`

Set `CARGO_TARGET_DIR=~/.cache/dar/target` (or a configurable override). Cargo's advisory file lock serializes concurrent compilations safely. The stock host crates (which change rarely) compile once; agents with only local-extension changes pay only the incremental link step. Per-agent `.dar/target/` is documented as an opt-in isolation fallback.

### Migration order (each step keeps `cargo test --release` green)

1. **Refactor `dist` → `dar-cli`.** Extract all boot/doctor/CLI wiring into `crates/dar-cli`. `dist` becomes a thin caller. No behavior change; the repo's own binary now builds through the same path all agents will use. All existing tests pass.
2. **Discovery marker + scaffold.** Add `[package.metadata.dar] factory` to the `cargo dar new` scaffold output. Update `extensions/example` to carry the marker. Define `pub fn extension()` convention.
3. **Composer + `init-build` + `build` subcommands.** Implement scan + codegen in `dar-cli`. Add `dar init-build --dir` (bootstraps `.dar/`) and `dar build --dir` (regenerate + `cargo build --locked` + copy binary to `bin/`). Validate against the `example-agent` fixture.
4. **Feature-gated stock subsetting.** Make stock extensions `optional` deps in the generated composition crate. Map `agent.yaml` selections to features in the composer.
5. **Self-update loop + doctor gate + atomic swap + `execv` + `.prev` fallback.** Implement `dar self rebuild` in `dar-cli`. Wire `data/self-update.lock`, out-of-process build, doctor-gate spawn, `fs::rename` swap, `execv`. Add boot-time self-check + `.prev` fallback in `main()`.
6. **Off-box portability.** Switch stock dep references to git-rev in generated `Cargo.toml`. Add `dar lock-refresh`. Add optional `--vendor` to `build` / `init-build`.

## Testing Decisions

Tests assert external behavior through public contracts, not codegen internals. Test suite additions:

1. **Composer generates a valid composition crate.** Given a fixture agent folder with one local extension (carrying the `factory` marker), the composer produces a `.dar/Cargo.toml` and `src/main.rs` that (a) parse as valid TOML/Rust, (b) contain the expected `[dependencies]` entries and `plugins![…]` items, and (c) compile successfully with `cargo build --locked` against the fixture.

2. **`--locked` determinism.** Build the same fixture twice on the same machine; assert the output binary is byte-for-byte identical (given a fixed `Cargo.lock` and toolchain).

3. **Doctor gate aborts on a broken extension.** Build a composition that includes a deliberately broken extension (its `doctor` validation always fails). Assert that the self-update loop aborts before `fs::rename`, that `bin/dar` is untouched, and that `bin/dar.new` is cleaned up.

4. **Atomic rename preserves `.prev`.** Run a full self-update cycle in a temp agent folder. Assert that after a successful swap, `bin/dar.prev` contains the previous binary and `bin/dar` contains the new one, and both are valid executables.

5. **Boot-time `.prev` fallback fires.** Simulate a current binary that fails `--self-check`; assert that `main()` `execv`s into `bin/dar.prev` rather than panicking or exiting non-zero.

6. **Scaffolded extension is auto-discovered.** Run `cargo dar new my-ext --kind background` inside a temp agent folder. Assert the generated `Cargo.toml` contains `[package.metadata.dar] factory = "my_ext::extension"` and that the composer's scan picks it up without manual configuration.

7. **Feature subsetting drops a stock extension.** Generate a composition crate with `tracker-linear` excluded from `agent.yaml`. Assert that `tracker-linear` does not appear in the generated `[dependencies]` block and that the resulting binary does not export the `tracker_linear` symbol (via `nm` or a link check).

8. **Concurrent self-update is serialized.** Trigger two concurrent `dar self rebuild` calls against the same agent folder. Assert that the second call waits behind the `data/self-update.lock` and does not corrupt `bin/` state.

Out of test scope: WASM/dlopen loading (not implemented), Windows self-replace (Unix-only), the external supervisor, askama template rendering (compile-time checked), bus transport internals.

## Out of Scope

**Runtime drop-in (WASM, dlopen, subprocess).** Rejected for the same reasons as PRD-EXTENSIONS: Rust has no stable ABI, so any dynamic loading mechanism requires a C-ABI or WIT contract rewrite. The extensions in this system do native I/O — spawn `claude`/`codex` child processes, call Linear over HTTPS, own the terminal — exactly the capabilities that are worst-fit for a sandbox. Recompile with a shared `CARGO_TARGET_DIR` costs seconds; dynamic loading buys nothing and loses type safety.

**Windows self-replace.** `rename(2)` over a running executable is safe on Unix (inode vs directory entry). On Windows, this requires a launcher shim to move the old binary before writing the new one. Declared out of scope for this iteration; a Windows support PRD can address it.

**Bundling a Rust toolchain.** Bundling `cargo` + `rustc` adds ~500 MB to the agent folder. Rejected. The toolchain must be present on the host; `doctor` checks for it and fails loudly with an actionable message if missing.

**Multi-repo split.** The crate graph is shaped for eventual extraction (no circular deps, contracts in separate crates), but splitting is a separate project.

**Behavior changes.** Tick loop, retry math, candidate sort, containment, permission flags, two-state invariant, dashboard UX — all unchanged. FSC is a packaging and lifecycle layer on top of the already-shipped extension architecture.

**Supervisor as default.** A tiny external supervisor (launchd/systemd unit) is documented as optional "paranoid mode." The doctor gate + `.prev` fallback make it unnecessary for the common case.

**`inventory`/`linkme` as composition mechanism.** See Further Notes.

## Further Notes

- **The single most important enabler** is the `dar-cli` refactor (Decision 1). The repo's own `dist` binary and every agent's `.dar` binary then build through the exact same `dar_cli::run(plugins![…])` path. Everything else (composer, self-update, doctor gate) is layered on top of that foundation. Start there.

- **`linkme` was evaluated and rejected.** The core problem: `linkme` does not remove the need to edit `Cargo.toml`. Cargo requires explicit `[dependencies]` entries before resolution can proceed. A `#[distributed_slice]` static in an unused dependency crate gets dead-code-eliminated unless force-linked (`use my_ext as _;`). So the composer must regenerate `[dependencies]` regardless — `linkme` would only save the `plugins![…]` list while adding a magic global and a force-link line per crate. Explicit codegen is more debuggable, grep-able, and Cargo-accurate.

- **Toolchain as a new runtime dependency.** `cargo` and `rustc` are now runtime prerequisites for self-update. This is the real new operational cost of FSC. `doctor` must verify both are present at the pinned version and emit a clear error (`cargo not found — install Rust via rustup, then re-run doctor`) so operators know what to fix. Document this in the agent setup guide.

- **Hold the ALG-186 footprint gate.** Binary size grows only with the stock extensions each agent actually links (feature subsetting, Decision 5). Per-agent binary size should be measurably smaller than the monolith for agents that exclude stock extensions they don't use. Re-run the pilot benchmark runbook after Step 4 of the migration order to confirm the gate holds.

- **Committed `.dar/` is intentional.** The composition crate, its `Cargo.lock`, and `rust-toolchain.toml` are committed source — not build artifacts. An operator cloning an agent folder gets a fully specified, reproducible build without any side-channel configuration. The generated `src/main.rs` and `Cargo.toml` carry a `# generated — do not hand-edit` header; the composer overwrites them on each `dar build` run.
