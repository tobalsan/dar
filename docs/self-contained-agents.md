# Self-contained agents (build B)

There are two independent build models. Build A is the from-source build (repo
dev, monolith) — one `cargo build` producing one binary with all stock
extensions baked in. Build B is the agent-specific, self-contained build
described here: each agent folder carries its own composition crate and
binary — no shared repo checkout required. Move the folder, move the agent.

## Setup (one-time per agent)

```bash
# Requires: Rust toolchain + cargo on the host (or vendor deps for offline use)

dar init-build --dir ./my-agent          # writes .dar/ — commit it
dar init-build --dir ./my-agent --vendor # also vendor deps for offline/air-gap

dar build --dir ./my-agent               # → ./my-agent/bin/dar
dar build --dir ./my-agent --vendor --offline   # air-gapped build
```

`.dar/` is the composition crate for this agent. Commit it alongside
`agent.yaml` and `WORKFLOW.md`; it pins which extensions the agent links.

## Worked example: a `worker` agent

A self-contained agent at `~/agents/worker` using the Linear tracker, the
Codex runner, a chat-enabled TUI, and one agent-local extension.

Prerequisites: `cargo`/`rustc` on PATH, plus the `cargo-dar` helper
(built from a repo checkout with `cargo build --release`, then put
`cargo-dar` on PATH).

1. Write `~/agents/worker/agent.yaml` — the `use:` / `foreground:` keys
   decide which stock extensions get linked. `tracker.use` here only selects
   the linked tracker crate at build time; the tracker's actual behavior
   (project scope, states, …) is configured in `WORKFLOW.md`, not here:

   ```yaml
   id: worker
   name: "Worker"

   tracker:
     use: linear             # links tracker-linear (build-time selection only)
   runner:
     use: codex             # links runner-codex
   foreground: tui          # links tui + frontend-log + chat-pi, and chat-codex
                            # (TUI chat backend follows runner.use)
   ```

   The Linear tracker needs `LINEAR_API_KEY` (personal API key) or
   `LINEAR_OAUTH_TOKEN` (OAuth app token, sent with a `Bearer ` prefix) in
   `~/agents/worker/.env`.

2. Scaffold the prompt and one local extension (run from inside the folder):

   ```bash
   cd ~/agents/worker
   dar init-workflow --dir . --linear-project-slug abc123  # writes WORKFLOW.md with tracker.projects
   cargo dar new standup-poster --kind background          # → extensions/standup-poster/
   ```

   Add `tracker.delegate: "@workeragent"` (or `tracker.team`, `tracker.label`,
   …) to the generated `WORKFLOW.md` frontmatter for any other Linear filters
   — see [Linear tracker](trackers.md#linear-tracker).

   The new crate's `Cargo.toml` carries the discovery marker
   `[package.metadata.dar] factory = "standup_poster::extension"` and a
   `pub fn extension() -> Box<dyn Extension>`. The scaffold pins `host-api`
   to the same `git`/`rev` source the composer uses for stock crates.

3. Bootstrap, build, run:

   ```bash
   dar init-build --dir .   # generates .dar/ (commit it)
   dar build --dir .        # → ~/agents/worker/bin/dar
   ./bin/dar run
   ```

**Where each extension comes from in the final binary:**

| Extension | Source | How it's linked |
|---|---|---|
| `orchestrator`, `tracker-linear`, `runner-codex`, `chat-codex`, `chat-pi`, `tui`, `frontend-log` | the dar repo | pinned `git = "…", rev = "…"` dep in `.dar/Cargo.toml`, feature-gated — only the subset `agent.yaml` selects is compiled in |
| `standup-poster` | the agent's own `extensions/standup-poster/` | relative `path = "../extensions/standup-poster"`, auto-discovered via its `[package.metadata.dar] factory` marker |

`orchestrator` and `tracker-linear` are always linked; the rest of the stock
subset follows `tracker.use` / `runner.use` / `foreground`. The composer
regenerates both the `[dependencies]` and the `plugins![…]` list in
`.dar/` on every `init-build` / `build` — stock entries
`#[cfg(feature = …)]`-gated, local entries always present, never hand-edited.

## Local extension crates

Drop an extension crate under the agent's `extensions/` folder. The composer
auto-discovers crates with dar package metadata. Scaffold one:

```bash
cargo dar new my-extension --kind background   # or service | foreground
```

The agent's `.dar/` composition root lists only what this agent needs —
unrelated stock extensions are not linked.

## Self-update loop

Offline rebuild recompiles and atomically swaps the agent binary, but does not
restart a running process:

```bash
dar self rebuild --dir ./my-agent
dar self rebuild --dir ./my-agent --vendor --offline   # air-gapped
```

Live rebuild finds a running agent by its `agent.yaml` `id`, then recompiles,
swaps, and restarts it in place:

```bash
dar self rebuild my-agent  # targets the default process, with or without WORKFLOW.md

# Select an exact workflow process.
dar self rebuild my-agent --workflow ./my-agent/workflows/release

# Required when dashboard presence uses a non-default registry directory.
dar self rebuild my-agent --registry-dir /path/to/registry
```

Without `--workflow`, live rebuild targets the agent's default process even
when other workflow processes are live. `--workflow` selects a process by its
canonical `WORKFLOW.md` path.

Live rebuild requires the dashboard extension, matching presence registry, a
Rust toolchain, and a running DAR-27-capable agent. The host recomposes `.dar/`,
builds, applies the `dar doctor` gate, atomically swaps `bin/dar`, then `execv`s
that explicit binary with the original `dar run` arguments. The CLI reports
success only after observing a changed boot identity and healthy endpoint; it
times out after 60 seconds otherwise. Build flags such as `--vendor` and
`--offline` are supported only with offline `--dir` rebuilds.

A host running a pre-DAR-27 binary needs one manual bootstrap: run the offline
`--dir` rebuild, then restart that agent once. Later updates can use live
name-based rebuilds.

To bump dependencies deliberately (then commit the updated lock):

```bash
dar lock-refresh --dir ./my-agent
```

## Portability

`dar build` runs `cargo build --release` against the host's native
target — the result is a dynamically-linked binary for that platform/arch.
`bin/dar` can be copied to another host only if that host is
ABI-compatible (same OS, arch, and libc). It is not a portable static binary.

A Rust toolchain is not needed merely to *run* `bin/dar`, but is
required to rebuild or self-update (`dar build` /
`dar self rebuild`).

For truly air-gapped hosts, run `init-build --vendor` once on a connected
machine, commit the `vendor/` tree inside `.dar/`, then build offline
with `--vendor --offline`.
