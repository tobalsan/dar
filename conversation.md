---
title: "Self-contained agent runtimes inspired by Symphony"
date: "2025-01-XX"
participants:
  - User
  - Assistant
source: Claude Code
tags:
  - aihub
  - symphony
  - agent-runtime
  - rust
  - wasm
  - extensions
---

# Self-contained agent runtimes inspired by Symphony

**User**:
Throwing a wild idea. This project is a centralized gateway that can define agents and provide them with tools and channels to be interacted with.

Since implementing PR #15, inspired by https://github.com/openai/symphony repo and https://github.com/openai/symphony/blob/main/SPEC.md sepcs (study both), I have a wild idea I want to expose to you.

**Assistant**:
Studied the Symphony SPEC (v1 draft) and the current state of aihub including PR #15.

**Where aihub is right now (post-PR #15):**
- Centralized gateway exposing agents via web UI, Discord, Slack, CLI, scheduled jobs (channels)
- Two-tier agents: lead agents (configured in `aihub.json`) and CLI subagents (Claude Code / Codex / Pi)
- Extension model: `packages/extensions/{board,discord,heartbeat,langfuse,multi-user,orchestrator,projects,scheduler,slack,subagents,webhooks}`
- **Orchestrator** (PR #15) now Symphony-aligned: per-project `WORKFLOW.md` owns tracker/workspace/hooks/agent/prompt; AIHub config only lists project folders + global concurrency; Linear-scoped polling by `project_slug`; directory-only workspaces; gateway-owned worker lifetimes (tracker+filesystem-driven recovery, no live-session reattach)
- Intentional divergence from strict Symphony: AIHub uses its `subagents` runtime (Claude/Codex/Pi profile adapter) instead of a Codex app-server client; profiles map to `agent.profile` in workflow

**Symphony's core shape (SPEC.md):**
- Single project = `WORKFLOW.md` (front matter: `tracker`/`polling`/`workspace`/`hooks`/`agent`/`codex` + Markdown prompt body)
- Poll → reconcile → dispatch loop; in-memory authoritative state; tracker+FS restart recovery
- Per-issue sanitized workspaces under `workspace.root`; cwd containment invariant
- Continuation turns on the same thread up to `max_turns`; short 1s continuation retry, 10s·2^n backoff on failure
- Tracker is a reader; writes flow through the agent (optionally via `linear_graphql` client-side tool)
- Optional HTTP `/api/v1/state`, `/api/v1/<id>`, `/api/v1/refresh`; optional SSH remote-worker extension

Ready — what's the wild idea?

---

**User**:
What if I embraced fully the Symphony's core philosophy and turned AIHub--at least what can be--into a fully self-contained runtime for each agent.
I'm thinking:

Agent runtime = folder + binary (e.g. symphony). Single, tiny self contained runtime that runs from inside the agent folder.

Each agent decides which surface they expose themselves too. Sort of a Subscription model.
Any centralized surface (like AIHub web UI) would be just just a space to host agents that decided to register on. It's up to the agent to decide if they want to live in that space, e.g. aihub does not define the agent list, only exist as available surface.

I'm just wondering if there exist a language / framework (e.g. rust, go) that would let us turn the current set of AIHub features into fully self-contained runtimes that have a footprint small enough to run tens of agents on a single consumer machine (e.g. 16gb RAM, mid-spec device). By set of features, I mean that any agent runtime can embed:
- channels (Slack, Discord, etc.)
- schedulers (cron/heartbeat)
- symphony orchestration loop, i.e. any agent has its own task queue
- a minimal web UI to see the agent config, the scheduled jobs, the orchestration loop queue (like what exist in AIHub `/orchestrator` part of the app), and a minimal chat interface to interact directly with the agent.

Note that the self-contained runtime would **not** include the LLM harness entirely, just a runner (like current aihub orchestrator, or similary to Symphony's exec of codex server) for the configured LLM runtime in `agent.yaml`, e.g. claude / codex / Pi / CLI or Codex Server / Opencode Server / Pi RPC server on the host machine.

**Assistant**:
This is a coherent direction and Symphony's SPEC actually points right at it: workflow-owned config, in-memory authoritative state, tracker+FS recovery, no required DB, optional HTTP surface. A single small binary per agent is the natural shape of that spec.

## The model you're describing

- **Agent = folder** containing `agent.yaml`, `WORKFLOW.md`, workspaces/, prompts, secrets ref, state files.
- **Runtime = one binary** invoked inside that folder. Owns: orchestration loop, scheduler, embedded HTTP/WS for chat + dashboard, channel clients the agent opted into, an exec adapter to the LLM CLI/server (claude / codex app-server / opencode / Pi RPC).
- **Hubs (AIHub web, a Discord server, a Slack workspace) are surfaces, not registries.** Agents register outward into the surfaces they want, the surface aggregates whoever showed up. Inverts current AIHub which owns the agent list.

That maps cleanly onto Symphony's "WORKFLOW.md is the contract, the runtime is small, in-memory, restart via tracker+FS." You'd just generalize from "Linear-orchestrated worker" to "long-lived agent process with N event sources."

## Footprint reality

Per-agent budget on a 16GB box, target 10–30 concurrent:

| Stack | Cold RSS / agent | Binary | Notes |
|---|---|---|---|
| Node/TS (current) | 80–200MB | n/a | 10 agents ≈ 1–2GB before LLM work |
| Go | 15–40MB | ~15–25MB | Easy, great stdlib, slightly chubbier RSS |
| **Rust (tokio/axum)** | **5–20MB** | **~10–25MB** | Best fit if footprint is the constraint |
| Zig | 3–15MB | small | Smallest, ecosystem thin for Discord/Slack |
| Elixir/BEAM | shared VM | n/a | Doesn't fit "one binary per agent" but does fit "100s of agents on one VM" — alternative architecture |

The LLM subprocess dominates regardless, so runtime overhead being 10MB vs 150MB is the difference between 10 agents and 100 agents per machine.

## Concrete Rust stack that covers your feature list

- **Async core**: `tokio`
- **HTTP/WS + dashboard + chat**: `axum` + `tower` + `tokio-tungstenite`
- **Embed UI in binary**: `rust-embed` for assets; HTMX + server-rendered `maud`/`askama` keeps the bundle tiny (~100KB JS total). A React UI works too but eats ~300KB+.
- **Scheduler**: `tokio-cron-scheduler` or a 200-line cron loop
- **State**: `rusqlite` (bundled, no system dep) or just JSON/markdown files like AIHub does today
- **Discord**: `serenity` or `twilight` (twilight is leaner, à-la-carte crates)
- **Slack**: `slack-morphism` (Events API + Socket Mode)
- **Linear/tracker**: `reqwest` + `serde` — Symphony's GraphQL client is ~300 LOC
- **Process supervision** (Codex/Claude/Pi): `tokio::process` with line-buffered stdio
- **Config**: `serde_yaml` for `agent.yaml` + `WORKFLOW.md` front matter
- **Hot reload**: `notify` for FS watch (Symphony §6.2)
- **Single static binary**: `cargo build --release` → 10–25MB, cross-compile macOS/Linux

Go equivalent if you want faster authoring: `chi` + `nhooyr/websocket` + `discordgo` + `slack-go/slack` + `robfig/cron` + `modernc.org/sqlite` (pure Go, no CGO). Binary ~20MB, RSS ~30MB.

## The "subscription" inversion — what actually changes

Today: AIHub config enumerates agents; gateway routes channel events to them.

Inverted: each agent process **dials out** to surfaces:

- Discord: agent owns its own bot token, opens its own gateway WS.
- Slack: agent is its own Slack app (Socket Mode WS) or registers an Events URL.
- Linear: agent polls its own `project_slug`.
- AIHub web (or any "hub"): agent makes outbound WS to `https://hub/register`, hub multiplexes per-agent panels. Hub never stores agent definitions — just live sessions.
- LAN discovery: mDNS for free local visibility.

Two consequences worth surfacing:

1. **Reachability**: chat from the hub → agent works fine over the outbound WS the agent already opened (reverse tunnel pattern). No NAT pain.
2. **Per-agent credentials**: 10 agents = 10 Discord bots / 10 Slack apps. Cheap, but ops overhead. Symphony-style "agent owns its tokens via `$VAR` indirection" stays the model.

## Where Symphony's spec already gives you load-bearing pieces

- §5 workflow contract → directly your `agent.yaml`/`WORKFLOW.md`
- §6.2 dynamic reload, §6.3 preflight validation → already what each agent runtime needs
- §7 in-memory orchestration state, §14.3 restart recovery via tracker+FS → no shared DB needed across agents
- §13.7 optional HTTP `/api/v1/*` → your minimal dashboard
- §9.5 workspace safety invariants → reuse verbatim
- Appendix A SSH worker extension → already anticipates multi-host fan-out

You're essentially extending Symphony with: persistent channel adapters, cron, and a per-agent chat UI. None of those break its invariants.

## Things that get harder than they look

- **Cross-agent collaboration / handoff**: today AIHub gateway can route a message between lead agents. With self-contained processes you need a message bus (NATS, Redis, or just hub-mediated WS). Decide early whether agents talk to each other or only to humans/trackers.
- **Updates**: 30 agents = 30 binaries. Want a self-update channel (e.g. `agent upgrade` pulling signed releases) or a thin shared launcher that pins versions per agent folder.
- **Observability**: logs need to go somewhere. Per-agent log files + optional push to a hub works; don't bake centralized logging in.
- **Web UI duplication**: if every agent ships React, you bloat. HTMX + server-rendered keeps each agent's UI ~200KB on disk and means the hub UI can iframe/proxy them uniformly.
- **Channel quota / WS connection counts**: Discord limits one gateway per bot token (fine), Slack Socket Mode 1 per app (fine). No real cap until you hit the platform itself.

## My read

If you want this, **Rust + axum + HTMX + twilight/slack-morphism** is the right combo. Go is the pragmatic fallback if Rust authoring slows you down — you lose maybe 2× RSS, gain faster iteration. Stay away from anything that ships a Chromium (Tauri etc.) for the per-agent binary; that kills the whole footprint argument.

The bet is sound only if the **multiplication factor matters to you** (10+ agents per host, edge devices, friends running their own). If you mostly run 2–3 agents on one workstation, the current TS gateway is already fine and the rewrite cost dominates.

Want me to:
1. Sketch a concrete `agent.yaml` + `WORKFLOW.md` schema that covers channels + scheduler + orchestrator?
2. Mock the minimal Rust crate layout + dependency list?
3. Spec the "hub as surface" registration protocol (outbound WS, message envelope, panel mounting)?

---

**User**:
Note: for Slack/Discord channel, and any "extension-like" feature, I actually want to keep it out of the agent core. I see rather a similar extension-based model, where the agent natively bakes in an extnesion mechanism, and we can develop and connect custom extensions if our choosing to add features.

**Assistant**:
Good call — that keeps the core auditable and lets the runtime survive policy changes (Slack v2 API, Linear renaming, etc.) without churning the binary.

## What stays in core vs. what's an extension

| In core (always linked) | Extension (opt-in, separate process or .wasm) |
|---|---|
| Symphony orchestration loop + in-memory state | Channels: Slack, Discord, Telegram, IRC, email |
| Workspace manager + path safety invariants | Trackers beyond Linear (GitHub Issues, Jira, Plane) |
| Scheduler (cron + heartbeat primitive) | Observability sinks (Langfuse, OTLP, Posthog) |
| `agent.yaml` + `WORKFLOW.md` loader + hot reload | Tools exposed to the LLM (MCP-style) |
| Exec adapter for the LLM CLI/server (claude/codex/pi/opencode) | UI panels added to the dashboard |
| Embedded HTTP/WS dashboard + chat | Hub registration (so the agent dials AIHub web) |
| Extension manager (discovery, lifecycle, dispatch, sandbox) | Auth bridges (OAuth dance services) |
| Secret resolution (`$VAR`, file refs) | Custom event sources / webhooks beyond the core listener |

The core is essentially: Symphony + scheduler + dashboard + extension host. Everything user-facing flows through extensions.

## Extension transport — pick one model, support two

Three viable transports; recommend supporting **two** to cover the realistic feature surface:

| Transport | Per-ext RSS | Languages | Good for | Bad for |
|---|---|---|---|---|
| **Subprocess + stdio JSON-RPC** (MCP-style) | 5–150MB depending on lang | Any | Long-lived WS connections (Slack/Discord), heavy SDKs, OS access | Adds processes; per-ext startup cost |
| **WASM (Extism / wasmtime)** | shared ~5–10MB runtime + <1MB/plugin | Rust, Go, JS (Javy), Python (limited) | Pure functions, tool calls, formatters, validators | Long-lived sockets, native crypto, big SDKs |
| Dynamic library (`dlopen`) | tiny | Rust/C/C++ only | n/a really | Rust ABI is unstable, fragile across versions; skip |

**Recommendation**: subprocess for channels/trackers/sinks/auth (anything network-heavy or SDK-bound), WASM for tools and lightweight transforms. Both share one manifest format and one capability vocabulary; the agent just picks the runtime based on what the manifest declares.

Already standard in the ecosystem: **MCP is exactly subprocess + stdio JSON-RPC for tool extensions**. Build on it instead of inventing a new protocol — your Slack extension is "MCP + a couple of extra capability verbs."

## Extension manifest (proposed)

```yaml
# ~/.aihub/extensions/slack/extension.yaml
id: slack
version: 1.2.0
runtime: subprocess     # or "wasm"
entrypoint: ./slack     # binary path, or .wasm module
capabilities:
  - channel.inbound     # pushes messages into the agent
  - channel.outbound    # agent can post messages
  - ui.panel            # provides a dashboard panel at /ext/slack
config_schema: ./schema.json
permissions:
  network: [api.slack.com, wss://wss-primary.slack.com]
  fs: []
  env: [SLACK_BOT_TOKEN, SLACK_APP_TOKEN]
```

Agent opts in:

```yaml
# agent.yaml
extensions:
  slack:
    path: ~/.aihub/extensions/slack
    config:
      bot_token: $SLACK_BOT_TOKEN
      channels: [C0123, C0456]
      route: chat  # which inbound surface this maps to
```

Disabled extensions never spawn — zero footprint.

## Capability vocabulary (small, stable)

Keep the verb set tight or you re-invent a framework. Suggested seed:

- `channel.inbound` / `channel.outbound`
- `tracker.poll` / `tracker.write` (so you can plug GitHub Issues as a tracker beside Linear)
- `tool.call` (MCP-compatible)
- `sink.event` (observability/logging)
- `auth.bridge` (OAuth callback handler)
- `ui.panel` (extension registers an HTML/HTMX fragment at `/ext/<id>`; core proxies)
- `lifecycle.hook` (matches Symphony `after_create`/`before_run`/`after_run`/`before_remove`)

Anything else is built on these. New capability = explicit core change.

## Wire protocol shape

JSON-RPC 2.0 over stdio (one frame per line, `Content-Length` headers optional). Identical methods regardless of subprocess vs WASM — the host just routes them differently.

Bootstrap:
```
agent → ext: initialize { agent_id, agent_dir, config, capabilities_requested }
ext   → agent: initialized { capabilities_provided, schema_version }
```

Inbound from extension (e.g. Slack message arrived):
```
ext → agent: notify "channel.message" { route, user, text, attachments, ts }
```

Outbound to extension (agent wants to post):
```
agent → ext: call "channel.send" { route, text, blocks? }
ext   → agent: result { ok, message_id }
```

Shutdown: `agent → ext: shutdown`, then SIGTERM after grace window.

## Footprint with this design

Concrete 16GB-box scenario, 1 agent with Slack + Discord + Linear-tracker + Langfuse-sink + 2 MCP tools:

| Component | RSS |
|---|---|
| Core agent (Rust) | 15MB |
| Slack ext (Rust, subprocess) | 12MB |
| Discord ext (Rust, subprocess) | 12MB |
| Langfuse sink ext (Rust, subprocess) | 8MB |
| 2 MCP tool exts (WASM) | shared 8MB + ~1MB plugins |
| **Total per agent** | **~57MB** before LLM subprocess |

10 such agents ≈ 600MB. With shared LLM clients (one codex app-server reused across agents via the exec adapter pool? — open question) you stay well under 8GB before LLM working set.

If extensions are written in **Node/Python**, multiply each by 4–10×. Ship official extensions in Rust or Go; let third parties pick whatever, accepting the footprint hit on themselves.

## What this changes vs. current AIHub

- `packages/extensions/*` becomes the source for first-party **separate-binary** extensions, not in-process modules.
- Extension API is a small JSON-RPC vocabulary, not a TS interface — works for any language.
- Each extension can be independently versioned and updated; agent declares minimum version.
- Sandboxing is real: subprocess gets restricted env, WASM gets WASI capabilities only.
- Hub (AIHub web) becomes one possible `ui.panel` consumer + outbound WS target, not a registry.

## Open decisions worth pinning before code

1. **MCP-compatible or MCP-superset?** Strict MCP for tools means your tool extensions work with Claude Desktop / Cursor / etc. unchanged. Worth it.
2. **Hot-reload of extensions** (Symphony §6.2 covers config; do extensions inherit that)? Recommend: yes for config changes, no for binary swaps — restart the ext process on file mtime change.
3. **Per-extension resource limits** (cgroups on Linux, `setrlimit`, WASM fuel)? Useful when running untrusted extensions; skippable for v1 if all extensions are first-party.
4. **One Slack/Discord extension per agent, or shared across agents?** Shared means one process serves N agents (lower RSS, higher complexity, breaks the "agent owns its tokens" model). Per-agent is cleaner and matches the inverted ownership story.
5. **WASM included from day one or added later?** Skipping WASM ships faster; adding it later is straightforward since the manifest already declares `runtime:`.

Want me to draft (a) the JSON-RPC method catalogue for the seed capability set, or (b) a minimal Rust crate layout showing the core + extension host + one example subprocess extension?

---

**User**:
self-containment is a tenet. So I don't even want an external extension use, more like pulling extension directly in the agent folder. Think Chrome plugins, or VSCode extensions.

**Assistant**:
Right — that's a cleaner mental model and changes the answer.

## Self-contained extensions = bundled into the agent folder

```
my-agent/
├── agent.yaml
├── WORKFLOW.md
├── workspaces/
├── state/
├── secrets.env
└── extensions/
    ├── slack/
    │   ├── extension.yaml
    │   ├── slack            # native binary OR
    │   └── slack.wasm       # wasm module
    ├── discord/
    ├── linear-tracker/
    └── tools/
        └── github-mcp.wasm
```

Move the agent folder, you move everything. No system-wide registry, no shared install dirs, no "did you `npm i -g` the right version" failures. `tar czf my-agent.tgz my-agent/` is the whole deploy.

## What this constraint actually forces

**The core runtime can no longer assume any extension exists**, but it must still be able to **load and run whatever's in `./extensions/`** without network or system package manager access. Two things follow:

1. **Extension format must be portable and verifiable from inside the folder.** Native binaries break this on cross-platform — a folder built on macOS won't run on Linux. WASM solves it; native is only viable if you publish multi-arch and the agent folder either ships all arches or is platform-locked.
2. **No external process the user has to install.** Everything the extension needs is in its own subfolder, or it's a static binary.

This pushes hard toward **WASM as the default** extension format, with native binaries as a documented escape hatch for things WASM can't do (today: long-lived TLS WebSockets are awkward, some crypto, raw sockets).

## Realistic format mix

| Extension type | Format | Why |
|---|---|---|
| Tools (MCP-style: format, validate, transform, call API once) | **WASM** | Truly portable; tiny; sandboxed for free |
| Trackers (poll HTTPS, parse JSON) | **WASM** with WASI HTTP | Portable; fits within wasmtime's HTTP support |
| Observability sinks (POST events) | **WASM** | Same |
| UI panels (HTML/HTMX fragments + small handler) | **WASM** | Portable, no FS needed |
| Channels with persistent WS (Slack Socket Mode, Discord Gateway) | **Native static binary** per platform, *or* WASM with `wasi-sockets` once stable | WASM sockets aren't there yet in stable wasmtime |
| Lifecycle hooks (shell scripts) | Already supported via Symphony `hooks.*` | n/a |

If you want **strict** "drop folder, it runs anywhere," go WASM-only for v1 and accept that channels lag behind until `wasi-sockets` lands (preview2 has it; wasmtime 27+ ships experimental support).

If you want channels day one, accept platform-tagged native extensions:

```
extensions/slack/
├── extension.yaml
├── slack-darwin-arm64
├── slack-darwin-x64
├── slack-linux-x64
└── slack-linux-arm64
```

Core picks the right binary at load time; `tar` of the folder works across platforms. Cost: ~4× the extension's binary size on disk (still tiny; Rust static binary for Slack ≈ 12MB × 4 = 48MB worst case).

## Loading model — Chrome/VSCode parallel

The Chrome/VSCode mental model is right but worth being precise about what to copy and what not to:

| Copy | Skip |
|---|---|
| Manifest in extension folder declares capabilities + permissions | Marketplace, signing infrastructure (yet) |
| Permissions surfaced to the user, granted per-extension | Auto-update from a registry — your agents are self-contained |
| Extensions can register UI fragments | A v8 isolate per extension (you don't need full JS) |
| Capability scopes (`permissions: [channel.inbound, network: api.slack.com]`) | Inter-extension messaging (keep extensions independent for v1) |
| Activation events (lazy load on first need) | n/a |

**Activation events** matter for footprint. VSCode doesn't boot every extension; it loads them on `onCommand`, `onLanguage`, etc. For your agent:

```yaml
# extensions/slack/extension.yaml
id: slack
activation:
  - on: startup           # always running (needed for channels)
# vs
activation:
  - on: scheduler.tick    # cron extension only runs on tick
  - on: tool.call:github  # MCP tool only on demand
```

Tool extensions you call once per turn don't need to be resident. Channel extensions do. This is the difference between 10MB resident per extension and 0MB until invoked.

## Discovery — Symphony-style, no registry

On startup, the agent simply:

1. Scans `./extensions/*/extension.yaml`.
2. Validates manifest schema + signature (optional, you sign your first-party set).
3. For each `activation.on: startup`, spin up the WASM instance (or spawn the native binary).
4. For lazy extensions, register their triggers and wait.
5. Watch `./extensions/` with `notify` for add/remove → reload.

No central registry. No "install" command needed — `cp -r path/to/ext my-agent/extensions/` works. An optional `agent ext add <url>` is a convenience that downloads into the local folder; it's not architectural.

## Bundling agent + extensions into one artifact (optional but neat)

Three packaging stories worth supporting:

1. **Loose folder** — the default; everything visible, editable, git-friendly.
2. **`.agent` archive** — `zip`/`tar` of the folder; the runtime can run from an archive via `agent run my-agent.agent`. Useful for distribution.
3. **Single static binary** — concat the runtime + the agent folder via `include_dir!` (Rust) or `embed.FS` (Go). Hands someone a single file that *is* the agent. Useful for "give your friend this `.exe`."

(3) is the strongest expression of self-containment. The runtime stays unchanged; the binary is just `runtime + embedded VFS overlay`. WASM extensions inside the embedded FS work transparently; native extensions can't be embedded portably, which reinforces "WASM default, native escape hatch."

## What the manifest looks like under this constraint

```yaml
# extensions/slack/extension.yaml
id: slack
version: 1.2.0
runtime: wasm            # or "native"
entrypoint: ./slack.wasm # relative to this folder, always

activation:
  - on: startup

capabilities:
  provides:
    - channel.inbound
    - channel.outbound
    - ui.panel:/slack
  requires_host:
    - secrets.read
    - http.fetch
    - websocket          # gated; native fallback if WASM runtime lacks it

permissions:
  network:
    - api.slack.com
    - wss://wss-primary.slack.com
  secrets:
    - SLACK_BOT_TOKEN
    - SLACK_APP_TOKEN

config_schema: ./schema.json
ui:
  panel: ./panel.html    # served by core at /ext/slack
```

Everything the extension needs to declare, plus everything the core needs to enforce, lives in that one file. No external state.

## Footprint with bundled WASM-default extensions

| Component | RSS |
|---|---|
| Core agent (Rust + wasmtime) | 25MB |
| Slack WASM (resident, has WS) | 6MB |
| Discord WASM (resident, has WS) | 6MB |
| Linear tracker WASM (resident, polls) | 4MB |
| 2 MCP tool WASMs (lazy) | 0MB until called, ~2MB peak |
| **Per agent** | **~40–45MB** |

10 agents ≈ 450MB before LLM. wasmtime shares its runtime code across instances within a process but not across processes; if you wanted cross-agent sharing you'd need a daemon — abandoning the self-containment tenet. Don't.

## The thing this design buys you

- **`git clone` the agent folder = working agent.** No install step. This is the killer property.
- **Forks are trivial.** Copy folder, edit, run.
- **No version skew between core and extensions.** The agent folder is internally consistent by construction.
- **Audit is local.** `ls extensions/` tells you exactly what this agent can do.

## The thing it costs you

- **Disk duplication.** 10 agents each shipping Slack = 10 copies of the Slack WASM. At ~6MB each, negligible. At 60MB each (native multi-arch), still fine. Stop worrying about this.
- **No central update channel.** Updating Slack everywhere = walk all agent folders. Acceptable; provide `agent ext update --all` that scans `~/agents/*/extensions/slack/` if you want.
- **First-party extensions need a build pipeline that produces WASM.** Rust → WASM is solved; Go → WASM is acceptable; TS → WASM via Javy works but is heavier. Pick Rust for first-party.

## Recommendation

- **Core**: Rust + tokio + axum + `wasmtime` (with `wasi-preview2`) + HTMX dashboard.
- **Extensions**: WASM-first, native escape hatch for now (channels). Migrate channels to WASM once `wasi-sockets` is solid.
- **Manifest**: as sketched above, schema-validated at load.
- **Layout**: folder is canonical; archive and single-binary packagings are derivatives of the folder.
- **Activation**: explicit, declared in manifest; lazy by default for tools.

This is the tightest possible expression of your tenet: the agent folder is the boundary, and crossing it is an explicit choice (network call, hub registration, channel subscription) made by code that lives inside the folder.

Want me to draft (a) the manifest JSON schema, (b) the host↔WASM ABI for the seed capabilities, or (c) the single-binary packaging recipe?

---

**User**:
oh, I forgot an important point: true to Symphony's philosophy, we shouldn't be specific on using Linear as an issue tracker. We should have the orchestrator loop understand and use fundamental verbs linked to issue tracking, but we should be able to configure the agent to use Linear, Github, or any Project manager that follows the known practices (projects, issues, PRs, assignees, statuses, priority, etc.).

**Assistant**:
Yes — and Symphony's spec already half-builds that exit ramp. §11 isolates `tracker.kind`, normalizes everything through a domain model (`Issue` in §4.1.1), and says explicitly "A non-Linear implementation MAY change transport details, but the normalized outputs MUST match the domain model." Linear is just the first concrete adapter. You finish the job by making the tracker a **capability**, not a hardcoded core module.

## What the core actually needs to know

Strip it down to the minimum vocabulary the orchestration loop touches. Everything else is adapter-specific and stays out of core.

**Domain model (frozen, lowest common denominator):**

```
Issue {
  id            string   # opaque tracker-internal ID
  identifier    string   # human-readable key, e.g. "ABC-123" or "#42"
  title         string
  description   string | null
  state         string   # raw tracker state name
  priority      int | null
  assignees     [string] # normalized handles
  labels        [string] # lowercase
  blocked_by    [BlockerRef]
  url           string | null
  created_at    timestamp | null
  updated_at    timestamp | null
  pr            { url, state, merged } | null   # optional, when known
}
```

**Tracker capability verbs (the only thing core calls):**

| Verb | Purpose | Required? |
|---|---|---|
| `tracker.poll_candidates` | Return issues in active states for the configured scope | Required |
| `tracker.fetch_states` | Refresh state for a set of IDs (reconciliation) | Required |
| `tracker.fetch_terminal` | List terminal-state issues (startup cleanup) | Required |
| `tracker.fetch_one` | Look up by id/identifier | Recommended |
| `tracker.write_comment` | Optional write surface | Optional |
| `tracker.transition` | Optional state transition | Optional |
| `tracker.link_pr` | Optional PR association | Optional |

That's it. Seven verbs covers Linear, GitHub Issues, Jira, Plane, Forgejo, Linear-clones, and "issues.json in a folder."

**Symphony's boundary stays intact**: writes are optional. The core never depends on them. Agents that want to update tickets call them through the same tool-extension surface. If a tracker adapter doesn't implement `write_comment`, the agent simply doesn't have that tool available — graceful degradation, no core knowledge of which trackers can do what.

## Tracker becomes an extension by the rules you already set

```
my-agent/
├── agent.yaml
├── WORKFLOW.md
└── extensions/
    └── tracker-github/
        ├── extension.yaml
        ├── tracker-github.wasm
        └── schema.json
```

```yaml
# extensions/tracker-github/extension.yaml
id: tracker-github
runtime: wasm
activation: [{ on: startup }]
capabilities:
  provides:
    - tracker.poll_candidates
    - tracker.fetch_states
    - tracker.fetch_terminal
    - tracker.fetch_one
    - tracker.write_comment
    - tracker.transition
permissions:
  network: [api.github.com]
  secrets: [GITHUB_TOKEN]
```

```yaml
# agent.yaml
tracker:
  use: tracker-github
  config:
    repo: tobalsan/aihub
    project: "AIHub Roadmap"
    active_states: [Todo, "In Progress"]
    terminal_states: [Done, Closed, "Won't Do"]
    active_filter:
      assignees: [me]
      labels: ["agent-ready"]
```

The agent declares **which tracker capability provider it uses** and **its scope config**. The orchestration loop calls verbs; whichever extension provides those verbs answers. Same as how the channel system works.

## What stays workflow-level vs. tracker-level

Symphony's `WORKFLOW.md` `tracker.*` block currently mixes Linear-specific keys (`endpoint`, `project_slug`) with universal ones (`active_states`, `terminal_states`). Split it:

**`agent.yaml` (universal — core understands):**
```yaml
tracker:
  use: <extension-id>
  active_states: [...]      # raw state names this tracker recognizes as active
  terminal_states: [...]    # raw state names that mean done/cancelled/etc.
  config:                   # opaque to core, schema-validated by the extension
    <whatever the adapter needs>
```

**Adapter-specific `config:` block (opaque to core):**

| Tracker | Typical config block |
|---|---|
| Linear | `endpoint`, `api_key`, `project_slug`, `team_key?` |
| GitHub Issues | `repo`, `project_number?`, `labels_filter?`, `assignees_filter?` |
| Jira | `base_url`, `email`, `api_token`, `project_key`, `jql_filter?` |
| Plane | `host`, `workspace_slug`, `project_id`, `api_key` |
| Forgejo/Gitea | `base_url`, `repo`, `token` |
| Local files | `path: ./issues/` (markdown + frontmatter, dead simple, great for offline/testing) |

The core never grows a switch statement on tracker type. New tracker = new extension, zero core changes.

## State normalization without losing tracker semantics

You hit one real problem: different trackers have different state vocabularies and the agent prompt often wants to *reason* about state. Two layers solve it:

1. **Raw state passed to the prompt**: workflow templates already get `{{ issue.state }}`. Whatever the tracker calls it ("In Review", "needs-qa", "Code Review"), the agent sees it as-is. Workflows are written for their tracker; that's fine.

2. **Active/terminal classification handled in core**: only orchestration cares about "is this dispatchable / should we stop the worker." That's already in `active_states`/`terminal_states`. No semantic state machine in core.

Don't introduce a "canonical state enum" (`open`/`in_progress`/`review`/`done`). Every attempt at that across trackers fails — Jira's status graph is not Linear's is not GitHub's. Symphony's choice of "raw strings + active/terminal partition" is the right minimum and you should keep it.

## PR awareness — keep it as an optional field

Symphony doesn't model PRs at all (§11.5 explicitly says tracker writes including PR links are agent-tool concerns). You probably want a small optional surface, because PR linkage often *gates* state ("don't mark Done until PR is merged"):

- Optional `Issue.pr` field — populated only if the adapter knows how.
- Optional verb `tracker.link_pr(issue_id, pr_url)` — adapter answers if it implements.
- Workflow can reference `{{ issue.pr.merged }}` if present.

This works for GitHub (issue ↔ PR is native), Linear (uses `attachments`/branchName), Jira (Smart Commits / dev panel). For trackers without PR concepts, the field stays `null` and templates handle the absence.

## Scope filtering — one verb, many filter shapes

The hardest cross-tracker design question is "which issues are *mine to work on*." Linear uses `project_slug`. GitHub uses repo + project board + label + assignee. Jira uses JQL. You can't unify the filter language without becoming a query DSL.

Solution: **scope config is opaque to core**. Core just calls `tracker.poll_candidates()`. The extension knows its own scope from the config block. The core's only contribution is `active_states` (so the adapter knows what to filter by) and pagination handling.

This matches Symphony §11.2 ("Linear-specific requirements") being a per-adapter section, not core spec.

## Tracker-side write writes vs. agent-tool writes

Symphony has a clean boundary here you should preserve: **the orchestrator never writes**. Writes happen through tools the agent calls during a turn.

So `tracker.write_comment` / `tracker.transition` are **not called by the orchestration loop**. They're exposed as **MCP tools to the agent** when the adapter provides them. The agent decides "I'm done, let me comment and transition." Core just routes the tool call to the same extension.

One adapter, two surfaces:
- Orchestrator-facing verbs (`poll`/`fetch`/`terminal`) — required.
- Agent-tool verbs (`comment`/`transition`/`link_pr`) — optional, exposed as MCP tools when present.

This means a read-only tracker (e.g. a CSV-file adapter for prototyping) still works perfectly; the agent just doesn't have write tools.

## What this changes in your `WORKFLOW.md` story

Symphony's §5.3.1 `tracker` block becomes adapter-agnostic:

```yaml
---
tracker:
  use: tracker-linear        # or tracker-github, tracker-jira, etc.
  active_states: [Todo, "In Progress"]
  terminal_states: [Done, Cancelled, Duplicate]
  config:                    # opaque, validated by extension's schema.json
    project_slug: aihub
    api_key: $LINEAR_API_KEY

polling:
  interval_ms: 30000

workspace:
  root: ./workspaces

agent:
  profile: worker
  max_turns: 20
---

You are working on an issue from the {{ tracker.kind }} tracker...
```

Notice `{{ tracker.kind }}` in the prompt — that's a templating affordance, not a core enum. The extension declares its kind string for prompt use. Workflows targeting "any tracker" can write generic prompts; tracker-specific workflows just hardcode it.

## Two adapters to validate the design

Build these two first; if both fit cleanly, the abstraction holds:

1. **`tracker-linear`** — port Symphony's GraphQL client as-is, exposed through the verb surface. Familiar territory.
2. **`tracker-github`** — REST + GraphQL hybrid; "issues in repo X with label Y assigned to Z" is the scope. Tests `assignees`, label-based filtering, PR linkage natively.

If you can write both adapters without touching core, the design works. If either forces a core change, the verb set is wrong.

Then add **`tracker-files`** as a third adapter — issues stored as `./issues/*.md` with YAML frontmatter. No network, no auth, runs offline, perfect for testing the orchestrator without external dependencies. Falls out almost for free and is genuinely useful for local-only agent workflows.

## Summary

| Aspect | Decision |
|---|---|
| Core knows | `Issue` domain model + 3 required + 4 optional verbs |
| Core doesn't know | Tracker kinds, query languages, auth schemes, state vocabularies |
| Tracker = | A capability provider extension, same machinery as channels |
| State semantics | Raw strings to prompt; only `active_states`/`terminal_states` are normalized |
| Writes | Always agent-tool surface, never orchestrator-direct |
| Scope filtering | Opaque to core, extension-config concern |
| Validation | Two real adapters (Linear + GitHub) + one offline adapter (files) |

This is what "true to Symphony's philosophy" looks like at the limit: the orchestration loop is a pure scheduler over a tiny issue verb set, and every concrete tracker is a swappable folder-local extension.

Want me to draft the verb-set JSON-RPC catalogue, or the `tracker-files` reference adapter schema (since that's the smallest existence proof)?
