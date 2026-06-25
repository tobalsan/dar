# Dar Extension SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let third-party Dar extensions live outside the `dar` repo without git-rev pinning or `bump.sh`, while publishing the fewest crates needed.

**Architecture:** Add one public SDK crate, `dar-extension-sdk`, as the only dependency external extension authors need to name. The SDK re-exports stable extension contracts and owns the small helper surface currently leaking through `runner-core`; concrete stock extensions such as `chat-pi` stay out of the public SDK.

**Tech Stack:** Rust 2021, Cargo workspace, crates.io package metadata, existing Dar typed service registry and event bus.

---

## Target Outcome

External extensions should depend on one crate:

```toml
[dependencies]
dar-extension-sdk = "0.2"
```

Minimum crates to publish for this goal:

1. `dar-host-api`
2. `dar-cap-runner`
3. `dar-cap-chat`
4. `dar-tool-registry`
5. `dar-orchestrator-api`
6. `dar-extension-sdk`

Do not publish all 31 crates for this goal. `cargo install dar` from crates.io is out of scope for this plan.

## Key Design Decisions

- `dar-extension-sdk` is the public interface for third-party extensions.
- `runner-core` remains first-party/internal for stock runners and chat backends.
- `chat-pi` remains a stock implementation crate, not SDK surface.
- External extensions should resolve the existing `"pi"` chat backend from host services instead of registering their own `chat_pi::PiChatBackend`.
- Chat-capable external extensions must declare required stock extensions in `[package.metadata.dar]`, e.g. `requires_stock = ["chat-pi"]`, so composed agents with `foreground: logs` still link the backend.
- The SDK should contain small stable helpers:
  - `sdk::log::event(issue, event, message)`
  - `sdk::tools::host_tool_bridge(services, agent_root)`
- The SDK may depend on `dar-tool-registry` to implement `host_tool_bridge`.
- The SDK should not expose pi/codex/opencode protocol helpers.
- Every crate outside the six-crate publish set must explicitly set `publish = false` so workspace release tooling cannot accidentally publish internal crates.

## Files

- Create: `crates/extension-sdk/Cargo.toml`
- Create: `crates/extension-sdk/src/lib.rs`
- Create: `crates/extension-sdk/tests/public_api.rs`
- Modify: `Cargo.toml`
- Modify: `crates/runner-core/src/bridge.rs`
- Modify: `crates/dar-cli/src/composer.rs`
- Modify: `extensions/orchestrator/src/logging.rs`
- Modify: `~/code/agentropy/dar-extensions/telegram/Cargo.toml`
- Modify: `~/code/agentropy/dar-extensions/telegram/src/lib.rs`
- Modify: `~/code/agentropy/dar-extensions/irc/Cargo.toml`
- Modify: `~/code/agentropy/dar-extensions/irc/src/lib.rs`
- Modify: every internal crate manifest outside the six-crate publish set to add `publish = false`.

## Task 1: Add the SDK Crate

**Files:**
- Create: `crates/extension-sdk/Cargo.toml`
- Create: `crates/extension-sdk/src/lib.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Add workspace member**

In root `Cargo.toml`, add this member near the other `crates/*` entries:

```toml
    "crates/extension-sdk",
```

- [ ] **Step 2: Create SDK manifest**

Create `crates/extension-sdk/Cargo.toml`:

```toml
[package]
name = "dar-extension-sdk"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Public SDK for writing third-party dar extensions"
rust-version = "1.83"

[lib]
name = "dar_extension_sdk"

[dependencies]
anyhow = "1"
cap-chat = { package = "dar-cap-chat", path = "../cap-chat", version = "0.2" }
host-api = { package = "dar-host-api", path = "../host-api", version = "0.2" }
orchestrator-api = { package = "dar-orchestrator-api", path = "../orchestrator-api", version = "0.2" }
tool-registry = { package = "dar-tool-registry", path = "../tool-registry", version = "0.2" }
tracing = "0.1"

[dev-dependencies]
tokio = { version = "1.43", features = ["sync"] }
```

- [ ] **Step 3: Create SDK implementation**

Create `crates/extension-sdk/src/lib.rs`:

```rust
//! Public SDK for writing third-party dar extensions.
//!
//! This crate is the stable extension-author surface. Prefer depending on this
//! crate instead of individual dar workspace crates.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub use host_api::{
    BoxFuture, ConfigStore, EventBus, Extension, HostPaths, RegisterCtx, ServiceRegistry,
    ShutdownToken, StartCtx,
};

pub mod chat {
    pub use cap_chat::{
        BoxFuture, ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams,
        ChatSessionParamsBuilder, HostToolBridge, CHAT_FALLBACK_BACKEND,
    };
}

pub mod orchestrator {
    pub use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
}

pub mod log {
    use super::{Mutex, OnceLock};

    /// Structured extension event logger: `(issue, event, message)`.
    pub type EventHook = fn(&str, &str, &str);

    static EVENT_HOOK: OnceLock<Mutex<Option<EventHook>>> = OnceLock::new();

    fn hook_slot() -> &'static Mutex<Option<EventHook>> {
        EVENT_HOOK.get_or_init(|| Mutex::new(None))
    }

    /// Install the host event logger used by SDK-based extensions.
    pub fn set_event_hook(hook: EventHook) {
        *hook_slot().lock().expect("extension sdk log hook poisoned") = Some(hook);
    }

    /// Emit one structured extension event.
    pub fn event(issue: &str, event: &str, message: &str) {
        let hook = *hook_slot().lock().expect("extension sdk log hook poisoned");
        match hook {
            Some(f) => f(issue, event, message),
            None => tracing::info!(issue = %issue, event = %event, "{message}"),
        }
    }
}

pub mod tools {
    use super::{Path, ServiceRegistry};
    use cap_chat::HostToolBridge;
    use tool_registry::{ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

    /// Resolve the hidden host MCP bridge command for a chat or runner spawn.
    ///
    /// Keep this in sync with `runner_core::host_tool_bridge`; both helpers
    /// intentionally emit the same `__mcp-bridge --dir <agent-root>` shape.
    ///
    /// Returns `None` when no tool registry is present or it has no tools.
    pub fn host_tool_bridge(
        services: &ServiceRegistry,
        agent_root: &Path,
    ) -> Option<HostToolBridge> {
        let registry = services
            .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            .ok()?;
        if registry.is_empty() {
            return None;
        }
        let command = std::env::current_exe().ok()?.to_string_lossy().into_owned();
        Some(HostToolBridge {
            command,
            args: vec![
                "__mcp-bridge".to_string(),
                "--dir".to_string(),
                agent_root.display().to_string(),
            ],
        })
    }
}
```

- [ ] **Step 4: Verify the SDK crate compiles**

Run:

```bash
cargo check -p dar-extension-sdk
```

Expected: command exits successfully.

- [ ] **Step 5: Link duplicated bridge helpers**

In `crates/runner-core/src/bridge.rs`, add the matching drift warning above `host_tool_bridge`:

```rust
/// Resolve the hidden host MCP bridge command for a runner/chat spawn. Returns
/// `None` when no tool registry is present or it has no tools, preserving the
/// cheap no-tools path for agents that do not use runtime tools.
///
/// Keep this in sync with `dar_extension_sdk::tools::host_tool_bridge`; both
/// helpers intentionally emit the same `__mcp-bridge --dir <agent-root>` shape.
```

Do not change the command or args shape in either helper.

## Task 2: Add SDK API Tests

**Files:**
- Create: `crates/extension-sdk/tests/public_api.rs`

- [ ] **Step 1: Add compile-facing API test**

Create `crates/extension-sdk/tests/public_api.rs`:

```rust
use std::sync::Arc;

use anyhow::Result;
use dar_extension_sdk::chat::{
    ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams, CHAT_FALLBACK_BACKEND,
};
use dar_extension_sdk::orchestrator::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
use dar_extension_sdk::{BoxFuture, Extension, RegisterCtx, ServiceRegistry, StartCtx};
use tokio::sync::mpsc;

struct TestExtension;

impl Extension for TestExtension {
    fn id(&self) -> &'static str {
        "test-extension"
    }

    fn register<'a>(&'a self, _ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct TestBackend;

impl ChatBackend for TestBackend {
    fn open<'a>(
        &'a self,
        _params: ChatSessionParams,
        _tx: mpsc::Sender<ChatEvent>,
    ) -> dar_extension_sdk::chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async { anyhow::bail!("test backend is compile-only") })
    }
}

#[test]
fn sdk_reexports_extension_contracts() {
    let extension = TestExtension;
    assert_eq!(extension.id(), "test-extension");

    let snapshot = RunSnapshot::empty();
    assert!(snapshot.active.is_none());
    assert!(snapshot.active_runs.is_empty());
    assert_eq!(RUN_SNAPSHOT_TOPIC, "orchestrator.run-snapshot");
    assert_eq!(CHAT_FALLBACK_BACKEND, "pi");

    let _role = ChatRole::Assistant;
}

#[test]
fn sdk_reexports_chat_backend_contract() {
    let mut services = ServiceRegistry::default();
    services
        .service::<dyn ChatBackend>("test", Arc::new(TestBackend))
        .unwrap();
    assert!(services.get_named::<dyn ChatBackend>("test").is_ok());
}
```

- [ ] **Step 2: Run SDK tests**

Run:

```bash
cargo test -p dar-extension-sdk
```

Expected: SDK tests pass.

## Task 3: Wire SDK Logging Into the Host

**Files:**
- Modify: `extensions/orchestrator/src/logging.rs`

- [ ] **Step 1: Inspect current hook installation**

Open `extensions/orchestrator/src/logging.rs`. It currently installs `runner_core::set_log_hook(ev)` in the host logging setup path.

- [ ] **Step 2: Add SDK hook installation**

Update the same setup path so it installs both hooks:

```rust
runner_core::set_log_hook(ev);
dar_extension_sdk::log::set_event_hook(ev);
```

- [ ] **Step 3: Add orchestrator dependency on SDK**

In `extensions/orchestrator/Cargo.toml`, add:

```toml
dar-extension-sdk = { path = "../../crates/extension-sdk", version = "0.2" }
```

- [ ] **Step 4: Verify orchestrator compiles**

Run:

```bash
cargo check -p dar-orchestrator
```

Expected: command exits successfully.

## Task 4: Teach the Composer About Extension Stock Requirements

**Files:**
- Modify: `crates/dar-cli/src/composer.rs`

- [ ] **Step 1: Extend `LocalExtension`**

Add a stock-requirement field:

```rust
#[derive(Debug, Clone, Eq, PartialEq)]
struct LocalExtension {
    package: String,
    factory: String,
    path: PathBuf,
    requires_stock: Vec<String>,
}
```

- [ ] **Step 2: Parse `[package.metadata.dar].requires_stock`**

In `discover_extensions`, after reading `factory`, parse an optional array:

```rust
let requires_stock = meta
    .and_then(|m| m.get("dar"))
    .and_then(|a| a.get("requires_stock"))
    .and_then(toml::Value::as_array)
    .map(|items| {
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .context("package.metadata.dar.requires_stock entries must be strings")
            })
            .collect::<Result<Vec<_>>>()
    })
    .transpose()?
    .unwrap_or_default();
```

Then construct:

```rust
extensions.push(LocalExtension {
    package,
    factory: factory.to_string(),
    path: PathBuf::from("../extensions").join(entry.file_name()),
    requires_stock,
});
```

- [ ] **Step 3: Include required stock extensions in the composition**

Change `write_composition_crate` so stock selection sees local extension requirements. Replace:

```rust
let stock = selected_stock_extensions(&agent)?;
let locals = discover_extensions(&agent)?;
```

with:

```rust
let locals = discover_extensions(&agent)?;
let stock = selected_stock_extensions(&agent, &locals)?;
```

Update the function signature:

```rust
fn selected_stock_extensions(
    agent: &Path,
    locals: &[LocalExtension],
) -> Result<Vec<&'static StockExtension>> {
```

Inside `selected_stock_extensions`, after `packages.extend(foreground_packages(&selection)?);`, add:

```rust
for local in locals {
    for package in &local.requires_stock {
        packages.push(package.as_str());
    }
}
```

Keep the existing unknown-stock validation, so typos such as `"chat_pi"` fail at build time.

- [ ] **Step 4: Add composer tests**

Add a test next to the existing composer tests:

```rust
#[test]
fn local_extension_can_require_chat_pi_under_logs_foreground() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path();
    write_agent_yaml(agent, "files", "fake", "logs", "");
    write_test_extension_with_metadata(
        agent,
        r#"[package.metadata.dar]
factory = "my_ext::extension"
requires_stock = ["chat-pi"]
"#,
    );

    compose(agent).unwrap();

    let manifest = std::fs::read_to_string(agent.join(".dar/Cargo.toml")).unwrap();
    let source = std::fs::read_to_string(agent.join(".dar/src/main.rs")).unwrap();
    assert!(manifest.contains("chat-pi = { package = \"dar-chat-pi\", version = "));
    assert!(source.contains("chat_pi::ChatPiExtension"));
}
```

If the existing `write_test_extension` helper hard-codes metadata, add a second helper:

```rust
fn write_test_extension_with_metadata(agent: &Path, metadata: &str) {
    let extension = agent.join("extensions/my-ext");
    std::fs::create_dir_all(extension.join("src")).unwrap();
    std::fs::write(
        extension.join("Cargo.toml"),
        format!(
            r#"[package]
name = "my-ext"
version = "0.1.0"
edition = "2021"

{metadata}

[dependencies]
host-api = {{ version = "0.2" }}
"#
        ),
    )
    .unwrap();
    std::fs::write(
        extension.join("src/lib.rs"),
        r#"use host_api::Extension;

pub fn extension() -> Box<dyn Extension> {
    Box::new(MyExt)
}

struct MyExt;

impl Extension for MyExt {
    fn id(&self) -> &'static str {
        "my-ext"
    }
}
"#,
    )
    .unwrap();
}
```

Then update the old `write_test_extension` helper to call it with:

```rust
write_test_extension_with_metadata(
    agent,
    r#"[package.metadata.dar]
factory = "my_ext::extension"
"#,
);
```

- [ ] **Step 5: Verify composer tests**

Run:

```bash
cargo test -p dar-cli-core composer
```

Expected: composer tests pass.

## Task 5: Convert External Extensions to the SDK

**Files:**
- Modify: `~/code/agentropy/dar-extensions/telegram/Cargo.toml`
- Modify: `~/code/agentropy/dar-extensions/telegram/src/lib.rs`
- Modify: `~/code/agentropy/dar-extensions/irc/Cargo.toml`
- Modify: `~/code/agentropy/dar-extensions/irc/src/lib.rs`

- [ ] **Step 1: Add stock chat backend requirement**

In `telegram/Cargo.toml`, replace the existing extension metadata with:

```toml
[package.metadata.dar]
factory = "telegram::extension"
requires_stock = ["chat-pi"]
```

In `irc/Cargo.toml`, replace the existing extension metadata with:

```toml
[package.metadata.dar]
factory = "irc::extension"
requires_stock = ["chat-pi"]
```

If either manifest still uses `[package.metadata.agentropy]`, replace it with `[package.metadata.dar]`. The current composer discovery path reads `[package.metadata.dar]`.

- [ ] **Step 2: Replace Dar git deps**

In both external extension manifests, replace:

```toml
cap-chat = { git = "https://github.com/tobalsan/dar.git", rev = "..." }
chat-pi = { git = "https://github.com/tobalsan/dar.git", rev = "..." }
host-api = { git = "https://github.com/tobalsan/dar.git", rev = "..." }
orchestrator-api = { git = "https://github.com/tobalsan/dar.git", rev = "..." }
runner-core = { git = "https://github.com/tobalsan/dar.git", rev = "..." }
```

with this local path dependency during development:

```toml
dar-extension-sdk = { path = "../../dar/crates/extension-sdk" }
```

After crates.io publish, replace it with:

```toml
dar-extension-sdk = "0.2"
```

- [ ] **Step 3: Replace imports**

In both `src/lib.rs` files, replace direct Dar imports:

```rust
use cap_chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use host_api::{ConfigStore, Extension, RegisterCtx, ShutdownToken, StartCtx};
use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
```

with:

```rust
use dar_extension_sdk::chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use dar_extension_sdk::orchestrator::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
use dar_extension_sdk::{ConfigStore, Extension, RegisterCtx, ShutdownToken, StartCtx};
```

- [ ] **Step 4: Replace `host_api::BoxFuture` paths**

Replace:

```rust
host_api::BoxFuture
```

with:

```rust
dar_extension_sdk::BoxFuture
```

- [ ] **Step 5: Replace `runner_core` helper calls**

Replace:

```rust
runner_core::log_ev("-", "telegram", "extension enabled; connecting to Telegram bot API");
runner_core::host_tool_bridge(&ctx.host.services, ctx.paths.root())
```

with:

```rust
dar_extension_sdk::log::event("-", "telegram", "extension enabled; connecting to Telegram bot API");
dar_extension_sdk::tools::host_tool_bridge(&ctx.host.services, ctx.paths.root())
```

Do the same mechanical replacement in `irc/src/lib.rs`.

- [ ] **Step 6: Confirm the host provides the stock `"pi"` chat backend**

Before deleting external extension self-registration, confirm the shipped/composed host registers `chat_pi::ChatPiExtension` through either the stock dist binary or the new `requires_stock = ["chat-pi"]` composer path.

Run:

```bash
rg -n 'chat_pi::ChatPiExtension|package: "chat-pi"|factory: "chat_pi::ChatPiExtension"' dist/src/main.rs crates/dar-cli/src/composer.rs
```

Expected:

```text
dist/src/main.rs contains chat_pi::ChatPiExtension
crates/dar-cli/src/composer.rs contains package: "chat-pi"
crates/dar-cli/src/composer.rs contains factory: "chat_pi::ChatPiExtension"
```

If the target composed host can still omit `chat-pi`, stop here and do not remove self-registration. Fix the composer requirement first.

- [ ] **Step 7: Remove direct `chat-pi` registration**

In both external extensions, delete the registration line that creates a private Pi backend:

```rust
ctx.services
    .service::<dyn ChatBackend>(SELF_BACKEND_ID, Arc::new(chat_pi::PiChatBackend))?;
```

Then change the extension's default backend id from its private backend to the stock backend:

```rust
const SELF_BACKEND_ID: &str = "pi";
```

If the constant name reads badly after this change, rename it to:

```rust
const DEFAULT_BACKEND_ID: &str = "pi";
```

Update local references to the renamed constant.

- [ ] **Step 8: Verify external extensions build against local SDK**

Run:

```bash
cargo check --manifest-path ~/code/agentropy/dar-extensions/telegram/Cargo.toml
cargo check --manifest-path ~/code/agentropy/dar-extensions/irc/Cargo.toml
```

Expected: both commands exit successfully.

## Task 6: Verify the Publish Closure

**Files:**
- Inspect only: `Cargo.toml`
- Inspect only: `crates/*/Cargo.toml`

- [ ] **Step 1: Confirm SDK dependency closure**

Run:

```bash
cargo tree -p dar-extension-sdk -e normal,build
```

Expected internal Dar crates in the tree:

```text
dar-extension-sdk
dar-host-api
dar-cap-chat
dar-cap-runner
dar-orchestrator-api
dar-tool-registry
```

Expected absent from the tree:

```text
dar-runner-core
dar-chat-pi
dar-orchestrator
dar-dashboard
dar-tui
dar-cli
```

- [ ] **Step 2: Confirm external extensions no longer use git-pinned Dar crates**

Run:

```bash
rg -n 'github.com/tobalsan/dar|rev = "[0-9a-f]{40}"' ~/code/agentropy/dar-extensions -g 'Cargo.toml'
```

Expected: no matches for Dar crate dependencies.

## Task 7: Mark Internal Crates Unpublishable

**Files:**
- Modify: all `Cargo.toml` package manifests outside the six-crate publish set

- [ ] **Step 1: Add `publish = false` outside the public SDK closure**

Add this under `[package]` in every crate that is not one of:

```text
dar-host-api
dar-cap-runner
dar-cap-chat
dar-tool-registry
dar-orchestrator-api
dar-extension-sdk
```

Use:

```toml
publish = false
```

This is required. Do not leave internal crates publishable, because `cargo release --workspace` or a similar workspace publish flow could otherwise try to publish the whole internal graph and hit crates.io's new-crate burst limit.

- [ ] **Step 2: Verify only the six public crates remain publishable**

Run:

```bash
cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.publish == null) | .name' \
  | sort
```

Expected:

```text
dar-cap-chat
dar-cap-runner
dar-extension-sdk
dar-host-api
dar-orchestrator-api
dar-tool-registry
```

- [ ] **Step 3: Verify the binary install path is intentionally out of scope**

Run:

```bash
cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "dar-cli") | .publish'
```

Expected: `[]` for any internal `dar-cli`/dist package unless this plan has been explicitly expanded to support `cargo install dar`.

## Task 8: Prepare Crates for Publishing

**Files:**
- Modify: `crates/host-api/Cargo.toml`
- Modify: `crates/cap-runner/Cargo.toml`
- Modify: `crates/cap-chat/Cargo.toml`
- Modify: `crates/tool-registry/Cargo.toml`
- Modify: `crates/orchestrator-api/Cargo.toml`
- Modify: `crates/extension-sdk/Cargo.toml`

- [ ] **Step 1: Ensure the six public crates have publishable metadata**

Each of the six manifests should have:

```toml
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version = "1.83"
description = "..."
```

Do not add `publish = false` to these six crates.

- [ ] **Step 2: Package-check each public crate**

Run:

```bash
cargo package -p dar-host-api --allow-dirty
cargo package -p dar-cap-runner --allow-dirty
cargo package -p dar-cap-chat --allow-dirty
cargo package -p dar-tool-registry --allow-dirty
cargo package -p dar-orchestrator-api --allow-dirty
cargo package -p dar-extension-sdk --allow-dirty
```

Expected: each package command exits successfully.

- [ ] **Step 3: Publish in dependency order**

When ready to publish for real, use this order:

```bash
cargo publish -p dar-host-api
cargo publish -p dar-cap-runner
cargo publish -p dar-tool-registry
cargo publish -p dar-cap-chat
cargo publish -p dar-orchestrator-api
cargo publish -p dar-extension-sdk
```

Expected: crates.io accepts each crate after its dependencies are available.

## Task 9: Final Verification

**Files:**
- Whole workspace
- `~/code/agentropy/dar-extensions`

- [ ] **Step 1: Run workspace tests**

Run:

```bash
cargo test --release
```

Expected: workspace tests pass.

- [ ] **Step 2: Build external extensions**

Run:

```bash
cargo check --manifest-path ~/code/agentropy/dar-extensions/telegram/Cargo.toml
cargo check --manifest-path ~/code/agentropy/dar-extensions/irc/Cargo.toml
```

Expected: both external extensions build without `bump.sh`.

- [ ] **Step 3: Confirm `bump.sh` is no longer needed for Dar crate pins**

Run:

```bash
rg -n 'dar.git|bump.sh|rev =' ~/code/agentropy/dar-extensions
```

Expected: no Dar git rev pins remain. `bump.sh` may still exist, but it should no longer be required for normal extension development.

## Fallback Shortcut

If the executor needs the smallest code change rather than the cleanest public interface, make `dar-extension-sdk` re-export `runner_core::log_ev` and `runner_core::host_tool_bridge`.

That shortcut publishes seven crates instead of six because `dar-runner-core` enters the public closure. It is acceptable as a temporary bridge, but it exposes stock runner protocol helpers as public surface. Prefer the six-crate plan above.

## Non-Goals

- Do not make `cargo install dar` from crates.io work in this plan.
- Do not publish `dar-cli`, `dar-orchestrator`, `dar-dashboard`, `dar-tui`, stock runners, stock trackers, or stock chat backends.
- Do not make a dynamic runtime plugin loader.
- Do not change the existing static composition model.
