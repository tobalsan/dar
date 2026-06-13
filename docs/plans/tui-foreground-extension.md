# Implementation plan — `tui` foreground extension (chat / logs / dashboard)

Per the adversarial critique's verdict: **Design B's three-crate contract-first architecture**, hybridized with Design A's lightness choices, with both designs' shared protocol error fixed — chat drives **`pi --mode rpc`** (long-lived JSONL child), not per-turn one-shot shim spawns.

---

## 1. Decision summary

- **Three new crates, zero host changes:** `crates/cap-chat` (contract), `extensions/chat-pi` (pi backend), `extensions/tui` (ratatui foreground). Dist gains 2 dep lines + 2 `plugins![]` lines.
- **Chat seam = new `ChatBackend`/`ChatSession` traits in `cap-chat`**, registered in the typed service registry under the same ids as runners (`dyn ChatBackend @ "pi"` coexists with `dyn Runner @ "pi"`, keys are `(id, TypeId)`). This is the only structure that allows claude/codex chat later without surgery on `cap-runner`.
- **Pi transport = one long-lived `pi --mode rpc --session-dir <data/tui/sessions>` child per launch** (fresh session each launch per spec), cwd = agent root. Turns via `{"type":"prompt"}`, cancel via `{"type":"abort"}` (graceful, session survives), quit via stdin close. Fixes protocol correctness, per-message spawn latency, lossy cancel, and gives real streaming deltas — verified live by the pi-protocol research.
- **Context pre-load (the spec's core win):** first turn prepends a bounded preamble built from the retained `RunSnapshot` (queue/active/history summary) + a capped listing of `<root>/issues/` if present; chat cwd = agent root so the agent's own tools can read everything else.
- **Backend resolution (lazy, at first submit):** `extensions.tui.chat.backend` config → `RunSnapshot.agent.runner` if that id has a registered `ChatBackend` (skip silently when topic absent OR `version == 0` OR runner empty) → fallback `"pi"` with a transcript notice when an incompatible runner was configured → nothing registered = disabled chat pane with banner. Never a boot failure.
- **TUI structure from A:** plain `enum Tab` + match (no Tab trait / Action machinery), single-line input (no textarea dep), OS-thread input reader (no `event-stream`/futures dep), dirty-flag + 100 ms coalesced redraw (from B). `TermWriter(ExclusiveTerminal)` adapter into `CrosstermBackend`.
- **Versions pinned to the workspace lock:** `crossterm = "0.28"` (lock has 0.28.1), `ratatui = "0.29"` (its crossterm backend matches 0.28) — one copy in tree. No ansi-to-tui, no markdown, no syntect.
- **Tab 3 hidden (not placeholder) when orchestrator topics are unregistered** — matches the locked spec wording ("present/active only if orchestrator enabled"); detection = `subscribe_retained::<RunSnapshot>` Err at startup. Recorded as the deliberate reading.
- **Failure handling from A:** fixed 10-min per-turn timeout (TUI-side timer → `abort()` + error block); `!is_interactive()` degrades to byte-for-byte frontend-log behavior (piped/CI runs keep working); `Lagged(n)` surfaced as a synthetic log row.
- **Repo rules respected:** tui registers no topics (subscribe/publish only), never writes issue state (Stop/Pause/Resume are `ControlMsg` publishes, rendered from snapshot — single-writer preserved), no work in register/start, host owns raw-mode/alt-screen/panic-hook lifecycle (extension adds none). `docs/extensions.md` amended to read "at most one *capability* crate; shared payload crates (`orchestrator-api`) don't count" (dashboard precedent).

---

## 2. Contracts

### 2.1 `crates/cap-chat` (new contract crate; host-api-free, own `BoxFuture` alias like cap-runner)

```rust
pub const CHAT_FALLBACK_BACKEND: &str = "pi";

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole { Assistant, Thinking }

#[derive(Clone, Debug)]
pub enum ChatEvent {
    /// Streamed text; UI appends to current block of this role (new block on role change).
    Delta { role: ChatRole, text: String },
    /// Completed tool call (name + rendered args).
    ToolCall { id: String, name: String, args: String },
    /// Tool output keyed by id. `text` REPLACES prior output for the same id
    /// (pi streams accumulated partialResult).
    ToolOutput { id: String, text: String, is_error: bool, done: bool },
    /// Backend-side error line (stderr, protocol error).
    Error(String),
    /// Exactly one per turn. aborted turns: ok=false, error=Some("aborted").
    TurnFinished { ok: bool, error: Option<String> },
    /// Backend process died outside a clean close; session is unusable.
    SessionClosed { error: Option<String> },
}

#[non_exhaustive]
pub struct ChatSessionParams {
    pub command: String,            // "" -> backend default binary
    pub agent_root: std::path::PathBuf,   // child cwd
    pub session_dir: std::path::PathBuf,  // persistence home, caller-owned & pre-created
    pub model: Option<String>,
}
impl ChatSessionParams {
    pub fn builder(command: &str, agent_root: &Path, session_dir: &Path) -> Builder;
    // Builder: .model(Option<String>) .build() -> ChatSessionParams
}

pub trait ChatBackend: Send + Sync {
    fn open<'a>(&'a self, params: ChatSessionParams,
                tx: tokio::sync::mpsc::Sender<ChatEvent>)
        -> BoxFuture<'a, anyhow::Result<Box<dyn ChatSession>>>;
}

pub trait ChatSession: Send {
    /// One turn at a time (caller enforces). Returns once the turn is ACCEPTED;
    /// completion arrives as ChatEvent::TurnFinished on tx.
    fn send_turn(&mut self, prompt: String) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Graceful cancel of the in-flight turn. Session stays usable.
    fn abort(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Close stdin, wait briefly, term-then-kill the process group on overrun.
    fn close(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>>;
}
```

Deliberately trimmed vs Design B: no `provider`/`thinking` knobs, no six-role enum, no `TurnHandle` (long-lived child makes `abort()` the cancel primitive). `#[non_exhaustive]` + builder kept (cheap; claude backend will add fields) and pinned by a `cap-chat/tests/builder.rs` contract test, same as `cap-runner`.

### 2.2 Registry ids

| Service | Id | Registered by |
|---|---|---|
| `dyn ChatBackend` | `"pi"` | `extensions/chat-pi` `register()` |
| (later, out of v1) | `"claude"`/`"claude-code"`, `"codex"` | future chat backends |
| `Foreground` | `"tui"` | `extensions/tui` `register()` |

### 2.3 Bus topics — **tui registers none; consume-only**

| Topic | Type | Use |
|---|---|---|
| `host.log-events` (`host_api::LOG_EVENTS_TOPIC`) | broadcast `LogEvent` | Logs tab; degrade path |
| `host.startup-banner` (`host_api::STARTUP_BANNER_TOPIC`) | retained `Option<LogEvent>` | one-shot startup banner, print once (see 2026-06-11 amendment) |
| `host.app-done` (`host_api::APP_DONE_TOPIC`) | retained `bool` | loop exit |
| `orchestrator.run-snapshot` (`orchestrator_api::RUN_SNAPSHOT_TOPIC`) | retained `RunSnapshot` | Dash tab render; chat runner-follow; context preamble |
| `orchestrator.control` (`orchestrator_api::CONTROL_TOPIC`) | broadcast `ControlMsg` | publish `Stop`/`Pause`/`Resume` only |

`frontend-log` and `orchestrator` stay linked in dist as topic owners; `foreground: tui` is pure selection.

### 2.4 agent.yaml schema

```yaml
foreground: tui            # opt-in; default remains "logs"

extensions:
  tui:
    chat:
      backend: pi          # optional; default: follow runner.use, then pi
      command: pi          # optional binary override
      model: gpt-5         # optional, forwarded to backend
```

```rust
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig { pub chat: ChatConfig }

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    pub backend: Option<String>,
    pub command: Option<String>,
    pub model:   Option<String>,
}
```

Parsed in `register()` from `ctx.config.get("tui")`; malformed → register error → clean boot failure + existing HITL startup hook.

### 2.5 Key map (resolves Design B's focus ambiguity)

Chat input is always focused on the Chat tab; there is no focus toggle.
- `Ctrl+C` — quit, everywhere (raw mode delivers it as a key).
- `q` — quit, **Logs/Dash tabs only** (types a "q" on Chat).
- `Tab` / `Shift+Tab` — cycle tabs (global; chat input never needs Tab).
- Chat: `Enter` submit (rejected while in flight), `Esc` abort in-flight turn, `PgUp/PgDn` scroll transcript (End re-follows).
- Logs: `Up/Down/PgUp/PgDn/End` scroll, End re-engages follow.
- Dash: `p` Pause, `r` Resume, `s` Stop.

---

## 3. Phased milestones

Each milestone merges green on `cargo build --release && cargo test --release` and leaves `foreground: logs` (the default) byte-identical in behavior.

### M1 — `cap-chat` contract + `chat-pi` backend (headless, fully tested)

**Scope:** new `crates/cap-chat` (types above + `tests/builder.rs`); new `extensions/chat-pi` (`PiChatBackend` registering `dyn ChatBackend @ "pi"`; `PiChatSession` = long-lived `pi --mode rpc --session-dir <dir> [--model m]` child, cwd `agent_root`, process group + `scrub_loaded_env` + `strip_ansi`/`term_then_kill` reused from `runner-core`; own JSONL line pump mapping rpc events → `ChatEvent` per the table in §4; auto-handles `extension_ui_request`); root `Cargo.toml` workspace members.
**Tests in-crate:** rpc command JSON shapes (`prompt`/`abort` lines); event-mapping table (canned `message_update`/`tool_execution_*`/`agent_end`/response/stderr lines → ChatEvent variants, ToolOutput replace semantics); register test (`get_named::<dyn ChatBackend>("pi")` resolves); one spawn-level test driving a stub script as `command` (printf canned rpc JSONL; runner-fake pattern) through open → send_turn → events → TurnFinished, plus abort and close-kills-group.
**VERIFY:** `cargo test --release -p cap-chat -p chat-pi`. Manual smoke (pi 0.79.1 installed): tiny `examples/smoke.rs` or test gated `#[ignore]` — open against real `pi`, send "say ping", observe `Delta{Assistant,"ping"}` + `TurnFinished{ok:true}`; second turn in same session recalls the first (session-continuity check, closes Design risk).

### M2 — `tui` extension boots, degrade path, chat pane works end-to-end (chat lands first)

**Scope:** new `extensions/tui` (`src/lib.rs` TuiExtension + TuiConfig; `src/foreground.rs` event loop + `TermWriter`; `src/app.rs` App with `enum Tab` — **Chat only in the tab list for this milestone**, no tab bar rendered with one tab; `src/chat.rs` transcript state `Vec<ChatBlock>` + input editor + in-flight gate + 10-min turn timer; `src/input.rs` OS-thread crossterm reader → mpsc; `src/view.rs` chat render: user/assistant/thinking(dim)/tool(boxed)/error(red) blocks, streaming append, spinner while in flight). Non-interactive path: exact frontend-log line loop. Dist: 2 dep lines, 2 `plugins![]` lines (`chat_pi::ChatPiExtension, tui::TuiExtension`). Session opened lazily on first submit: `paths.data_dir("tui")?` + `create_dir_all`, params from config, backend = config override or `"pi"` (follow logic is M3).
**Tests:** register test (`select(Some("tui"))` resolves, bad config errors, **no topics registered by tui**); non-interactive run with `ExclusiveTerminal::non_interactive(Vec<u8>)` — publish `LogEvent`, flip `APP_DONE_TOPIC` → returns Ok, output **byte-for-byte** equals frontend-log's `"{level} {target} {message}\n"`; `TestBackend` render tests (blocks, in-flight gate, Esc→abort path); chat-turn integration via stub-script backend.
**VERIFY:** edit `example-agent/agent.yaml` → `foreground: tui`; `./target/release/agentropy run --dir ./example-agent`; type a message → streamed assistant reply with visible thinking/tool blocks; `Esc` mid-turn aborts gracefully and chat stays usable; `Ctrl+C` exits with terminal fully restored (no raw-mode residue); `agentropy run --dir ./example-agent </dev/null | head` still streams plain log lines. Revert agent.yaml.

### M3 — context pre-load + `runner.use`-follow + fallback notice

**Scope:** `extensions/tui/src/chat.rs` + `src/backend.rs`. Preamble builder: `bus.read_retained::<RunSnapshot>` (best-effort) → agent id/folder, queue/active/recent-history summary (capped ~30 lines) + bounded `<root>/issues/` listing (≤20 entries, name + first `state:`/title line) + one orientation paragraph ("operator chat inside agent folder X; issues at ./issues; you run trusted"); prepended to the first prompt only. Resolution order from §1, with the `version == 0` / empty-runner guard; incompatible runner → transcript notice block `runner "claude-code" has no interactive chat backend; chatting via pi`; nothing registered → disabled input + banner.
**Tests:** resolution unit tests against hand-built registry + bus (config wins; follow when registered; `version==0` → silent pi; incompatible → pi + notice; empty registry → disabled); preamble builder caps respected; preamble only on turn 1.
**VERIFY:** `cargo test --release -p tui`; live: in example-agent ask "what issues are pending?" as the *first* message → answer reflects `issues/*.md` without manual context; set `runner.use: claude-code` → chat tab shows the fallback notice, still chats via pi.

### M4 — Logs tab

**Scope:** `extensions/tui/src/logs.rs` + view + Tab list grows to `[Chat, Logs]` with a rendered tab bar. `VecDeque<LogEvent>` cap 2000, follow-tail default, `Lagged(n)` → dim synthetic row `"… {n} log lines skipped"`, `Closed` → stop pumping keep buffer; subscribe Err → "log topic unavailable (frontend-log not linked)" placeholder.
**Tests:** ring buffer + lagged-row + follow/scroll logic; `TestBackend` render; line format identical to frontend-log's.
**VERIFY:** run example-agent with orchestrator dispatching (or just doctor-level activity): Logs tab shows the same lines `foreground: logs` printed in the prior run — diff a captured non-interactive run against the visible buffer content; nothing lost.

### M5 — Dashboard tab + controls (present only when orchestrator linked)

**Scope:** `extensions/tui/src/dash.rs` + view. At startup, `subscribe_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)`: Ok → Dash appears in tab list; Err → tab absent entirely (locked-spec reading, documented in crate doc). Render: header (agent id/tracker/runner, **paused badge**, last-tick age, rate-limit), tables for `active_runs`, `queue`, `retry`, `history`, recent `events` tail (richer than the web dashboard — all from one snapshot). `version == 0` → "waiting for first tick…". Keys `p/r/s` → `bus.publish(CONTROL_TOPIC, ControlMsg::{Pause,Resume,Stop})`, fire-and-forget, exactly the web dashboard's usage; no local state mutation.
**Tests:** `TestBackend` renders for empty/populated/`version==0` snapshots; key→publish mapping (assert on a subscribed receiver); tab-absent when topic unregistered.
**VERIFY:** run example-agent, switch to Dash, press `p` → next snapshot (≤ poll interval) shows the paused badge and queue stops draining; `r` resumes; `s` stops runs (issue files untouched — confirm `state:` unchanged). Build a dist variant with orchestrator commented out of `plugins![]` → boots, Dash tab not shown, Chat/Logs fine.

### M6 — docs + rule amendment

**Scope:** `docs/extensions.md` — amend the dependency rule: "at most one *capability* crate; shared bus-payload crates (`orchestrator-api`) don't count" + note `frontend-log` owns `host.log-events`/`host.app-done` and `tui` consumes them; README + example-agent comments for `foreground: tui` and `extensions.tui.chat`; CLAUDE.md architecture blurb (tui/chat-pi/cap-chat, `q` quits the whole agent).
**VERIFY:** `cargo build --release && cargo test --release` workspace-green; `./target/release/agentropy doctor --dir ./example-agent` unchanged.

---

## 4. Protocol appendix

### 4.1 pi interactive protocol (`pi --mode rpc`) — what chat-pi implements (v1)

Spawn: `pi --mode rpc --session-dir <data/tui/sessions> [--model <m>]`, cwd = agent root, own process group, `.env`-scrubbed env. Strict JSONL both ways: split stdout on `\n` only (strip `\r`); commands may carry an `id`, the matching `{"type":"response","id":...,"success":...}` echoes it; **events have no id**. Tolerate interleaved non-response lines (verified live: fire-and-forget `extension_ui_request` lines appear immediately).

stdin commands used:
```json
{"id":"t1","type":"prompt","message":"Fix the failing test"}
{"type":"abort"}
```
(`steer`/`follow_up`/`set_model` etc. exist — not used v1.) Quit = close stdin (clean shutdown), then `term_then_kill` the group if it lingers.

stdout → `ChatEvent` mapping:

| pi event | ChatEvent |
|---|---|
| `message_update` w/ `assistantMessageEvent.type == "text_delta"` (`delta`) | `Delta{Assistant, delta}` |
| … `"thinking_delta"` | `Delta{Thinking, delta}` |
| … `"toolcall_end"` (`toolCall: {id,name,arguments}`) | `ToolCall{id,name,args}` |
| `tool_execution_update` (`toolCallId`, `partialResult`) | `ToolOutput{id, text, done:false}` — **replace**, partialResult is accumulated |
| `tool_execution_end` (`result.content[].text`, `isError`) | `ToolOutput{id, text, is_error, done:true}` |
| `agent_end` | `TurnFinished{ok:true}` |
| `assistantMessageEvent` `error` reason `"aborted"` | `TurnFinished{ok:false, error:Some("aborted")}` |
| … reason `"error"` / `response` with `success:false` | `TurnFinished{ok:false, error}` |
| `extension_ui_request` dialog (`select`/`confirm`/`input`/`editor`, **blocks**) | auto-respond `{"type":"extension_ui_response","id":...,"value":...}` (confirm→true, select→first option, input/editor→"") + `Error("auto-answered dialog: ...")` notice |
| `extension_ui_request` fire-and-forget (`notify`/`setStatus`/`setWidget`/`setTitle`), `queue_update`, `compaction_*`, `auto_retry_*` | ignore |
| stderr line / unparseable line | `Error(line)` (cap rate) |
| process exit unexpectedly | `SessionClosed{error}` |

Example stream:
```json
{"type":"message_update","message":{...},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"Let me look..."}}
{"type":"message_update","message":{...},"assistantMessageEvent":{"type":"toolcall_end","contentIndex":1,"toolCall":{"id":"call_123","name":"bash","arguments":{"command":"ls"}}}}
{"type":"tool_execution_end","toolCallId":"call_123","toolName":"bash","result":{"content":[{"type":"text","text":"total 48..."}]},"isError":false}
```

Note: the `jsonrpc/turn` shape used by `extensions/runner-pi` is **not** stock pi's protocol — do not copy it into chat-pi.

### 4.2 claude stream-json essentials (for the later `chat-claude` backend — seam compatibility only)

```bash
claude -p --input-format stream-json --output-format stream-json --verbose \
  --include-partial-messages --permission-mode bypassPermissions --add-dir <agent_root> [--model m]
```
Long-lived NDJSON session: stdin stays open across turns; closing stdin ends it.
- Turn: `{"type":"user","message":{"role":"user","content":"..."}}` per line; each turn ends with a `{"type":"result",...}` line (`result` ≠ exit).
- Interrupt: `{"type":"control_request","request_id":"r1","request":{"subtype":"interrupt"}}` → `control_response` + a `result` for the aborted turn; session survives → maps onto `ChatSession::abort()`.
- Streaming: `{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}}` (`text_delta`/`thinking_delta`/`input_json_delta`) → `Delta`; tool calls in `assistant` lines' `tool_use` blocks; results in `user`-type lines with `tool_result` + `tool_use_result` sidecar (distinguish replays by `isReplay:true`).
- `system/init` re-emits per turn (constant `session_id` — capture for `--resume`); ignore unknown types/subtypes.
- **Never** `--dangerously-skip-permissions` headless (TTY acceptance hang); `--verbose` is mandatory with stream-json output; consider `--bare` for determinism.
The `ChatBackend::open / send_turn / abort / close` contract maps 1:1 onto this — no contract change anticipated.

### 4.3 Host terminal facts implementers rely on

Host already enabled raw mode + alternate screen and installed a restoring panic hook before `Foreground::run`; `ExclusiveTerminal::restore()`/`Drop` undoes it. The extension must **not** call `enable_raw_mode`/`EnterAlternateScreen` or install hooks. Render via `CrosstermBackend::new(TermWriter(terminal))`; input via `crossterm::event::poll/read` on a dedicated OS thread (no handle needed; nothing else consumes stdin). Foreground return = app shutdown.

---

## 5. Risks & explicit non-goals

### Risks
1. **pi rpc surface drift / dialog semantics** — `extension_ui_request` auto-answers are a guess (confirm→true etc.); validate against real pi tool flows in the M1 `#[ignore]` live smoke. Unknown event types must be ignored, never crash the pump.
2. **"Fresh session per launch" semantics in `--session-dir`** — assumed pi starts a new session in the dir per process (transcripts kept for post-mortem). If it auto-continues, add `new_session` on open or fall back to `--no-session`. Checked in M1 smoke.
3. **Logs tab content depends on the orchestrator's tracing bridge** (sole `host.log-events` publisher) — orchestrator-absent build = quiet logs tab; placeholder text says so.
4. **2-worker tokio runtime** (`dist/src/main.rs`) — coalesced 100 ms redraw + OS-thread input keeps the loop light; watch for stutter under log bursts + huge transcripts (mitigation: render window already bounded by scroll).
5. **`q`/Ctrl-C kills the whole agent** (foreground-return semantics) — bigger footgun than the web dashboard ever had; documented in M6, no code change.
6. **Fixed 10-min turn timeout fires during legitimately long tool runs** — error block tells the operator to resend; constant, not config, until proven wrong.
7. **Two api-crate imports in `extensions/tui`** (`cap-chat` + `orchestrator-api`) — covered by the M6 docs amendment; dashboard precedent.

### Explicit non-goals (v1 cuts)
- claude/codex chat backends (contract supports them; not built).
- Markdown/syntax rendering, `ratatui-textarea` multiline input, themes, mouse, configurable keybindings, suspend (Ctrl-Z).
- Reply-channel controls (`Tick/Claim/Release/Interrupt/Kill`) on the Dash tab — Stop/Pause/Resume only, matching the web dashboard.
- Chat transcript persistence into `data/store.db` / structured chat logging (pi's own session JSONL in `data/tui/sessions` is the post-mortem artifact).
- In-TUI permission prompts (trusted session per spec), `steer`/`follow_up` mid-turn commands, multi-session / `/new`, detach-TUI-keep-agent-running.
- Any change to `host-api`, `cap-runner`, `runner-core`, `agentropy-host`, or existing extensions' behavior; `foreground: logs` remains the untouched default.

---

## Amendment (2026-06-11)

Startup banner restored in M0 (the `agentropy running; dashboard on http://{bind}:{port}/` line was removed accidentally in the dist refactor, 72cac39). Publishing it on `host.log-events` from `start()` does NOT work: that topic is a plain broadcast (no replay), and the host runs every extension's `start()` before the foreground's `run()` subscribes, so the event would be dropped. The restored mechanism is:

- **Retained topic `host.startup-banner`** (`host_api::STARTUP_BANNER_TOPIC`, `Option<LogEvent>`, initial `None`), registered by `frontend-log` next to the other log topics. Retained = replayed to a foreground that subscribes later, so delivery does not depend on boot ordering.
- **The orchestrator publishes the banner after its first tick completes** (`Orchestrator::with_startup_banner` → `emit_startup_banner` in `extensions/orchestrator/src/lib.rs`), not at spawn time, so it only announces a loop that is actually running. The host now binds the HTTP listener synchronously in `boot_inner` before any extension starts (`crates/agentropy-host/src/lib.rs`), restoring fail-fast on occupied ports — the banner can no longer announce a dashboard URL that failed to bind.
- The `logs` foreground prints the retained banner exactly once (at subscription if already set, else on change), in the same `{level} {target} {message}` line format as `host.log-events` rows.

**TUI impact:** the banner does NOT arrive on `host.log-events`, so the M4 Logs tab must additionally `subscribe_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC)` and prepend/print it once, mirroring `frontend-log` (topic row added to §2.3).