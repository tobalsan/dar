//! Host-side extension tool registry — the stable contract by which extensions
//! expose runtime tools to agents, and by which the host MCP bridge enumerates
//! and dispatches them.
//!
//! ## Shape
//!
//! An extension registers a [`ToolSpec`] (name + JSON input schema + metadata)
//! together with a boxed async [`ToolExecutor`]. Registration happens during the
//! extension's `register()` pass, against a single shared [`ToolRegistry`] that
//! one host extension publishes as a service before any tool-providing
//! extension runs.
//!
//! The registry is the single place that:
//!   - rejects duplicate tool names as a hard error at registration time (which,
//!     because registration runs during doctor/boot, surfaces as a doctor/boot
//!     failure — no auto-prefixing, no last-writer-wins),
//!   - lists tool specs for the MCP bridge's `tools/list`,
//!   - dispatches a call by name + JSON args for the bridge's `tools/call`,
//!     normalizing executor errors and panics into a structured
//!     [`ToolOutcome`] (`is_error: true`) rather than letting them stall a run.
//!
//! Executors run in the host runtime and are the only place that touches
//! extension config/secrets; the agent only ever sees the tool name, schema and
//! the structured result.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

pub mod observe;
pub use observe::{Redactor, ToolCallObservation};

/// Service id under which the shared [`ToolRegistry`] is published in the host
/// service registry. Extensions resolve it with
/// `ctx.services.get::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)`.
pub const TOOL_REGISTRY_SERVICE: &str = "tool-registry";

/// A tool advertised to an agent: a stable name, a human-readable description,
/// and a JSON Schema describing the accepted arguments object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique tool name the agent calls by. Must be non-empty and unique across
    /// the whole registry.
    pub name: String,
    /// Short description surfaced to the agent.
    pub description: String,
    /// JSON Schema for the arguments object (the MCP `inputSchema`).
    pub input_schema: Value,
    /// Optional read/write metadata describing whether the tool reads and/or
    /// mutates host state. Advisory only in v1 (surfaced in logs / future UI;
    /// the registry does **not** enforce permissions on it). Both default to
    /// `false` so existing specs keep their meaning across (de)serialization.
    #[serde(default)]
    pub access: ToolAccess,
}

/// Advisory read/write metadata for a tool. `read` means the tool inspects host
/// state; `write` means it may mutate it. Used purely to explain a tool in logs
/// and a future permission UI — v1 does not enforce anything on these flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAccess {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
}

impl ToolAccess {
    /// Compact human-readable label for logs, e.g. `read,write`, `write`,
    /// `read`, or `none` when neither flag is set.
    pub fn label(&self) -> &'static str {
        match (self.read, self.write) {
            (true, true) => "read,write",
            (true, false) => "read",
            (false, true) => "write",
            (false, false) => "none",
        }
    }
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            access: ToolAccess::default(),
        }
    }

    /// Set the read/write access metadata, returning the spec (builder style).
    pub fn with_access(mut self, read: bool, write: bool) -> Self {
        self.access = ToolAccess { read, write };
        self
    }

    /// Mark this tool as reading host state.
    pub fn reads(mut self) -> Self {
        self.access.read = true;
        self
    }

    /// Mark this tool as mutating host state.
    pub fn writes(mut self) -> Self {
        self.access.write = true;
        self
    }

    /// Render this spec as an MCP `tools/list` entry. Read/write metadata rides
    /// along in the standard MCP `annotations` object (`readOnlyHint` /
    /// non-`readOnly`), so a client/UI that understands annotations can explain
    /// the tool without a bespoke field.
    pub fn to_mcp_tool(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "readOnlyHint": self.access.read && !self.access.write,
                "destructiveHint": self.access.write,
            },
        })
    }
}

/// The structured result of a tool call, modeled on the MCP `tools/call`
/// result: a single text content block plus an `is_error` flag. A failed call
/// is data, not a transport error — it returns to the agent so the run
/// continues.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }

    /// Render as an MCP `tools/call` result object.
    pub fn to_mcp_result(&self) -> Value {
        json!({
            "content": [{ "type": "text", "text": self.text }],
            "isError": self.is_error,
        })
    }
}

/// An async tool implementation. Runs in the host runtime with access to the
/// extension's config/secrets. Return `Ok(ToolOutcome::error(..))` for an
/// expected/structured failure; return `Err(..)` only for an unexpected fault —
/// the registry normalizes both into a structured `is_error` outcome so the run
/// never stalls.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, args: Value) -> Result<ToolOutcome>;
}

/// Object-safe handle published as a service so extensions and the bridge share
/// one registry across `register()` passes. Backed by [`ToolRegistry`].
#[async_trait::async_trait]
pub trait ToolRegistryHandle: Send + Sync {
    /// Register a tool. Errors (notably duplicate names) propagate out of the
    /// caller's `register()` and fail doctor/boot.
    fn register_tool(&self, spec: ToolSpec, executor: Arc<dyn ToolExecutor>) -> Result<()>;

    /// Snapshot of all registered tool specs, sorted by name for stable output.
    fn list(&self) -> Vec<ToolSpec>;

    /// Whether any tool is registered. Used to gate bridge wiring.
    fn is_empty(&self) -> bool;

    /// Dispatch a call by name + JSON args, returning a structured outcome.
    /// Errors and panics are normalized to `is_error: true`.
    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome;

    /// Dispatch a call and also return a [`ToolCallObservation`] (redacted +
    /// truncated, carrying name/status/duration/read-write) for logging.
    async fn dispatch_observed(
        &self,
        name: &str,
        args: Value,
        redactor: &Redactor,
    ) -> (ToolOutcome, ToolCallObservation);
}

struct RegisteredTool {
    spec: ToolSpec,
    executor: Arc<dyn ToolExecutor>,
}

/// The shared registry. Cheaply clonable (`Arc` inside); a single instance is
/// published as a service and mutated through interior locking, so extensions
/// registering in their own `register()` pass all see the same map.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<Mutex<BTreeMap<String, RegisteredTool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Async dispatch a tool by name with the given JSON args. The result is
    /// always a structured [`ToolOutcome`]: an unknown tool, a panicking
    /// executor, or an executor `Err` are all normalized to `is_error: true`
    /// instead of propagating as a transport error.
    pub async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        let executor = {
            let tools = self.tools.lock().await;
            match tools.get(name) {
                Some(tool) => Arc::clone(&tool.executor),
                None => {
                    return ToolOutcome::error(format!("unknown tool {name:?}"));
                }
            }
        };

        // Catch executor panics so one tool can never take down the bridge or
        // stall the agent's run; normalize to a structured error.
        let result =
            match tokio::spawn(async move { executor.execute(args).await }).await {
                Ok(inner) => inner,
                Err(join_err) => {
                    return ToolOutcome::error(format!("tool {name:?} panicked: {join_err}"));
                }
            };

        match result {
            Ok(outcome) => outcome,
            Err(err) => ToolOutcome::error(format!("tool {name:?} failed: {err:#}")),
        }
    }

    /// Dispatch a tool and, alongside the structured outcome, produce a
    /// [`ToolCallObservation`] for logging: tool name, read/write metadata,
    /// success/failure, wall-clock duration, and redacted + truncated args and
    /// result summary. `redactor` masks host secrets (and arbitrary token-shaped
    /// strings) before anything is recorded, so secrets never reach a log.
    ///
    /// The outcome returned is byte-for-byte the same one [`dispatch`] returns;
    /// observation is a side-channel and never alters what the agent sees.
    pub async fn dispatch_observed(
        &self,
        name: &str,
        args: Value,
        redactor: &Redactor,
    ) -> (ToolOutcome, ToolCallObservation) {
        let access = {
            let tools = self.tools.lock().await;
            tools.get(name).map(|t| t.spec.access)
        };
        let started = Instant::now();
        let outcome = ToolRegistry::dispatch(self, name, args.clone()).await;
        let elapsed = started.elapsed();
        let observation = ToolCallObservation::build(
            name,
            access.unwrap_or_default(),
            &outcome,
            elapsed,
            &args,
            redactor,
        );
        (outcome, observation)
    }
}

#[async_trait::async_trait]
impl ToolRegistryHandle for ToolRegistry {
    fn register_tool(&self, spec: ToolSpec, executor: Arc<dyn ToolExecutor>) -> Result<()> {
        if spec.name.trim().is_empty() {
            bail!("tool name must not be empty");
        }
        let mut tools = self
            .tools
            .try_lock()
            .map_err(|_| anyhow::anyhow!("tool registry is busy during registration"))?;
        if tools.contains_key(&spec.name) {
            bail!(
                "duplicate tool name {:?}: a tool with this name is already registered \
                 (names must be globally unique; no auto-prefixing)",
                spec.name
            );
        }
        tools.insert(
            spec.name.clone(),
            RegisteredTool { spec, executor },
        );
        Ok(())
    }

    fn list(&self) -> Vec<ToolSpec> {
        // `dispatch` clones the executor `Arc` and releases the lock before any
        // await, and registration is synchronous, so the lock is only ever held
        // briefly. Fall back to an empty list on the (unreachable) contended
        // case rather than panicking the bridge.
        match self.tools.try_lock() {
            Ok(tools) => tools.values().map(|t| t.spec.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.tools
            .try_lock()
            .map(|t| t.is_empty())
            .unwrap_or(false)
    }

    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        ToolRegistry::dispatch(self, name, args).await
    }

    async fn dispatch_observed(
        &self,
        name: &str,
        args: Value,
        redactor: &Redactor,
    ) -> (ToolOutcome, ToolCallObservation) {
        ToolRegistry::dispatch_observed(self, name, args, redactor).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoUpper;

    #[async_trait::async_trait]
    impl ToolExecutor for EchoUpper {
        async fn execute(&self, args: Value) -> Result<ToolOutcome> {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing 'text' string argument"))?;
            Ok(ToolOutcome::ok(text.to_uppercase()))
        }
    }

    struct AlwaysErrOutcome;

    #[async_trait::async_trait]
    impl ToolExecutor for AlwaysErrOutcome {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            Ok(ToolOutcome::error("structured failure"))
        }
    }

    struct AlwaysFault;

    #[async_trait::async_trait]
    impl ToolExecutor for AlwaysFault {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            bail!("unexpected fault")
        }
    }

    struct Panics;

    #[async_trait::async_trait]
    impl ToolExecutor for Panics {
        async fn execute(&self, _args: Value) -> Result<ToolOutcome> {
            panic!("boom")
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            "desc",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        )
    }

    #[tokio::test]
    async fn registers_and_dispatches() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("echo_upper"), Arc::new(EchoUpper))
            .unwrap();

        let out = reg
            .dispatch("echo_upper", json!({ "text": "hello from spike" }))
            .await;
        assert_eq!(out, ToolOutcome::ok("HELLO FROM SPIKE"));
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn duplicate_name_is_an_error() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("echo_upper"), Arc::new(EchoUpper))
            .unwrap();
        let err = reg
            .register_tool(spec("echo_upper"), Arc::new(EchoUpper))
            .unwrap_err();
        assert!(err.to_string().contains("duplicate tool name"));
        // The first registration survives; no last-writer-wins.
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn empty_name_rejected() {
        let reg = ToolRegistry::new();
        let err = reg
            .register_tool(spec(""), Arc::new(EchoUpper))
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn list_exposes_schema_metadata_sorted() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("b_tool"), Arc::new(EchoUpper)).unwrap();
        reg.register_tool(spec("a_tool"), Arc::new(EchoUpper)).unwrap();
        let names: Vec<_> = reg.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a_tool", "b_tool"]);

        let first = &reg.list()[0];
        assert_eq!(first.input_schema["properties"]["text"]["type"], "string");
        // MCP rendering uses camelCase `inputSchema`.
        let mcp = first.to_mcp_tool();
        assert!(mcp.get("inputSchema").is_some());
        assert_eq!(mcp["name"], "a_tool");
    }

    #[tokio::test]
    async fn unknown_tool_is_structured_error_not_panic() {
        let reg = ToolRegistry::new();
        let out = reg.dispatch("nope", json!({})).await;
        assert!(out.is_error);
        assert!(out.text.contains("unknown tool"));
    }

    #[tokio::test]
    async fn executor_ok_error_outcome_passes_through() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("fails"), Arc::new(AlwaysErrOutcome))
            .unwrap();
        let out = reg.dispatch("fails", json!({})).await;
        assert!(out.is_error);
        assert_eq!(out.text, "structured failure");
    }

    #[tokio::test]
    async fn executor_err_is_normalized() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("faults"), Arc::new(AlwaysFault))
            .unwrap();
        let out = reg.dispatch("faults", json!({})).await;
        assert!(out.is_error);
        assert!(out.text.contains("unexpected fault"));
    }

    #[tokio::test]
    async fn executor_panic_is_normalized_and_does_not_stall() {
        let reg = ToolRegistry::new();
        reg.register_tool(spec("panics"), Arc::new(Panics)).unwrap();
        let out = reg.dispatch("panics", json!({})).await;
        assert!(out.is_error);
        assert!(out.text.contains("panicked"));

        // Registry is still usable after a panicking call.
        reg.register_tool(spec("echo_upper"), Arc::new(EchoUpper))
            .unwrap();
        let ok = reg.dispatch("echo_upper", json!({ "text": "ok" })).await;
        assert_eq!(ok, ToolOutcome::ok("OK"));
    }

    #[test]
    fn mcp_result_shape_matches_protocol() {
        let result = ToolOutcome::error("bad").to_mcp_result();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "bad");
    }
}
