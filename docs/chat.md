# Chat surfaces

dar has two operator chat surfaces — a terminal UI and a web chat — that share one live session.

## Silent replies (`NO_REPLY`)

An agent whose entire final reply is `NO_REPLY` (whitespace, `*`/`**`,
backticks and a trailing `.` tolerated) delivers nothing on any surface: web
chat, TUI, and channel extensions. The instruction is appended to the agent's
system prompt when it has identity files (`AGENTS.md` / `system_files`). Text
sent before a tool call in the same turn is still delivered. The backend's own
session history and the web transcript keep the raw `NO_REPLY` turn.

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
    idle_minutes: 360  # deprecated compatibility key; accepted but ignored
```

Backend resolution matches the TUI: `backend` if set, else `runner.use` when
that id has a registered chat backend, else `pi`. The web chat and the TUI
share one live session: each process boot starts fresh while prior backend
sessions remain available to resume. Web replay lives at
`data/chat/sessions/main.jsonl`; backend sessions stay under
`data/chat/sessions/`, with titles/archive state in `data/chat/sessions-meta.json`.
Turns stream into every open browser tab, and reconnects replay missed events
(SSE with `Last-Event-ID`). Attachments are uploaded via
multipart `POST /chat/{session}/upload` (max 8 files, 8 MiB body) and stored
under `data/chat/uploads/`; the agent turn receives their local paths.
Assistant turns are labeled with the agent's `name` from `agent.yaml` (falls
back to `Agent`). An optional top-level `avatar` in `agent.yaml` shows next to
the name in the chat header and above the new-chat invite: an emoji
(`avatar: "🦉"`), an `http(s)` image URL, or an image path relative to the
agent folder (`avatar: assets/avatar.png`; png/jpg/gif/webp/svg), served at
`GET /chat/avatar`. Paths outside the agent folder are ignored.

**New chat.** An empty chat shows a short invite with the composer centered
below it; the composer glides down to its dock once the first message is
sent. The header's **New chat** button (or `/new`) starts a fresh chat; the
previous one stays in the sidebar.

**Composer.** Attach (paperclip), send, and a stop button that appears only
while a turn is running. Files can also be pasted or dropped anywhere on the
chat; pending images show as thumbnails. Messages sent while a turn is
running are queued by the backend. Slash commands: `/stop` aborts the running
turn, `/compact` compacts the session context, `/new` starts a fresh chat.
The header shows a token meter fed by backend-reported context usage; above
70% of the context window a hint suggests `/compact`.

**Transcript.** Assistant replies render as GitHub-flavored Markdown (tables,
code blocks, lists, blockquotes; raw HTML is escaped and output sanitized,
links open in a new tab). Tool calls and thinking are collapsible, tool rows
showing a one-line argument summary. Messages carry timestamps; a stopped turn
is marked **Interrupted**.

**Sidebar.** Lists saved sessions grouped by recency (Today, Yesterday,
Earlier this week/month, then by month) with relative times and a search box.
Clicking a session resumes it live: its history is replayed and the next
message continues that backend session. Hover a row to rename, archive, or
delete it (delete is permanent and removes the backend session file).
Archived sessions move to a collapsible **Archived** section. After the first
reply, a session is titled automatically (3–6 words) by a one-off call to the
same backend and model, in the background; it never blocks the chat or
appears as a session itself. The sidebar collapses to a rail (remembered per
browser) and becomes a slide-in drawer on narrow screens.

Session routes (all under `/chat`): `GET /sessions` lists sessions,
`POST /main/resume` `{"id": ...}` resumes one, `PATCH /sessions/{id}`
`{"title"?, "archived"?}` renames/archives (empty title clears it), and
`DELETE /sessions/{id}` deletes.

When the agent is passive (no orchestration loop configured), the dashboard
opens on the Chat tab by default; the composer sends with Enter (Shift+Enter
inserts a newline), and while the chat tab is active the dashboard's periodic
content refresh is suspended so the conversation and draft are never torn
down.
