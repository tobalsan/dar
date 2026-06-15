# ALG-254 Spike — Dynamic Tool Contract & Runner Feasibility Matrix

Status: spike result (no production code wired)
Blocks: ALG-253 (PRD: Extension-exposed runtime tools for agents)
Author: worker (automated)

## TL;DR

All three current agent backends — **codex, pi, and opencode — natively support
host-advertised dynamic tools via MCP**, and each was proven end-to-end with a
toy tool (`echo_upper`) returning a correct result. The structured-failure toy
(`always_fail`, `isError:true`) was directly verified on **codex** (surfaced as
`(failed)` without stalling); pi/opencode failure paths are inferred from their
shared result/permission plumbing and are flagged as a follow-up.

This contradicts the spike's prior assumption that only codex was a likely
native path. The practical blocker is **not** runner protocol capability — it is
that **no runner or chat surface currently passes any MCP/tool config to its
backend**. Wiring is additive per surface.

**Recommendation: native-first via an MCP bridge, with a runner-agnostic shim as
the explicit reliability floor.** See [Recommendation](#recommendation).

## Dynamic Tool Capability Contract

A runner/chat surface **supports Agentropy dynamic tools** if it satisfies all
seven observable behaviors below. The contract is behavioral; it does not mandate
a specific transport.

| # | Behavior | Observable test |
|---|----------|-----------------|
| C1 | **Advertise** — Agentropy can provide tool specs (name + JSON schema) to the agent before/during a turn. | Backend lists the tool; agent knows it exists. |
| C2 | **Invoke** — agent can request a tool by name + JSON arguments. | A tool-call event with name + args is emitted. |
| C3 | **Execute in host** — Agentropy runs the tool inside the host runtime using extension config/secrets. | Host process logs the call and produces the result (not the agent sandbox). |
| C4 | **Correlated return** — the result returns to the *same* conversation with a correlation id or clearly matching context. | Result is tied to the originating call id and the same session/thread. |
| C5 | **Structured failure** — malformed/unsupported calls return a structured failure and do **not** stall the run/session. | `isError`/error result; turn finishes; session stays usable. |
| C6 | **Observability** — tool calls/results produce runtime logs: name, success/failure, duration, redacted/truncated args/result. | Log rows exist for call + output. |
| C7 | **Continuation** — conversation context continues after the result so the agent can act on it. | Next turn/step uses the tool result. |

### Transport strategies

- **`native`** — backend protocol supports host-advertised/client-side tools
  directly (here: an MCP server the host controls).
- **`shim`** — Agentropy advertises tools via prompt convention, parses a strict
  tool-call marker from agent output, executes the host tool, and feeds the
  result back as a continuation prompt.
- **`none`** — surface cannot satisfy the contract without upstream/protocol
  changes.

## How each surface works today (evidence)

All file references are in this repo unless noted.

### Backends and their transports

| Surface | Backend transport | Inbound host channel that exists today |
|---------|-------------------|----------------------------------------|
| `runner-pi` / `chat-pi` | `pi --mode rpc` JSONL (`{"type":"prompt"}` in; `message_update`/`tool_execution_*`/`agent_end` out) | `extension_ui_request`→`extension_ui_response` (confirm/select/input/editor) — UI dialogs only, not arbitrary tool exec. `extensions/runner-pi/src/lib.rs:485`, `extensions/chat-pi/src/lib.rs:309` |
| `runner-codex` / `chat-codex` | `codex app-server` JSON-RPC 2.0 (`turn/start` in; `item/*`, `turn/*` out) | Server→client requests are already handled: `requestApproval` answered, unknown methods get `-32601`. `extensions/runner-codex/src/lib.rs:620`, `extensions/chat-codex/src/lib.rs:466` |
| `runner-opencode` / `chat-opencode` | `opencode serve` HTTP + SSE (`/session/{id}/prompt_async` in; `message.part.updated`/`session.idle` out) | `respond_permission` REST verb. `crates/opencode-client/src/lib.rs:238`, runner config writer `extensions/runner-opencode/src/lib.rs:216` |

Key structural fact: every backend already exposes tool-call / tool-output
events for the agent's *own* built-in tools (bash/edit/etc.), and the runners
already classify and log them (`runner-core::classify_protocol_line`,
`classify_opencode_event`). C6 observability is therefore mostly already in
place; the host tool just needs to ride the same log path.

### Native capability per backend — verified with toy tools

Each backend CLI accepts host-controlled MCP servers:

- **codex**: `codex mcp add <name> -- <cmd>` or `-c mcp_servers.<name>.command=...`
- **pi**: `pi --mode rpc --mcp-config <file.json>`
- **opencode**: `mcp` block in `opencode.json` (the runner already writes this file)

Toy experiments (reproducible scripts in `docs/plans/alg-254-spike-artifacts/`):

| Backend | Toy `echo_upper` end-to-end | Structured failure (`always_fail`, `isError:true`) |
|---------|------------------------------|-----------------------------------------------------|
| codex   | PASS — `mcp: toy/echo_upper (completed)` → agent returned `HELLO FROM SPIKE` | PASS — `(failed)` surfaced; agent replied `TOOL ERRORED`; no stall |
| pi      | PASS — `--mcp-config` loaded; returned `HELLO FROM SPIKE` | Did not stall; clean turn end (one run had an MCP init race where the tool wasn't surfaced — see Risks) |
| opencode| PASS — `toy_echo_upper {"text":...}` executed; returned `HELLO FROM SPIKE`; permission evaluated/allowed | Not separately run; opencode routes tool errors through the same result/permission path |

(opencode namespaces MCP tools as `<server>_<tool>`, so the `toy` server's
`echo_upper` appears to the agent as `toy_echo_upper`.)

These prove C1–C4 and C7 natively, and C5 for codex (and pi by non-stall). C6
is covered by existing event classification once the host tool emits on the same
path.

## Per-surface feasibility result

For each surface: strategy, evidence, toy-tool result, result return path,
failure behavior, blocker, confidence.

### runner-pi
- **Strategy: native (via `--mcp-config`)**, shim also viable.
- Evidence: `pi --mcp-config` documented in `pi --help`; toy tool ran end-to-end.
- Toy tool exposed & called: **yes** (`echo_upper` → `HELLO FROM SPIKE`).
- Result return: MCP server returns `tools/call` result; pi feeds it back into the same `--mode rpc` session; runner already streams `tool_execution_end` (`extensions/runner-pi/src/lib.rs:1061`).
- Failure/stall: structured `isError` returns to the agent; turn ends normally; no stall observed.
- Blocker: **not wired** — `runner-pi` does not pass `--mcp-config` today. Minor MCP startup race seen once (Risks).
- Confidence: **High** for native capability; Medium for production reliability until the init race is characterized.

### runner-codex
- **Strategy: native (MCP server config)**, shim also viable.
- Evidence: `codex mcp` subcommand + `-c mcp_servers.*`; app-server already handles inbound server requests (`extensions/runner-codex/src/lib.rs:620`); toy tool ran end-to-end.
- Toy tool exposed & called: **yes** (`echo_upper` → `HELLO FROM SPIKE`).
- Result return: MCP `tools/call` result returns inside the same `threadId`/turn; runner sees `item/*` + `turn/completed`.
- Failure/stall: `isError:true` → `mcp: toyerr/always_fail (failed)`; agent continued and replied `TOOL ERRORED`. **C5 confirmed.**
- Blocker: **not wired** — `codex_args`/`thread_start` add no `mcp_servers`. Codex MCP auth shows `Unsupported` for stdio servers (irrelevant for a local host bridge).
- Confidence: **High**.

### runner-opencode
- **Strategy: native (`mcp` block in `opencode.json`)**, shim also viable.
- Evidence: `opencode mcp` subcommand; runner already writes `opencode.json` (`extensions/runner-opencode/src/lib.rs:216`); toy tool ran end-to-end.
- Toy tool exposed & called: **yes** (`toy_echo_upper` executed; `HELLO FROM SPIKE`).
- Result return: tool result returns over SSE in the same session; runner classifies completed tool parts into `tool_call`/`tool_output` (`extensions/runner-opencode/src/lib.rs:539`).
- Failure/stall: opencode evaluates permission then runs; errors flow through the same result path (`respond_permission` exists for gated tools). Not separately stall-tested.
- Blocker: **not wired** — `opencode_config()` writes no `mcp` block. Permission map must `allow` host tools (currently `"*":"allow"`, so fine).
- Confidence: **High** for capability; Medium for failure-path until stall-tested.

### chat-pi
- **Strategy: native (same `--mcp-config`) + chat continuation**, shim viable.
- Evidence: chat-pi drives the same pi RPC protocol and maps `tool_execution_*` to `ChatEvent::ToolOutput` (`extensions/chat-pi/src/lib.rs:272`). The `ChatSession::send_turn(prompt)` loop supports continuation (C7).
- Toy tool: not separately run for chat, but identical backend mechanism as `runner-pi` (proven).
- Result return: native MCP result returns inside the session; surfaced as `ChatEvent::ToolOutput`.
- Failure/stall: chat maps assistant errors to `TurnFinished{ok:false}`; session stays usable.
- Blocker: **not wired**; chat backends never pass MCP config.
- Confidence: **Medium-High** (capability inherited from runner-pi; not directly demoed in the chat path).

### chat-codex
- **Strategy: native** (same app-server, inbound-request handling at `extensions/chat-codex/src/lib.rs:466`), shim viable.
- Toy tool: inherited from runner-codex (proven backend mechanism).
- Result return: same-thread JSON-RPC result; `ChatEvent::ToolOutput`.
- Failure/stall: `-32601` for unknown server methods; `isError` surfaces; no stall.
- Blocker: **not wired**.
- Confidence: **Medium-High**.

### chat-opencode
- **Strategy: native** (same `opencode serve` + config), shim viable.
- Toy tool: inherited from runner-opencode (proven backend mechanism).
- Result return: SSE tool result in the same session; `ChatEvent::ToolOutput`.
- Failure/stall: permission/result path; `respond_permission` available.
- Blocker: **not wired**; chat path writes config via the same helper.
- Confidence: **Medium-High**.

## Feasibility matrix

| Surface | Best strategy | Toy tool e2e | Result return path | Failure = structured, no stall | Currently wired | Confidence |
|---------|---------------|--------------|--------------------|-------------------------------|-----------------|------------|
| runner-pi | native (`--mcp-config`) / shim | PASS | pi RPC same session → `tool_execution_end` | yes (no stall) | no | High / Med-rel |
| runner-codex | native (MCP cfg) / shim | PASS | app-server same thread → `item/*` | **yes (verified)** | no | High |
| runner-opencode | native (`mcp` cfg) / shim | PASS | SSE same session → tool parts | yes (path exists) | no | High / Med-rel |
| chat-pi | native / shim | inherited | `ChatEvent::ToolOutput` | yes | no | Med-High |
| chat-codex | native / shim | inherited | `ChatEvent::ToolOutput` | yes | no | Med-High |
| chat-opencode | native / shim | inherited | `ChatEvent::ToolOutput` | yes | no | Med-High |

No surface is `none`. No surface needs an upstream protocol change to reach
native — only Agentropy-side wiring.

## Recommendation

**Adopt a native-first architecture built on a single Agentropy MCP bridge, with
a runner-agnostic shim defined as the reliability floor.** Concretely:

1. **Agentropy host MCP bridge (one component).** A small stdio MCP server,
   spawned/owned by the host, that exposes the extension tool registry and
   executes tools in-host with extension config/secrets. This is the single
   place that satisfies C3 + C6 (host execution + observability) and is reused
   by every backend.
2. **Wire each surface to the bridge natively:**
   - codex: `-c mcp_servers.agentropy.command=<bridge>`
   - pi: `--mcp-config <file pointing at bridge>`
   - opencode: add `mcp.agentropy` to the `opencode.json` the runner already writes
   - chat-{pi,codex,opencode}: same config on the chat spawn path
3. **Define the shim as the contract floor, not the default.** Specify the strict
   prompt convention + tool-call marker + continuation-prompt loop (all runners
   already support `TurnDecision::Continue { prompt }` / `send_turn`). Use it for
   any backend/version where native MCP is unavailable or unreliable, and as the
   conformance harness for the C1–C7 contract.

### Why not native-only admission
Native works on every current surface, but admission should not be gated on it:
backend MCP support varies by version, codex flags stdio MCP auth as
`Unsupported`, and one pi run showed an MCP init race. A pure native-only policy
would make the contract hostage to per-version backend quirks. Keeping the shim
as a defined floor preserves parity if a future backend/version regresses.

### Why not file-substrate-first
A file-substrate fallback (agent reads/writes files; host watches and acts) is
the most robust degraded mode and pairs well with the shim, but it is the worst
ergonomics and weakest correlation (C4) of the three. It should be the
documented last resort, not the primary architecture, given native works today.

### Parity vs reliability tradeoff (explicit)
- **Native** maximizes reliability and ergonomics (real tool-call protocol,
  correlation ids, structured errors) and — surprisingly — already gives full
  parity across all six surfaces. Its risk is per-backend/version drift.
- **Shim** maximizes worst-case parity (works wherever there's a prompt + a
  continuation turn, which is every surface) but trades reliability: marker
  parsing is brittle, correlation is by convention, and malformed agent output
  can mis-fire. It belongs as the floor/conformance baseline, not the hot path.

Net: native-first gets reliability *and* current parity; the shim floor insures
parity against future backend regressions. This is strictly better than
native-only (fragile to version drift) and than shim-floor-by-default (needlessly
sacrifices reliability that all backends already offer).

## Scope notes for ALG-253

- The PRD can assume **native MCP on all current surfaces**, not codex-only.
- The first implementation slice is the **host MCP bridge + per-surface config
  wiring**, not protocol research — capability is proven.
- Keep the **shim spec + a C1–C7 conformance test** as a sibling slice so the
  contract has a portable floor and a backend-agnostic test harness.
- Out of scope here and confirmed unchanged: full registry, `linear_graphql`
  production tool, scheduler tools, full permission model, remote execution.

## Risks / follow-ups
- **pi MCP init race:** one toy run did not surface the tool (likely server not
  ready at turn start). Characterize startup ordering before relying on pi native
  in production; the shim floor covers this.
- **opencode failure path** not separately stall-tested; verify `isError`
  surfacing + session survival.
- **codex stdio MCP auth = `Unsupported`:** benign for a local host bridge
  (no auth needed) but confirm before any remote/authed MCP.
- **Secret hygiene:** the bridge executes with host secrets; ensure
  `runner-core::scrub_loaded_env` semantics extend to the bridge process and that
  redaction (C6) covers tool args/results.
