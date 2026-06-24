//! C1–C7 conformance harness.
//!
//! The same seven observable behaviors are asserted against **two** surfaces:
//!
//!   * the **shim transport** ([`tool_shim`]) — the portable reliability floor,
//!     driven through its advertise/parse/dispatch/continuation API; and
//!   * a **native surface** — the host MCP bridge ([`dar_cli_core::bridge`])
//!     driven in-process over byte pipes as a real JSON-RPC 2.0 stdio server.
//!
//! Both surfaces are wired to the *same* [`ToolRegistry`] and observe the
//! *same* structured [`ToolOutcome`], so the harness proves the two transports
//! are behaviorally interchangeable — which is exactly the "shim = reliability
//! floor with parity to native" guarantee from ALG-254/ALG-260. (There is no
//! shared host observability layer yet; the parity asserted here is over the
//! structured outcome data, which both transports expose identically.)
//!
//! The conformance dimensions:
//!
//!   C1 advertise               — the surface advertises the registry's tools.
//!   C2 invoke                  — a tool can be invoked by name + args.
//!   C3 host-execute            — the call runs in the host registry (real
//!                                executor), not the agent.
//!   C4 correlated-return       — the result is correlated to its call.
//!   C5 structured-failure-no-stall — a bad call returns a structured error and
//!                                the surface keeps serving (no stall).
//!   C6 observability           — each call emits the same structured outcome
//!                                signal regardless of transport.
//!   C7 continuation            — after a call the surface yields the next turn
//!                                (a continuation prompt for the shim; the next
//!                                request for the native bridge).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};
use tool_registry::{ToolExecutor, ToolOutcome, ToolRegistry, ToolRegistryHandle, ToolSpec};
use tool_shim::{ShimObservation, ShimTransport, ShimTurn, CALL_BEGIN, CALL_END};

// ---------------------------------------------------------------------------
// Shared fixture: an echo_upper tool that executes IN THE HOST registry.
// ---------------------------------------------------------------------------

/// Flips true the moment the executor body runs, proving host-execution (C3).
#[derive(Clone, Default)]
struct ExecutionWitness(Arc<Mutex<bool>>);

impl ExecutionWitness {
    fn ran(&self) -> bool {
        *self.0.lock().unwrap()
    }
}

struct EchoUpper {
    witness: ExecutionWitness,
}

#[async_trait::async_trait]
impl ToolExecutor for EchoUpper {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        *self.witness.0.lock().unwrap() = true;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing 'text'"))?;
        Ok(ToolOutcome::ok(text.to_uppercase()))
    }
}

fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo_upper",
        "Uppercase the input text.",
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }),
    )
}

/// One observed tool call: the parity signal both surfaces feed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observed {
    name: String,
    is_error: bool,
    text: String,
}

/// A registry plus an observation log, shared by a surface under test.
struct Fixture {
    registry: Arc<dyn ToolRegistryHandle>,
    witness: ExecutionWitness,
    observed: Arc<Mutex<Vec<Observed>>>,
}

fn fixture() -> Fixture {
    let witness = ExecutionWitness::default();
    let reg = ToolRegistry::new();
    reg.register_tool(
        echo_spec(),
        Arc::new(EchoUpper {
            witness: witness.clone(),
        }),
    )
    .unwrap();
    Fixture {
        registry: Arc::new(reg),
        witness,
        observed: Arc::new(Mutex::new(Vec::new())),
    }
}

// ---------------------------------------------------------------------------
// Surface abstraction: the seven observable behaviors, transport-agnostic.
// ---------------------------------------------------------------------------

/// A tool result observed at a surface boundary, normalized across transports.
struct SurfaceResult {
    /// Correlation id echoed back with the result (C4). The native bridge uses
    /// the JSON-RPC request id; the shim uses the agent's `call_id`.
    correlation: String,
    is_error: bool,
    text: String,
}

/// The behaviors a surface must expose for the harness. Implemented once for the
/// shim and once for the native MCP bridge.
#[async_trait::async_trait]
trait ToolSurface {
    /// C1: tool names advertised to the agent.
    async fn advertised_tools(&self) -> Vec<String>;

    /// C2/C3/C4/C6: invoke `name` with `args`, correlating with `call_id`.
    /// Returns the correlated result. Implementations MUST record an
    /// observation for C6.
    async fn invoke(&self, call_id: &str, name: &str, args: Value) -> SurfaceResult;

    /// C5: invoke something malformed/invalid and return the structured result;
    /// the surface MUST remain usable afterward.
    async fn invoke_malformed(&self) -> SurfaceResult;

    /// C7: whether, after a successful call, the surface yields a follow-up turn
    /// (a continuation prompt / the ability to take another request).
    async fn yields_continuation(&self, call_id: &str) -> bool;
}

// ---------------------------------------------------------------------------
// Surface 1: the shim transport.
// ---------------------------------------------------------------------------

struct ShimSurface {
    shim: ShimTransport,
    observed: Arc<Mutex<Vec<Observed>>>,
}

impl ShimSurface {
    fn new(fx: &Fixture) -> Self {
        let observed = Arc::clone(&fx.observed);
        let sink = Arc::clone(&observed);
        let shim = ShimTransport::new(Arc::clone(&fx.registry)).with_observer(Arc::new(
            move |obs: &ShimObservation<'_>| {
                sink.lock().unwrap().push(Observed {
                    name: obs.name.to_string(),
                    is_error: obs.outcome.is_error,
                    text: obs.outcome.text.clone(),
                });
            },
        ));
        Self { shim, observed }
    }

    /// Simulate the agent emitting a strict tool-call marker for `name`.
    fn agent_emits_call(call_id: &str, name: &str, args: Value) -> String {
        format!(
            "{CALL_BEGIN}\n{}\n{CALL_END}\n",
            json!({ "call_id": call_id, "name": name, "arguments": args })
        )
    }

    /// Pull the structured result back out of a continuation prompt.
    fn parse_result_prompt(prompt: &str) -> SurfaceResult {
        // The prompt holds a RESULT block whose middle line is the JSON payload.
        let json_line = prompt
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .expect("result prompt carries a JSON payload line");
        let v: Value = serde_json::from_str(json_line.trim()).unwrap();
        SurfaceResult {
            correlation: v["call_id"].as_str().unwrap_or("").to_string(),
            is_error: v["isError"].as_bool().unwrap_or(false),
            text: v["content"].as_str().unwrap_or("").to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ToolSurface for ShimSurface {
    async fn advertised_tools(&self) -> Vec<String> {
        // The advertise prompt documents each tool under a `### \`<name>\`` head.
        let prompt = self.shim.advertise();
        assert!(
            prompt.contains(CALL_BEGIN),
            "advertise documents the marker"
        );
        prompt
            .lines()
            .filter_map(|l| l.strip_prefix("### `").and_then(|s| s.strip_suffix("`")))
            .map(str::to_string)
            .collect()
    }

    async fn invoke(&self, call_id: &str, name: &str, args: Value) -> SurfaceResult {
        let output = Self::agent_emits_call(call_id, name, args);
        match self.shim.handle_output(&output).await {
            ShimTurn::Continue { prompt } => Self::parse_result_prompt(&prompt),
            ShimTurn::NoToolCall => panic!("a marker was emitted; expected a tool call"),
        }
    }

    async fn invoke_malformed(&self) -> SurfaceResult {
        // A present-but-malformed marker: opened, garbage body, closed.
        let output = format!("{CALL_BEGIN}\nnot valid json\n{CALL_END}\n");
        let before = self.observed.lock().unwrap().len();
        let result = match self.shim.handle_output(&output).await {
            ShimTurn::Continue { prompt } => Self::parse_result_prompt(&prompt),
            ShimTurn::NoToolCall => panic!("malformed marker must still continue"),
        };
        // Surface stays usable: a normal call right after still works.
        let after = self
            .invoke("recover", "echo_upper", json!({ "text": "still alive" }))
            .await;
        assert_eq!(
            after.text, "STILL ALIVE",
            "surface stalled after malformed call"
        );
        // The malformed attempt was observed too (structured, not swallowed).
        assert!(self.observed.lock().unwrap().len() > before);
        result
    }

    async fn yields_continuation(&self, call_id: &str) -> bool {
        let output = Self::agent_emits_call(call_id, "echo_upper", json!({ "text": "again" }));
        matches!(
            self.shim.handle_output(&output).await,
            ShimTurn::Continue { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Surface 2: the native host MCP bridge (JSON-RPC 2.0 over pipes).
// ---------------------------------------------------------------------------

struct NativeBridgeSurface {
    registry: Arc<dyn ToolRegistryHandle>,
    observed: Arc<Mutex<Vec<Observed>>>,
}

impl NativeBridgeSurface {
    fn new(fx: &Fixture) -> Self {
        Self {
            registry: Arc::clone(&fx.registry),
            observed: Arc::clone(&fx.observed),
        }
    }

    /// Drive one batch of JSON-RPC requests through the real bridge server and
    /// collect the responses (the bridge serves until EOF).
    async fn rpc(&self, requests: &[Value]) -> Vec<Value> {
        let mut input = String::new();
        for req in requests {
            input.push_str(&serde_json::to_string(req).unwrap());
            input.push('\n');
        }
        let mut output: Vec<u8> = Vec::new();
        dar_cli_core::bridge::serve_stdio(
            Arc::clone(&self.registry),
            tool_registry::Redactor::default(),
            input.as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// A `tools/call` request whose JSON-RPC id is the correlation id.
    fn call_request(id: &str, name: &str, args: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args },
        })
    }

    /// Normalize an MCP `tools/call` result envelope to a `SurfaceResult`, and
    /// record an observation derived from it (C6 parity: the native transport
    /// has no production observability hook of its own, but its result envelope
    /// carries the identical structured `ToolOutcome` data the shim observer
    /// emits — so the harness can assert the observable outcome is the same).
    fn observe_result(&self, name: &str, resp: &Value) -> SurfaceResult {
        let result = &resp["result"];
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        self.observed.lock().unwrap().push(Observed {
            name: name.to_string(),
            is_error,
            text: text.clone(),
        });
        SurfaceResult {
            correlation: resp["id"].as_str().unwrap_or("").to_string(),
            is_error,
            text,
        }
    }
}

#[async_trait::async_trait]
impl ToolSurface for NativeBridgeSurface {
    async fn advertised_tools(&self) -> Vec<String> {
        let out = self
            .rpc(&[json!({ "jsonrpc": "2.0", "id": "list", "method": "tools/list" })])
            .await;
        out[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    async fn invoke(&self, call_id: &str, name: &str, args: Value) -> SurfaceResult {
        let out = self.rpc(&[Self::call_request(call_id, name, args)]).await;
        self.observe_result(name, &out[0])
    }

    async fn invoke_malformed(&self) -> SurfaceResult {
        // The native analogue of a malformed call: a call for an unknown tool.
        // It returns a structured `isError` result (not a transport error), and
        // the SAME server keeps serving the next request in the batch.
        let out = self
            .rpc(&[
                Self::call_request("bad", "does_not_exist", json!({})),
                // Proof the surface did not stall: a follow-up succeeds.
                Self::call_request("recover", "echo_upper", json!({ "text": "still alive" })),
            ])
            .await;
        // No JSON-RPC transport error: the failure is a result.
        assert!(
            out[0].get("error").is_none(),
            "must be structured, not transport error"
        );
        let recover = self.observe_result("echo_upper", &out[1]);
        assert_eq!(
            recover.text, "STILL ALIVE",
            "surface stalled after bad call"
        );
        self.observe_result("does_not_exist", &out[0])
    }

    async fn yields_continuation(&self, call_id: &str) -> bool {
        // After a call, the same server accepts another request in-session.
        let out = self
            .rpc(&[Self::call_request(
                call_id,
                "echo_upper",
                json!({ "text": "again" }),
            )])
            .await;
        out[0].get("result").is_some() && out[0]["id"] == call_id
    }
}

// ---------------------------------------------------------------------------
// The C1–C7 harness, run once per surface.
// ---------------------------------------------------------------------------

/// Run all seven conformance checks against `surface`, backed by `fx`.
async fn run_conformance<S: ToolSurface>(surface: &S, fx: &Fixture, label: &str) {
    // C1 advertise.
    let tools = surface.advertised_tools().await;
    assert!(
        tools.contains(&"echo_upper".to_string()),
        "[{label}] C1 advertise: echo_upper not advertised (got {tools:?})"
    );

    // C2 invoke + C3 host-execute + C4 correlated-return.
    assert!(
        !fx.witness.ran(),
        "[{label}] executor ran before any invoke"
    );
    let r = surface
        .invoke("call-42", "echo_upper", json!({ "text": "hello floor" }))
        .await;
    assert_eq!(r.text, "HELLO FLOOR", "[{label}] C2 invoke: wrong result");
    assert!(
        fx.witness.ran(),
        "[{label}] C3 host-execute: registry executor did not run"
    );
    assert_eq!(
        r.correlation, "call-42",
        "[{label}] C4 correlated-return: result not correlated to its call"
    );
    assert!(!r.is_error, "[{label}] C2: unexpected error result");

    // C5 structured-failure-no-stall.
    let bad = surface.invoke_malformed().await;
    assert!(
        bad.is_error,
        "[{label}] C5: malformed/invalid call must be a structured error"
    );

    // C7 continuation (asserted before draining observations).
    assert!(
        surface.yields_continuation("call-cont").await,
        "[{label}] C7 continuation: surface did not yield a follow-up turn"
    );

    // C6 observability: every call emitted the same structured outcome signal.
    let observed = fx.observed.lock().unwrap();
    assert!(
        observed
            .iter()
            .any(|o| o.name == "echo_upper" && !o.is_error),
        "[{label}] C6 observability: success outcome not observed (got {observed:?})"
    );
    assert!(
        observed.iter().any(|o| o.is_error),
        "[{label}] C6 observability: structured failure not observed"
    );
}

#[tokio::test]
async fn shim_surface_passes_c1_through_c7() {
    let fx = fixture();
    let surface = ShimSurface::new(&fx);
    run_conformance(&surface, &fx, "shim").await;
}

#[tokio::test]
async fn native_bridge_surface_passes_c1_through_c7() {
    let fx = fixture();
    let surface = NativeBridgeSurface::new(&fx);
    run_conformance(&surface, &fx, "native-mcp-bridge").await;
}

/// Cross-surface parity: the two transports advertise the SAME tools and return
/// the SAME structured result for the SAME call — the shim is a true floor, not
/// a divergent path.
#[tokio::test]
async fn shim_and_native_agree_on_observable_behavior() {
    let shim_fx = fixture();
    let shim = ShimSurface::new(&shim_fx);
    let native_fx = fixture();
    let native = NativeBridgeSurface::new(&native_fx);

    // C1 parity.
    assert_eq!(
        shim.advertised_tools().await,
        native.advertised_tools().await,
        "shim and native advertise different tools"
    );

    // C2/C3/C4 parity on an identical call.
    let s = shim
        .invoke("x", "echo_upper", json!({ "text": "parity" }))
        .await;
    let n = native
        .invoke("x", "echo_upper", json!({ "text": "parity" }))
        .await;
    assert_eq!(s.text, n.text, "result text diverged");
    assert_eq!(s.is_error, n.is_error, "error flag diverged");
    assert_eq!(s.correlation, n.correlation, "correlation diverged");

    // C5 parity: the SAME fault (unknown-tool call) through both surfaces
    // produces the same structured error outcome.
    let s_bad = shim.invoke("bad", "does_not_exist", json!({})).await;
    let n_bad = native.invoke("bad", "does_not_exist", json!({})).await;
    assert!(
        s_bad.is_error,
        "shim: unknown-tool call must be structured error"
    );
    assert!(
        n_bad.is_error,
        "native: unknown-tool call must be structured error"
    );
    assert_eq!(
        s_bad.is_error, n_bad.is_error,
        "C5 parity: error flag diverged"
    );
}
