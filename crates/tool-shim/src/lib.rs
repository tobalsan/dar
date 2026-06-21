//! Runner-agnostic **shim transport** — the portable reliability floor for
//! exposing host-registered extension tools to an agent when native MCP is
//! unavailable or unreliable (e.g. the pi init race; backend/version drift).
//!
//! Per ALG-254 the shim is the *defined reliability floor, not the default*. The
//! native host MCP bridge ([`agentropy_cli::bridge`]) stays the preferred
//! surface; the shim is the fallback that guarantees parity by exercising the
//! **same** [`ToolRegistry`] and the **same** structured [`ToolOutcome`]
//! observability — only the transport differs.
//!
//! ## How it works
//!
//! Every runner already supports a turn loop: it pauses at a turn boundary and
//! the host replies with `TurnDecision::Continue { prompt }` / `send_turn`. The
//! shim rides that loop, with no backend-specific protocol:
//!
//! 1. **Advertise** ([`advertise_prompt`]) — render the registry's tool specs
//!    into a strict prompt convention the agent reads on its first turn. It
//!    documents the tools (name, description, JSON schema) and the exact
//!    tool-call marker grammar the agent must emit to call one.
//! 2. **Parse** ([`parse_tool_call`]) — scan agent output for the strict marker.
//!    A well-formed marker yields a [`ShimToolCall`]; a malformed marker yields
//!    a structured [`ShimParseError`] (never a panic, never a silent drop).
//! 3. **Host-execute + correlated return** ([`ShimTransport::handle_output`]) —
//!    dispatch the parsed call through the registry (host runtime, real
//!    config/secrets), then format the structured result back into a
//!    *continuation prompt* the caller feeds to the runner as the next turn.
//!    The `call_id` echoed in the result correlates it to the originating call.
//! 4. **Structured failure, no stall** — an unknown tool, a panicking executor,
//!    or a malformed marker all become a `RESULT` continuation prompt carrying
//!    an error, so the session always advances to another turn instead of
//!    hanging.
//!
//! The marker is deliberately line-oriented and fenced so it is unambiguous to
//! both emit (for the agent) and parse (for the host), independent of any
//! backend's own tool-call format.

use std::sync::Arc;

use serde_json::Value;
use tool_registry::{ToolOutcome, ToolRegistryHandle, ToolSpec};

/// Opening fence of the strict tool-call marker. The agent emits a single line
/// equal to this, then a JSON object line, then [`CALL_END`].
pub const CALL_BEGIN: &str = "<<<AGENTROPY_TOOL_CALL";

/// Closing fence of the strict tool-call marker.
pub const CALL_END: &str = "AGENTROPY_TOOL_CALL>>>";

/// Opening fence the host uses when rendering a tool result back into the
/// continuation prompt. Symmetric with the call marker so a transcript reads
/// call → result unambiguously.
pub const RESULT_BEGIN: &str = "<<<AGENTROPY_TOOL_RESULT";

/// Closing fence of the result marker.
pub const RESULT_END: &str = "AGENTROPY_TOOL_RESULT>>>";

/// A parsed, well-formed tool call extracted from agent output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShimToolCall {
    /// Opaque id the agent assigned so the returned result can be correlated to
    /// this call. Empty string when the agent omitted it.
    pub call_id: String,
    /// The registry tool name to dispatch.
    pub name: String,
    /// The arguments object passed to the executor.
    pub arguments: Value,
}

/// Why a tool-call marker failed to parse. These are *structured* failures: the
/// transport turns them into an error continuation prompt rather than stalling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShimParseError {
    /// No `CALL_BEGIN` fence was present in the output at all.
    NoMarker,
    /// A `CALL_BEGIN` fence was opened but never closed with `CALL_END`.
    Unterminated,
    /// The body between the fences was not a single JSON object.
    MalformedJson(String),
    /// The JSON object was missing the required `name` field (or it was empty).
    MissingName,
}

impl ShimParseError {
    /// Human-readable, agent-facing reason. Surfaced verbatim in the error
    /// continuation prompt so the agent can correct itself on the next turn.
    pub fn reason(&self) -> String {
        match self {
            ShimParseError::NoMarker => "no tool-call marker found".to_string(),
            ShimParseError::Unterminated => format!(
                "tool-call marker opened with `{CALL_BEGIN}` but never closed with `{CALL_END}`"
            ),
            ShimParseError::MalformedJson(detail) => {
                format!("tool-call body was not a single JSON object: {detail}")
            }
            ShimParseError::MissingName => {
                "tool-call JSON missing required non-empty \"name\" field".to_string()
            }
        }
    }
}

/// Render the strict prompt convention advertising `tools` to the agent. This is
/// the documented contract: it lists each tool (name, description, JSON schema)
/// and specifies the exact marker the agent must emit to call one.
///
/// Returns an empty string when there are no tools (nothing to advertise), so a
/// caller can unconditionally prepend it.
pub fn advertise_prompt(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("# Host tools available\n\n");
    out.push_str(
        "You can call the following host-provided tools. To call a tool, emit a \
         marker on its own lines exactly as shown, then STOP and wait — the host \
         executes the tool and replies with the result on your next turn:\n\n",
    );
    out.push_str(CALL_BEGIN);
    out.push('\n');
    out.push_str("{\"call_id\": \"<your-id>\", \"name\": \"<tool-name>\", \"arguments\": { ... }}\n");
    out.push_str(CALL_END);
    out.push_str("\n\n");
    out.push_str(
        "Rules: the marker fences must each be on their own line; the middle line \
         must be a single JSON object with a `name` (a tool below) and an \
         `arguments` object matching its schema; `call_id` is an opaque string \
         you choose so you can correlate the reply. Emit at most one tool call \
         per turn. The host replies with a matching `",
    );
    out.push_str(RESULT_BEGIN);
    out.push_str("` block.\n\n");
    out.push_str("## Tools\n\n");
    for spec in tools {
        out.push_str(&format!("### `{}`\n\n", spec.name));
        out.push_str(&format!("{}\n\n", spec.description));
        out.push_str("Arguments schema (JSON Schema):\n\n```json\n");
        out.push_str(
            &serde_json::to_string_pretty(&spec.input_schema)
                .unwrap_or_else(|_| spec.input_schema.to_string()),
        );
        out.push_str("\n```\n\n");
    }
    out
}

/// Parse the FIRST well-formed tool-call marker out of `output`.
///
/// Strict grammar (line-oriented):
///
/// ```text
/// <<<AGENTROPY_TOOL_CALL
/// { "call_id": "...", "name": "...", "arguments": { ... } }
/// AGENTROPY_TOOL_CALL>>>
/// ```
///
/// The body is the lines between the fences joined back with newlines, parsed as
/// a single JSON object. Returns a structured [`ShimParseError`] for every
/// malformed case so the caller never stalls.
pub fn parse_tool_call(output: &str) -> Result<ShimToolCall, ShimParseError> {
    let mut lines = output.lines();
    // Find the opening fence (trimmed, on its own line).
    loop {
        match lines.next() {
            Some(line) if line.trim() == CALL_BEGIN => break,
            Some(_) => continue,
            None => return Err(ShimParseError::NoMarker),
        }
    }

    // Collect body lines until the closing fence.
    let mut body = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == CALL_END {
            closed = true;
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    if !closed {
        return Err(ShimParseError::Unterminated);
    }

    let value: Value = serde_json::from_str(body.trim())
        .map_err(|e| ShimParseError::MalformedJson(e.to_string()))?;
    let Some(obj) = value.as_object() else {
        return Err(ShimParseError::MalformedJson(
            "expected a JSON object".to_string(),
        ));
    };

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or(ShimParseError::MissingName)?;

    let call_id = obj
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    Ok(ShimToolCall {
        call_id,
        name,
        arguments,
    })
}

/// Render a tool result (or a parse failure) back into a continuation prompt the
/// caller feeds to the runner as the next turn. The `call_id` is echoed so the
/// agent can correlate the result to its originating call (C4).
pub fn result_prompt(call_id: &str, name: &str, outcome: &ToolOutcome) -> String {
    let payload = serde_json::json!({
        "call_id": call_id,
        "name": name,
        "isError": outcome.is_error,
        "content": outcome.text,
    });
    format!(
        "{RESULT_BEGIN}\n{}\n{RESULT_END}\n",
        serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string())
    )
}

/// Observability hook: invoked once per shim tool call with the correlation id,
/// tool name, and structured outcome. It carries the identical structured
/// [`ToolOutcome`] the native bridge returns in its result envelope, so a tool
/// call is observable with the same data regardless of transport.
pub type ShimObserver = Arc<dyn Fn(&ShimObservation<'_>) + Send + Sync>;

/// What the shim observed for one tool-call attempt.
pub struct ShimObservation<'a> {
    pub call_id: &'a str,
    pub name: &'a str,
    pub outcome: &'a ToolOutcome,
}

/// The outcome of feeding one agent turn's output through the shim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShimTurn {
    /// The output contained no tool-call marker; the turn is a plain agent
    /// message and the caller should finish (or apply its own turn policy).
    NoToolCall,
    /// A tool call was handled (successfully or as a structured failure). Feed
    /// `prompt` to the runner as the next turn (`Continue { prompt }`).
    Continue { prompt: String },
}

/// The shim transport: binds the shared registry and an optional observability
/// hook, and turns one agent turn's output into the next continuation prompt.
#[derive(Clone)]
pub struct ShimTransport {
    registry: Arc<dyn ToolRegistryHandle>,
    observer: Option<ShimObserver>,
}

impl ShimTransport {
    /// Bind the shim to the SAME registry the native MCP bridge serves.
    pub fn new(registry: Arc<dyn ToolRegistryHandle>) -> Self {
        Self {
            registry,
            observer: None,
        }
    }

    /// Attach an observability hook (the native-parity signal).
    pub fn with_observer(mut self, observer: ShimObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The advertise prompt for this transport's registry (C1).
    pub fn advertise(&self) -> String {
        advertise_prompt(&self.registry.list())
    }

    /// Process one agent turn's `output`:
    ///   - no marker → [`ShimTurn::NoToolCall`];
    ///   - well-formed marker → dispatch through the registry, observe, and
    ///     return a result continuation prompt;
    ///   - malformed marker → a structured *error* continuation prompt (no
    ///     stall), still observed.
    pub async fn handle_output(&self, output: &str) -> ShimTurn {
        match parse_tool_call(output) {
            Ok(call) => {
                let outcome = self
                    .registry
                    .dispatch(&call.name, call.arguments.clone())
                    .await;
                self.observe(&call.call_id, &call.name, &outcome);
                ShimTurn::Continue {
                    prompt: result_prompt(&call.call_id, &call.name, &outcome),
                }
            }
            Err(ShimParseError::NoMarker) => ShimTurn::NoToolCall,
            Err(err) => {
                // Malformed-but-present marker: fail structured, do not stall.
                let outcome = ToolOutcome::error(format!("malformed tool call: {}", err.reason()));
                self.observe("", "<malformed>", &outcome);
                ShimTurn::Continue {
                    prompt: result_prompt("", "<malformed>", &outcome),
                }
            }
        }
    }

    fn observe(&self, call_id: &str, name: &str, outcome: &ToolOutcome) {
        if let Some(observer) = &self.observer {
            observer(&ShimObservation {
                call_id,
                name,
                outcome,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::json;
    use std::sync::Mutex;
    use tool_registry::{ToolExecutor, ToolRegistry};

    struct EchoUpper;

    #[async_trait::async_trait]
    impl ToolExecutor for EchoUpper {
        async fn execute(&self, args: Value) -> Result<ToolOutcome> {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing 'text'"))?;
            Ok(ToolOutcome::ok(text.to_uppercase()))
        }
    }

    fn spec() -> ToolSpec {
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

    fn registry() -> Arc<dyn ToolRegistryHandle> {
        let reg = ToolRegistry::new();
        reg.register_tool(spec(), Arc::new(EchoUpper)).unwrap();
        Arc::new(reg)
    }

    fn call_marker(call_id: &str, name: &str, args: Value) -> String {
        format!(
            "Sure, calling it now.\n{CALL_BEGIN}\n{}\n{CALL_END}\n",
            json!({ "call_id": call_id, "name": name, "arguments": args })
        )
    }

    #[test]
    fn advertise_lists_tools_and_marker_grammar() {
        let prompt = advertise_prompt(&[spec()]);
        assert!(prompt.contains(CALL_BEGIN));
        assert!(prompt.contains(CALL_END));
        assert!(prompt.contains("echo_upper"));
        assert!(prompt.contains("Uppercase the input text."));
        // The schema is surfaced so the agent knows the argument shape.
        assert!(prompt.contains("\"text\""));
    }

    #[test]
    fn advertise_empty_when_no_tools() {
        assert_eq!(advertise_prompt(&[]), "");
    }

    #[test]
    fn parse_extracts_well_formed_call() {
        let out = call_marker("c1", "echo_upper", json!({ "text": "hi" }));
        let call = parse_tool_call(&out).unwrap();
        assert_eq!(call.call_id, "c1");
        assert_eq!(call.name, "echo_upper");
        assert_eq!(call.arguments, json!({ "text": "hi" }));
    }

    #[test]
    fn parse_no_marker_is_structured() {
        assert_eq!(
            parse_tool_call("just a plain message"),
            Err(ShimParseError::NoMarker)
        );
    }

    #[test]
    fn parse_unterminated_is_structured() {
        let out = format!("{CALL_BEGIN}\n{{\"name\":\"x\"}}\n");
        assert_eq!(parse_tool_call(&out), Err(ShimParseError::Unterminated));
    }

    #[test]
    fn parse_malformed_json_is_structured() {
        let out = format!("{CALL_BEGIN}\nnot json\n{CALL_END}\n");
        assert!(matches!(
            parse_tool_call(&out),
            Err(ShimParseError::MalformedJson(_))
        ));
    }

    #[test]
    fn parse_missing_name_is_structured() {
        let out = format!("{CALL_BEGIN}\n{{\"arguments\":{{}}}}\n{CALL_END}\n");
        assert_eq!(parse_tool_call(&out), Err(ShimParseError::MissingName));
    }

    #[tokio::test]
    async fn handle_output_dispatches_and_returns_correlated_result() {
        let shim = ShimTransport::new(registry());
        let out = call_marker("c7", "echo_upper", json!({ "text": "hello" }));
        let turn = shim.handle_output(&out).await;
        let ShimTurn::Continue { prompt } = turn else {
            panic!("expected continue");
        };
        assert!(prompt.contains(RESULT_BEGIN));
        // Correlated by call_id and tool name.
        assert!(prompt.contains("\"call_id\":\"c7\""));
        assert!(prompt.contains("\"name\":\"echo_upper\""));
        assert!(prompt.contains("HELLO"));
        assert!(prompt.contains("\"isError\":false"));
    }

    #[tokio::test]
    async fn handle_output_no_marker_finishes() {
        let shim = ShimTransport::new(registry());
        assert_eq!(
            shim.handle_output("all done, nothing to call").await,
            ShimTurn::NoToolCall
        );
    }

    #[tokio::test]
    async fn malformed_marker_fails_structured_without_stall() {
        let shim = ShimTransport::new(registry());
        let out = format!("{CALL_BEGIN}\nnot json at all\n{CALL_END}\n");
        let turn = shim.handle_output(&out).await;
        let ShimTurn::Continue { prompt } = turn else {
            panic!("malformed marker must still continue, not stall");
        };
        assert!(prompt.contains(RESULT_BEGIN));
        assert!(prompt.contains("\"isError\":true"));
        assert!(prompt.contains("malformed tool call"));
    }

    #[tokio::test]
    async fn unknown_tool_is_structured_error_continuation() {
        let shim = ShimTransport::new(registry());
        let out = call_marker("c9", "does_not_exist", json!({}));
        let ShimTurn::Continue { prompt } = shim.handle_output(&out).await else {
            panic!("expected continue");
        };
        assert!(prompt.contains("\"isError\":true"));
        assert!(prompt.contains("unknown tool"));
    }

    #[tokio::test]
    async fn observer_sees_same_outcome_as_native() {
        let seen: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let shim = ShimTransport::new(registry()).with_observer(Arc::new(move |obs| {
            sink.lock().unwrap().push((
                obs.call_id.to_string(),
                obs.name.to_string(),
                obs.outcome.is_error,
            ));
        }));
        let out = call_marker("obs1", "echo_upper", json!({ "text": "x" }));
        shim.handle_output(&out).await;
        let rows = seen.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("obs1".to_string(), "echo_upper".to_string(), false));
    }
}
