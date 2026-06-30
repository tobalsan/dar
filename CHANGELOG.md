# Changelog

All notable changes to Dar are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0:
the minor slot carries breaking changes, the patch slot carries compatible ones).

Breaking changes are marked **⚠ BREAKING**.

## [Unreleased]

### Changed
- Runners: spawn agent children with non-interactive git env (`GIT_EDITOR=true`,
  `GIT_SEQUENCE_EDITOR=true`, `GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`/`PAGER=cat`)
  so editor/pager/credential prompts auto-resolve instead of hanging the run until
  the stall guard parks it (ALG-291).
- Worker prompt: added Git Usage guidance steering workers away from
  interactive/TTY-blocking git commands (ALG-291).

## [0.3.1] — 2026-06-29

Patch release: additive SDK/chat surface plus TUI session recall. No breaking
public-API changes.

### Added
- TUI: daily-driver Chat tab — multi-line editor, mouse scroll, status line (#71).
- TUI: session recall — resume newest session across restart (ALG-302), `/new`
  starts a fresh session (ALG-303), `session_list` (ALG-304), and
  `session_search` + `session_read` (ALG-305) tools.
- TUI: hydrate the Chat tab from the resumed session on launch (ALG-313).
- TUI: configurable chat turn timeout, default 60m (#84).
- SDK: shared agent-chat session params across all chat surfaces (ALG-315).
- system-context: extract agent identity into a substrate extension (ALG-312).
- init/build: auto-generate the agent `.gitignore` (ALG-306).

### Fixed
- SDK: preserve explicit chat-backend errors instead of masking them.
- composer: unify the SDK source in local builds; preserve agent identity.
- TUI: clear hydrated transcript on `/new`; bound `read_recent` hydration to a
  streaming ring buffer (ALG-313); drop coding framing from the passive chat
  preamble (ALG-311); fix chat system-prompt identity (#78).
- chat-pi: resume sessions via `--session`, not interactive `--resume` (ALG-309).
- tool-registry: stop redacting UUIDs as secrets.
- Fix pi subagent liveness tailing (#72).

### Documentation
- Refresh `AGENTS.md` for the v0.3.0 architecture.

## [0.3.0] — 2026-06-25

Project renamed **agentropy → dar**, the public extension SDK lands, and the
ecosystem grows: a scheduler, a host tool registry + MCP bridge, system-context
identity, a dashboard-tab contract, and portable agent builds.

### Added
- **SDK**: new `dar-extension-sdk` crate — the single dependency third-party
  extensions name; compose resolves stock crates by path.
- **Tooling**: host extension tool registry + MCP bridge wired to the codex, pi,
  opencode, and TUI-chat runners (ALG-260, ALG-261, ALG-263); a runner-agnostic
  shim transport with a C1–C7 conformance harness; tool-call observability with
  read/write metadata (ALG-263); validated JSON-backed mutation tools (ALG-262).
- **Scheduler extension**: cron jobs fire a runner and write output markdown
  (ALG-219); HTTP job CRUD with atomic persistence (ALG-222); hot-reload of
  `cron/jobs.json` (ALG-223); run-now and tail endpoints (ALG-224); read-only
  Cron dashboard tab (ALG-225); overlap-skip, timeout, and kill-switch guards
  (ALG-221); management tools (#69).
- **System context**: `system-files` resolver + config + retained topic + boot
  publish (ALG-270); orchestrator prepends system context to runner prompts
  (ALG-271); TUI chat consumes it (ALG-272); workspace `skills/` exposed to all
  runners; passive (tracker-less) agents supported.
- **Dashboard**: extension dashboard-tab contract (ALG-220); fleet aggregator
  via the presence registry (ALG-247); paginated run history (ALG-248);
  per-run dispatched runner + model shown (ALG-265).
- **Linear**: OAuth app tokens via `LINEAR_OAUTH_TOKEN` (ALG-268); delegate
  tracker targeting (#70); combinable issue filters (#36).
- **Builds**: portable / self-contained agent builds; agents compose their own
  binaries; subset stock extensions by config; bundle all runners per-agent
  build; hot-reload live agent config.

### Changed
- **⚠ BREAKING**: rename `agentropy` → `dar` across crates and registry deps.
- Remove the Claude Code runner (ALG-246).
- Guard self-rebuild swaps and roll back crash-looping self-updates.
- Mark internal crates non-publishable so only the public SDK closure ships.
- Refactors: split `runner-core` and hoist builders; share `cap-runner` default
  consts; decompose scheduler internals; dedup dashboard handlers.

### Fixed
- cli: load `.env` on the run path.
- TUI: wrap long log lines; show newest runs first in dash history.
- orchestrator: show the passive-agent dashboard URL.
- dashboard: log the real bind address; keep the paginated page across
  self-poll; order fleet agents by first-seen.

### Documentation
- Add the `dar-extension-sdk` plan; document self-contained agent builds; split
  the README into two build models.

## [0.2.0] — 2026-06-13

The runtime becomes extension-based: the shipped binary is assembled from an
explicit `dist` plugin list, capabilities move into contract crates, and a TUI
foreground with native chat backends arrives.

### Added
- **Architecture**: per-extension config and foreground selection via
  `agent.yaml`; host API two-phase boot; typed service-registry dispatch
  (ALG-211); foreground log frontend (ALG-214); extract runner/tracker contract
  crates; extract orchestrator + dashboard into extensions; move trackers into
  extensions.
- **TUI**: chat / logs / dash terminal foreground; chat reuses the runner model
  and provider.
- **Runners & chat**: opencode runner; live pi RPC turn loop; tracker-driven
  turn loop; runner turn contract (ALG-234); native codex chat backend (#27);
  native OpenCode chat backend (ALG-231); mid-turn steering (#26); canonical
  thinking/effort reasoning-level config (ALG-232, ALG-233).
- **Dashboard**: restore the run-detail drawer; render codex logs as a
  transcript; extension example + scaffold generator (#23).

### Changed
- **⚠ BREAKING**: assemble the binary from the `dist` plugin list.
- host: surface startup errors via a hook.

### Fixed
- host-api: merge root HTTP mounts instead of nesting.
- Numerous dashboard and runner log/UX fixes (drawer stickiness, history
  streaming, codex/opencode log structuring).

### Documentation
- Extension authoring guide; TUI foreground + chat config; align docs with the
  `dist` extension layout.

## [0.1.0] — 2026-06-11

First tagged release: the folder-scoped agent runtime and core orchestration
loop.

### Added
- Core orchestration loop (#6) with retry and stall semantics (#10).
- SQLite persistence for runs, events, claims, and heartbeats (#1).
- Runner abstraction — pi / claude / codex / cli / fake — with structured event
  capture (ALG-174).
- `LinearTracker`: GraphQL polling, project-slug scoping, rate limits (ALG-175).
- `WORKFLOW.md` frontmatter as orchestrator-loop config (ALG-172) and the
  default workflow prompt scaffold (ALG-176).
- Orchestrator HTTP API + websocket (#9); Linear webhook tick endpoint (#12);
  pluggable HITL notifier (#11); workspace lifecycle hooks (#7); Linear safety
  parking (#8).
- Dashboard run-detail UI (#14); workflow init Linear export tools (#13).

### Changed
- **⚠ BREAKING**: unify child env vars under `AGENT_*`.

### Fixed
- Align dispatch backoff exponent with the PRD; wait for stale workers to die at
  startup; record runs for failed dispatch attempts; honor Linear complexity
  rate limits.

[0.3.1]: https://github.com/tobalsan/dar/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/tobalsan/dar/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tobalsan/dar/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tobalsan/dar/releases/tag/v0.1.0
