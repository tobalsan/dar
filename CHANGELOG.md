# Changelog

All notable changes to Dar are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0:
the minor slot carries breaking changes, the patch slot carries compatible ones).

Breaking changes are marked **⚠ BREAKING**.

## [Unreleased]

- Scheduler jobs can now deliver completed results to registered communication sinks using explicit `deliver` targets; delivery failures are recorded as warnings without changing the job result.

### Added

- Scheduler jobs can run contained scripts directly, including script-only and wake-gated agent jobs, so deterministic scheduled work records process exit failures mechanically.

### Fixed

- `WORKFLOW.md` `polling:` now accepts `poll_interval_ms` as an alias for `interval_ms` (matching agent.yaml's `orchestrator.poll_interval_ms`), and unrecognized `polling:` keys are logged as warnings instead of being silently dropped — a misnamed interval key previously fell back to the 1 s default and could exhaust tracker API rate limits.
- An older overlapping dashboard process can no longer remove a newer same-agent/workflow fleet presence entry during shutdown.
- Extension `data_dir` lookup now works for fresh agent folders by preparing the contained shared `data/` parent, so SDK callers no longer need to create it first.
- Scheduler jobs now consistently reject `timeoutMs: 0` from `cron/jobs.json`, HTTP mutations, and agent tools instead of accepting runs that time out immediately.
- Boot and `dar doctor` now reject zero-valued workflow polling, concurrency, timeout, and turn limits instead of accepting configurations that spin, disable dispatch, or terminate runs immediately.
- Chat transcript write failures now return or stream a recoverable error and settle active turns instead of panicking request or backend-forwarding tasks.
- Concurrent scheduler job changes from HTTP and agent tools no longer overwrite one another; memory and `cron/jobs.json` now retain every accepted mutation.
- Scheduler runs now report an error when their required output artifact cannot be written, keeping run-now responses and Cron status consistent with persisted output.
- Web chat now reports rejected sends and restores the message and pending attachments instead of silently discarding the draft or leaving the composer in a stale busy state.
- Rejected web and TUI chat turns no longer create phantom transcript messages or leave turns marked active; accepted turns still publish the user message before backend output.
- Orchestrator tracker read errors at turn boundaries and after normal worker exits now preserve work for a later retry instead of finishing it as non-active.
- Workspace skill discovery now ignores skill directories and files whose symlinks resolve outside the agent folder, preventing external files from entering system context.
- Abnormally exited orchestrator attempts are now closed in persisted run history before a retry is queued, so dashboards and restart cleanup no longer treat exited workers as live.
- Web chat dashboard no longer froze the page for seconds on load when a session had a long transcript; repaint is now coalesced per animation frame instead of per replayed event.
- Self-rebuild now deploys the binary via temp file + rename instead of overwriting `bin/dar`/`bin/dar.new` in place; in-place overwrite of an already-executed binary poisons macOS's per-inode code-signature cache, making every later launch (e.g. the MCP bridge) die with SIGKILL.

### Changed

- Linear HTTP responses now emit structured process, operation, status, attempt, and rate-limit telemetry without logging request contents or credentials.
- `dar-artifacts` is now published to crates.io alongside the other public
  crates — it became part of the SDK surface (`dar-cap-chat` and
  `dar-extension-sdk` depend on it), so registry-only extension builds resolve.

## [0.4.0] — 2026-07-19

### Fixed

- `dar self rebuild` now bootstraps through the freshly built binary: after
  building, it re-invokes the new binary's own `dar compose` (a new,
  lockfile-untouched subcommand) to detect composition drift, resolving the
  dar source checkout itself and passing it down via `DAR_SRC` so this works
  for agent folders outside the checkout. Only when composition actually
  changed does it refresh the lockfile and rebuild once more. One pass now
  picks up newly selected stock extensions (e.g. `chat-web`), self rebuild no
  longer fails with a stale `--locked` lockfile after a newer composer ran
  against the still-running old binary, and unaffected rebuilds no longer pay
  for an unconditional `cargo update` (and its network round trip) on every
  run. Live-rebuild restart confirmation now waits up to 300s (was 60s), since
  a composition-changed live rebuild can now run two full release builds.
- `dar dash`: agents are now reverse-proxied at `/agent/<port>/...` instead of
  being iframed over plain HTTP, enabling HTTPS fronts. HTML attributes, htmx
  verbs, and heuristic inline-JS URLs are rewritten while SSE/chat streams
  through unchanged. Proxied agents share the aggregator origin, so cookies
  and credentials are shared across agents. All dashboards on the host must be
  trusted: this prefix is a presentation convenience, not an isolation or
  security boundary.
- `dar dash` proxy now sends `X-Forwarded-Prefix` header and skips HTML rewriting for prefix-aware first-party pages (dashboard, cron tab, chat), enabling them to work behind the fleet proxy with SSE reconnect and attachments; regex rewriting retained as a compatibility fallback for third-party pages.
- `dar dash` proxy now forwards all request headers to agents except `Authorization` and `Accept-Encoding`, fixing SSE `Last-Event-ID` reconnect and HTTP `Range` requests through the proxy.
- `dar dash` agent liveness is now cached (~1s) instead of re-scanning the registry per request.
- `dar dash` proxy errors now log the target agent port and cause.
- Web chat send no longer crashes on non-HTTPS, non-localhost origins (e.g. tailnet hostnames) where `crypto.randomUUID` is unavailable.
- Web chat now follows `runner.use` and its model/provider for passive agents without an orchestration snapshot, instead of silently falling back to Pi.

### Added

- `dar self rebuild` with no arguments now rebuilds the live agent whose
  dashboard presence folder is the current directory, matching `dar build`'s
  default.
- Web chat accepts image and document attachments through multipart uploads, renders them inline, and supplies their local paths to the agent turn.
- Web chat now has a responsive phone layout with touch-friendly controls and contained tool, thinking, and code blocks.
- Web chat can compact the active conversation and shows backend-reported context usage.
- Chat tabs and the TUI now join one live agent-wide chat session, with shared
  transcript storage and browser stream fan-out.
- Shared chat now resumes the newest backend session, expires idle sessions
  lazily, and migrates legacy TUI sessions into an empty shared store.
- TUI Logs tab and `foreground: logs` output now prefix each line with the local date and time.
- Web chat transcripts persist per session and reconnecting clients replay missed events.
- Opt-in web chat dashboard tab with streaming replies and turn abort.
- Web chat now renders streamed thinking, tool calls and updating tool output,
  markdown, and errors in the transcript.
- Web chat honors `extensions.chat-web.enabled: false` as a runtime kill switch
  (the extension still links and loads but mounts nothing), matching the
  scheduler extension.
- Dashboard tabs can now declare themselves self-refreshing (the shell's 2s
  poll leaves them alone while active) and mark themselves as the default tab
  for passive agents; the web chat tab uses both, so agents without an
  orchestration loop open on Chat.
- Web chat UI redesign: full-height conversation layout with role labels,
  collapsible thinking and tool drawers, attachment chips, Enter-to-send
  composer with Shift+Enter newline, and inline code rendering.
- Web chat slash commands: `/compact` compacts the session context; `/new`
  starts a fresh session, clearing the transcript with a "Context cleared,
  started a new session." notice. `/compact` is likewise supported in the TUI
  chat, passed through verbatim to the backend CLI.
- Web chat busy indicator now appears in the transcript as a pending-reply
  placeholder under your message, with a randomly drawn whimsical status word
  (Pondering, Conjuring, Brewing, ...) and pulsing dots per turn.

- Added: live `dar self rebuild <agent>` triggers a trusted dashboard-host
  rebuild and confirms the restarted boot identity.
- Fixed: worker `self_update` relays target the live host's actual ephemeral
  dashboard port.
- Fixed: live rebuilds without `--workflow` now target the default process,
  and passive-agent presence no longer reports a nonexistent `WORKFLOW.md`.
- Fixed: `dar self rebuild` now reports rebuild progress and confirms success.
- Fixed: Codex and OpenCode TUI chats now receive the configured agent system
  files instead of silently dropping their identity context.
- Fixed: the web chat tab no longer breaks under the dashboard's periodic
  refresh — the transcript, stream, and composer draft now survive tab
  switches, and sending with Enter or Send no longer reloads the page.

- External workflows now expand `$AGENT_HOME` in `workspace.root` to the agent identity folder while relative roots and `{{ workflow.dir }}` resolve to the canonical WORKFLOW.md folder.
- Fleet dashboard: same-named workers using different workflow paths now get distinct, stable labels.
- Secrets: Plane and Linear requests now read agent-root credentials through a shared provider, so running host and MCP bridge processes observe valid `.env` rotations without `reload_secrets` while preserving process-env precedence, last-known-good fallback, child scrubbing, and redaction.

### Added
- Dashboard Cron tab: full-width redesign with humanized schedules ("daily at
  08:00", "every 15 min") and a relative next/last run summary, inline
  "Run now" / "Disable"/"Enable" controls per job, and a uniform-height row
  layout (each row shows only its 3 most recent outputs). Clicking a job's
  name, or its "all N outputs" line, opens a job-detail drawer with the full
  prompt and complete output history; clicking any output opens its full
  content in the same drawer.
- WORKFLOW.md prompt templates can now reference `{{ workflow.dir }}`
  (directory containing the resolved WORKFLOW.md) and `{{ workflow.file }}`
  (the WORKFLOW.md path itself), alongside the existing `{{ issue.* }}` and
  `{{ attempt }}` variables.
- Artifacts: `artifact.publish` now copies exports into host-private immutable storage under OS data home, instead of agent-writable `data/`. Publishing is limited to 25 MiB for cross-surface compatibility (including Slack). Delivery APIs validate canonical vault metadata and atomically claim each surface/origin destination before upload.
- Secrets: `reload_secrets` host tool refreshes only the invoking MCP bridge's
  local `.env` state. The live host independently applies valid `.env` changes
  on its next poll, refreshing opted-in caches such as the tracker without a
  restart. Secret values stay scrubbed from logs and child env.
- tracker-plane: new Plane issue tracker extension (`tracker.kind: plane` in
  `WORKFLOW.md`), scoped to a workspace + project(s) via `tracker.workspace` /
  `tracker.projects` (or the `extensions.tracker-plane` fallback:
  `workspace`, `projects`, optional `api_url`/`app_url`). Polls work items,
  skips issues blocked by non-terminal `blocked_by` relations, parks stalled
  issues to a needs-human state with a comment, and honours Plane's
  rate-limit headers. Auth via `PLANE_BOT_TOKEN` / `PLANE_OAUTH_TOKEN`
  (Bearer, preferred) or `PLANE_API_KEY` (`X-API-Key`). Exposes a `plane_api`
  host tool (host-held auth, structured errors, token redaction) so agents can
  call the Plane REST API, supports `tracker.mention` to target work items whose
  descriptions mention a Plane bot user, and `init-workflow.plane` /
  `export.plane` commands that scaffold a Plane `WORKFLOW.md` and snapshot the
  project's work items.
- CLI: `dar create` — front-door command that scaffolds an agent workspace
  (`agent.yaml` + `.gitignore`, plus `WORKFLOW.md` when the orchestrator loop is
  enabled), usable interactively or via flags. Fills the gap where nothing
  scaffolded `agent.yaml`.
- runner-builtin: zero-install builtin runner porting OpenAI-compatible
  streaming and tool calls into dar, kept optional behind stock extension
  feature gates.
- runner-builtin: builtin coding tools — Pi-like file and shell tools for
  zero-install agents, carrying image outputs through tool results for read
  parity.

### Fixed
- Web chat attachment paths given to the agent were missing the session
  directory, so the agent hit ENOENT reading any uploaded file.
- CLI: `dar build` agent-local binaries now link the opt-in web chat extension
  (and its chat backends) when `extensions.chat-web` is present; previously
  the section was silently ignored.
- CLI: standalone `dar self rebuild` now swaps the rebuilt binary and exits
  instead of re-executing itself indefinitely.
- Secrets: `dar run` now detects valid agent `.env` content changes on its poll
  tick, including removals and file deletion, and refreshes opted-in consumers
  without disrupting active work. MCP bridge `reload_secrets` now accurately
  reports its bridge-local scope.
- CLI: agent builds now link only the tracker selected by `tracker.use`, keeping
  unused stock trackers out of generated agent binaries.
- tracker-plane: treat Plane's `X-RateLimit-Reset` as a Unix epoch timestamp
  (like Linear) rather than a duration, so an exhausted rate-limit bucket sleeps
  until the real reset instead of hanging the tracker for decades.
- tracker-plane: a single work item's relations fetch failing (e.g. a 404 for a
  deleted item) no longer aborts candidate discovery for the whole project — that
  item is logged and treated as unblocked while the rest of the tick proceeds.
- tracker-plane: parking an issue to needs-human now succeeds even when the
  explanatory comment POST fails (the state move is what matters); the comment
  failure is logged instead of masking the successful park.
- tracker-plane: guard cursor pagination against a page that reports more results
  but returns no advancing cursor, which previously looped forever.
- tracker-plane: truncate HTTP error bodies on a UTF-8 char boundary so a
  multibyte Plane error message can no longer panic the tracker.
- tracker-plane: keep reconcile lookups from applying `tracker.mention`, avoiding false "issue file missing" cancellations when Plane's mention index omits an active work item.
- Scheduler: pass configured runner settings so cron jobs use the selected
  provider, model, and thinking level, and surface builtin runner errors instead
  of a generic exit code.
- Scheduler: pass the agent's system context (`AGENTS.md`, `system_files`, and
  workspace skills) into scheduled runs. Runners with a system-prompt channel
  receive it there; other runners get it prepended to the job prompt.
- Composer: omit `runner-builtin` unless `runner.use: builtin`, preventing its
  globally registered tools from colliding with pi MCP direct tools.
- Scheduler: capture streamed responses so successful scheduled runs no longer
  record an empty response, falling back to text deltas when a runner omits the
  final assistant event.
- Scheduler: populate the new `ToolOutcome` content field for error results to
  keep scheduler tools building.
- Scheduler: drive the turn-decision channel for turn-capable runners
  (pi/codex/opencode). A scheduled job's single-shot run now sends
  `TurnDecision::Finish` at the first turn boundary so the long-lived child
  exits cleanly instead of parking until `job_timeout` and being reported as a
  false `TimedOut`. Turn-opt-out runners (`cli`/`fake`) are unaffected (ALG-348).
- Web chat: a session reset is now persisted to the chat transcript, so
  connected browsers see the reset immediately and the notice survives page
  reloads (previously the reset event never reached SSE clients).

### Changed
- Web chat transcript labels agent turns with the agent's `name` from
  `agent.yaml` (falls back to `Agent`).
- Web chat composer redesigned around icon buttons: attach (paperclip) left of
  the input, send (arrow) right of it, and a stop button that appears only
  while a turn is running; the Compact button is replaced by the `/compact`
  chat command.
- CLI: `dar init-workflow` / `dar export` now route to the tracker-specific
  command for the resolved `WORKFLOW.md`'s `tracker.kind` (e.g.
  `init-workflow.plane`, `export.plane`), falling back to the
  tracker-agnostic command when no tracker-specific one is registered.
  Files/Linear behavior is unchanged.
- **⚠ BREAKING — config home**: the issue-loop config (tracker, polling, workspace) is no
  longer readable from `agent.yaml` — it lives entirely in `WORKFLOW.md`
  frontmatter now. `agent.yaml`'s `tracker`, `orchestrator`, and `workspace`
  top-level keys are removed from the schema and silently ignored if present
  (unknown keys); `agent.yaml` retains `id`, `name`, `runner`, `hitl`,
  `dashboard`, `foreground`, `providers`, `extensions`, `system_files`. The
  orchestrator loop now runs whenever the resolved `WORKFLOW.md` exists and
  its frontmatter validates (`tracker.kind` plus non-empty `active_states`/
  `terminal_states`); no `agent.yaml` gate. `dar create --orchestrator` and
  `dar init-workflow` no longer write the retired trio into `agent.yaml`.
- **`dar run`/`doctor`/`export --workflow <path>`**: one agent identity
  (`agent.yaml`, `.env`, system files) can now drive more than one
  `WORKFLOW.md` — "one agent, many hats." `--workflow` accepts a directory
  (its `WORKFLOW.md` is used) or an explicit `…/WORKFLOW.md` path; workflow
  identity is the canonical resolved path, so re-running the same value
  resumes its state. The default workflow (`<agent>/WORKFLOW.md`) keeps the
  legacy `<agent>/data/store.db` + `<agent>/logs/agent.log` layout
  byte-identical; a non-default workflow's run-history db + logs live under
  `<agent>/workflows/<key>/{data/store.db,logs/agent.log}` so concurrent
  workflows never share state, while identity paths always stay on the agent
  root. `workspace.root` now resolves relative to the WORKFLOW.md's own
  directory rather than always the agent root. A non-default `--workflow`
  process skips agent-singleton extensions (the scheduler) so one agent
  identity connects to that external surface at most once; chat backends work
  normally in `--workflow` processes. `dar dash` presence now tracks one live
  entry per agent + workflow, and a workflow's own dashboard header shows
  `id · folder · workflow`.
- **⚠ BREAKING — tracker project scope unified**: Linear's `tracker.project_slug` and
  Plane's `tracker.project` are replaced by one tracker-agnostic
  `tracker.projects` key in `WORKFLOW.md` frontmatter, accepting a scalar or a
  list. Linear matches project(s) with an OR-of-equality GraphQL clause
  (never an unverified `in`); Plane fetches and merges per configured
  project, with an empty/absent list still fetching the whole workspace as
  before. Multiple Linear/Plane projects can now be polled by one tracker.
  `dar export` requires exactly one configured project and bails clearly on 0
  or more than 1.
- Runners: spawn agent children with non-interactive git env (`GIT_EDITOR=true`,
  `GIT_SEQUENCE_EDITOR=true`, `GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`/`PAGER=cat`)
  so editor/pager/credential prompts auto-resolve instead of hanging the run until
  the stall guard parks it (ALG-291).
- Worker prompt: added Git Usage guidance steering workers away from
  interactive/TTY-blocking git commands (ALG-291).

### Removed
- Web chat no longer accepts the obsolete `extensions.chat-web.sessions_dir` config key (ignored since sessions became shared with the TUI); remove it from `agent.yaml` if present.

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

[0.4.0]: https://github.com/tobalsan/dar/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/tobalsan/dar/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/tobalsan/dar/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tobalsan/dar/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tobalsan/dar/releases/tag/v0.1.0
