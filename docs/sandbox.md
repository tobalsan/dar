# Docker sandbox

Run an agent in a hardened Docker container so it can only touch what is
mounted. Same machine as the agent folder; no separate deploy step.

## Quick start

```bash
dar create ./my-agent --runner builtin --sandbox   # new agent
dar sandbox ./my-agent                             # or convert an existing one
# fill provider keys in ./my-agent/.env
dar build --dir ./my-agent                         # native + bin/dar-sandbox
cd ./my-agent && docker compose up -d
```

Update: `dar build`, then `docker compose restart`.

## What `dar sandbox` does

Idempotent: writes only missing files, prints written/skipped lists. Edits
`agent.yaml` as text (comments kept) by appending `sandboxed: true`, plus
`dashboard: port: 7878` when chat-web needs it. Refuses agent.yaml forms it
can't edit safely (flow style, `...` end) and asks you to add the key by hand.

Writes `Dockerfile`, `docker-compose.yml`, `.env.example`, `.env` (mode 600,
your UID/GID), `SANDBOX.md`, and the `memory/`, `skills/`, `data/`, `logs/`,
`workspaces/`, `cron/` folders (+ `pi-agent/` for pi). `.env`, `bin/`,
`pi-agent/` are git-ignored.

Runners: `builtin` (Debian slim + CA certs; provider keys come from `$env:`
refs in `.env`) and `pi` (Node + pi CLI; auth in `./pi-agent`). Others error.

## Static binary

With `sandboxed: true`, `dar build` also compiles a static musl
`bin/dar-sandbox` inside `rust:1.96-alpine` for the Docker host's
architecture (works from macOS). The binary is bind-mounted read-only, not
baked into the image. The build mounts the dar checkout (`DAR_SRC`) because
`.dar/Cargo.toml` path-deps point into it. `--universal` is rejected for
sandboxed agents.

## Security model

- Non-root (`user: UID:GID` from `.env`), `cap_drop: ALL`,
  `no-new-privileges`, read-only root fs, 2 CPU / 4 GiB / 512 PIDs.
- No Docker socket, ever.
- Read-only: `agent.yaml`, `AGENTS.md`, `TOOLS.md`, `WORKFLOW.md`, other
  system files, `skills/`. Writable: `memory.md`, `memory/`, `data/`,
  `logs/`, `workspaces/`, `cron/` (+ `pi-agent/`).
- Not mounted: `Dockerfile`, `docker-compose.yml`, `.env` — the agent cannot
  plant code the host later runs.
- Writable mounts that resolve (via symlink) outside the agent folder are
  rejected.
- `system_files` that are absolute or escape the folder are warned about and
  not mounted.

## Chat web

chat-web is served by the dashboard server, whose port defaults to `0`
(ephemeral) and can't be published. With chat-web enabled, the sandbox pins
`dashboard.port` (7878 unless already set) and publishes it on host loopback:
`http://127.0.0.1:7878/chat` (override host port with `CHAT_PORT` in `.env`).
A loopback `dashboard.bind` or ephemeral port is refused. For remote access
put a proxy (e.g. `tailscale serve`) in front. Use `foreground: logs`; the
container has no TTY for the TUI.

## Workspace mounts

Uncomment `- ${WORKSPACE_SRC}:/code` in `docker-compose.yml` and set
`WORKSPACE_SRC` in `.env`. Add more lines the same way.

## Gotchas

- Host paths must exist before `up`, else Docker creates root-owned dirs.
- Files added later (e.g. `TOOLS.md`) need a compose line to be mounted.
- `memory.md` is a single-file mount: rename-replacing it fails (EBUSY).
- Stale `.dar` crates may need `dar lock-refresh` before `dar build`.
