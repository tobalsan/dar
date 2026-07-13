# Changelog

All notable changes to Dar are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0:
the minor slot carries breaking changes, the patch slot carries compatible ones).

Breaking changes are marked **⚠ BREAKING**.

## [Unreleased]

### Added
- WORKFLOW.md prompt templates can now reference `{{ workflow.dir }}`
  (directory containing the resolved WORKFLOW.md) and `{{ workflow.file }}`
  (the WORKFLOW.md path itself), alongside the existing `{{ issue.* }}` and
  `{{ attempt }}` variables.
- Artifacts: `artifact.publish` now copies exports into host-private immutable storage under OS data home, instead of agent-writable `data/`. Publishing is limited to 25 MiB for cross-surface compatibility (including Slack). Delivery APIs validate canonical vault metadata and atomically claim each surface/origin destination before upload.
- Secrets: `reload_secrets` host tool — agents can rotate `LINEAR_API_KEY` /
  `LINEAR_OAUTH_TOKEN` (or any `.env` secret) and refresh the running host
  without a restart. It re-reads `.env` (overriding only keys originally loaded
  from the file, never genuine process env) and, inside `dar run`, swaps the
  Linear client's cached token in place via a new `ReloadSecrets` control
  message. Reachable through the host MCP bridge; the next Linear request uses
  the new token. Secret values stay scrubbed from logs and child env.
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

### Changed
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
