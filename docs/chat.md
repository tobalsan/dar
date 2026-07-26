# Chat surfaces

dar has two operator chat surfaces — a terminal UI and a web chat — that share one live session.

## Terminal UI (`foreground: tui`)

Set `foreground: tui` in `agent.yaml` to replace the plain log stream with an
in-terminal UI: a **Chat** tab (interactive operator chat with an AI agent
running inside the agent folder), a **Logs** tab (the same lines `foreground:
logs` would print), and a **Dash** tab (run snapshot + Stop/Pause/Resume —
present only when the orchestrator extension is linked).

```yaml
foreground: tui

extensions:
  tui:
    chat:
      backend: pi       # optional; default: follow runner.use, then pi
      command: pi       # optional binary override
```

**Chat.** Backed by a long-lived `pi --mode rpc` child (cwd = the agent
folder, so it can read issues, workspaces, and logs with its own tools), with
session transcripts kept under `data/chat/sessions/` (shared with the web
chat; old `data/tui/sessions/` transcripts are migrated on boot). The first
message is prepended with a context preamble (run snapshot summary +
`issues/` listing).
Backend resolution at first message: `extensions.tui.chat.backend` if set,
else the configured `runner.use` when it has a registered chat backend, else
`pi` (with a transcript notice when the runner had no chat backend; chat is
disabled with a banner when nothing is registered).

**Keys:** `Tab`/`Shift+Tab` cycle tabs. Chat: `Enter` send, `Esc` abort the
in-flight turn, `PgUp`/`PgDn`/`End` scroll. Logs: arrow/page keys scroll,
`End` re-follows the tail. Dash: `p` pause, `r` resume, `s` stop (run state
only — issue files are never touched).

**Quitting quits the whole agent:** `Ctrl-C` anywhere, or `q` on the
Logs/Dash tabs (on Chat it types a "q"), exits the foreground — which shuts
dar down and kills running children, exactly like Ctrl-C on
`foreground: logs`.

When stdout is not a terminal (piped/CI), the TUI degrades to the exact
`foreground: logs` line stream.

## Web chat (`extensions.chat-web`)

Opt-in browser chat on the agent dashboard. Add an `extensions.chat-web`
section to `agent.yaml` (`{}` is enough) and the dashboard grows a **Chat**
tab plus HTTP routes under `/chat`; without the section the extension mounts
nothing.

```yaml
extensions:
  chat-web:
    enabled: true      # optional runtime kill switch; linking is by section presence
    backend: pi        # optional; default: follow runner.use, then pi
    command: ""        # optional backend binary override ("" = backend default)
    idle_minutes: 360  # optional; shut an idle session's child down (default 360)
```

Backend resolution matches the TUI: `backend` if set, else `runner.use` when
that id has a registered chat backend, else `pi`. The web chat and the TUI
share one live session: transcripts live under `data/chat/sessions/`, a turn
started on either surface streams into every open browser tab, and reconnects
replay missed events (SSE with `Last-Event-ID`). Attachments are uploaded via
multipart `POST /chat/{session}/upload` (max 8 files, 8 MiB body) and stored
under `data/chat/uploads/`; the agent turn receives their local paths.
Assistant turns are labeled with the agent's `name` from `agent.yaml` (falls
back to `Agent`).

The composer is a row of icon buttons: attach (paperclip) left of the input,
send (arrow) right of it, and a stop button that appears only while a turn is
running, plus a token meter fed by backend-reported context usage. Two slash
commands are recognized in the chat input: `/compact` passes through to the
backend CLI to compact the session context, and `/new` clears the shared
session with a "Context cleared, started a new session." notice.

When the agent is passive (no orchestration loop configured), the dashboard
opens on the Chat tab by default; the composer sends with Enter (Shift+Enter
inserts a newline), and while the chat tab is active the dashboard's periodic
content refresh is suspended so the conversation and draft are never torn
down.
