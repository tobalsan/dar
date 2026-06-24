# Shim transport + C1–C7 conformance (reliability floor)

The **shim transport** (`crates/tool-shim`) is the runner-agnostic, portable
fallback for exposing host-registered extension tools to an agent. It is the
*defined reliability floor, not the default* (ALG-254): the native host MCP
bridge (`crates/dar-cli/src/bridge.rs`) stays the preferred surface, and
the shim is used where native MCP is unavailable or unreliable (e.g. the pi
init race, or backend/version drift that breaks native tool discovery).

Both paths share the **same** `ToolRegistry`, the **same** structured
`ToolOutcome`, and the **same** observability signal. Only the transport
differs: the shim rides each runner's existing turn loop
(`TurnDecision::Continue { prompt }` / `send_turn`) instead of a JSON-RPC stdio
channel.

## The four moves

1. **Advertise** — `advertise_prompt(&[ToolSpec])` renders the strict prompt
   convention (below). Prepend it on the agent's first turn.
2. **Parse** — `parse_tool_call(output)` scans agent output for the strict
   marker. A well-formed marker yields a `ShimToolCall`; a malformed one yields
   a structured `ShimParseError` (never a panic, never a silent drop).
3. **Host-execute + correlated return** — `ShimTransport::handle_output` parses,
   dispatches through the registry (host runtime, real config/secrets), and
   returns a *continuation prompt* carrying the structured result. The agent's
   `call_id` is echoed so the result correlates to its call.
4. **Structured failure, no stall** — an unknown tool, a panicking executor, or
   a malformed marker all become a `RESULT` continuation prompt with an error,
   so the session always advances to another turn instead of hanging.

## Strict prompt convention

`advertise_prompt` emits a `# Host tools available` section documenting the
marker grammar, then one `### \`<name>\`` block per tool with its description
and JSON Schema. The agent calls a tool by emitting **exactly** this marker on
its own lines, then stopping to wait for the host's reply:

```text
<<<DAR_TOOL_CALL
{"call_id": "<your-id>", "name": "<tool-name>", "arguments": { ... }}
DAR_TOOL_CALL>>>
```

Rules:

- Each fence (`<<<DAR_TOOL_CALL` / `DAR_TOOL_CALL>>>`) is on its own
  line.
- The middle is a single JSON object with a non-empty `name` (a registered
  tool) and an `arguments` object matching that tool's schema.
- `call_id` is an opaque string the agent chooses, echoed back for correlation.
- At most one tool call per turn.

The host replies as a symmetric result block which the agent reads on its next
turn:

```text
<<<DAR_TOOL_RESULT
{"call_id": "<echoed>", "name": "<tool>", "isError": false, "content": "<text>"}
DAR_TOOL_RESULT>>>
```

`isError: true` is a structured failure (bad arguments, unknown tool, malformed
marker, executor fault) — it is **data returned to the agent**, not a transport
error, so the run continues.

## C1–C7 conformance harness

`crates/tool-shim/tests/conformance.rs` asserts the same seven observable
behaviors against **two** surfaces — the shim and the native MCP bridge —
wired to the same registry and observation log, proving they are behaviorally
interchangeable:

| Code | Behavior | Assertion |
|------|----------|-----------|
| C1 | advertise | the surface advertises the registry's tools |
| C2 | invoke | a tool is invoked by name + args |
| C3 | host-execute | the registry executor actually runs (witnessed) |
| C4 | correlated-return | the result is correlated to its call (`call_id` / JSON-RPC id) |
| C5 | structured-failure-no-stall | a bad call returns a structured error and the surface keeps serving |
| C6 | observability | each call exposes the same structured outcome (`ToolOutcome`) regardless of transport — the shim through its observer hook, the native bridge through its result envelope |
| C7 | continuation | after a call the surface yields the next turn |

A third test asserts cross-surface parity: identical advertised tools and an
identical structured result for an identical call. Run it with:

```sh
cargo test -p tool-shim
```
