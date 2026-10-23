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
    idle_minutes: 5    # optional; close an idle conversation's backend after N minutes (default 5, 0 = never)
```

Idle conversations: after `idle_minutes` with no running turn, chat-web closes
that conversation's backend process. Transcript and identity are kept; the next
message transparently reopens the same saved conversation. Conversations whose
backend never reported a session identity (e.g. codex) are never closed.

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
`GET /chat/avatar`. Paths outside the agent folder are ignored. The avatar
also becomes the browser favicon and the apple touch icon; without an avatar
the page keeps its default icon.

**Notification sound.** A short sound plays when a turn finishes. The bell
button in the dashboard tab bar turns it on or off for all chats. To give an
agent its own sound, put an audio file in the agent folder and name it with a
top-level `notification_sound` in `agent.yaml`
(`notification_sound: sounds/ping.mp3`). Use mp3, wav, ogg, or m4a. Chat web
serves the file at `GET /chat/notify.mp3` and reads it again for each request.
A change needs no restart. If the file is missing or outside the agent folder,
chat web plays the default sound.

**New chat.** An empty chat shows a short invite with the composer centered
below it; the composer glides down to its dock once the first message is
sent. The header's **New chat** button (or `/new`) starts a fresh chat; the
previous one stays in the sidebar.

**Composer.** Attach (paperclip), send, and a stop button that appears only
while a turn is running. Files can also be pasted or dropped anywhere on the
chat; pending images show as thumbnails. Messages sent while a turn is
running are queued by the backend. Slash commands: `/stop` aborts the running
turn, `/compact [instructions]` compacts the session context natively on every
backend (codex ignores the instructions), `/new` starts a fresh chat.
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

### MCP App views

`artifact_publish` shows published files directly under the tool call with all
four backends: pi, codex, opencode, and builtin. No `mcp.json` is needed.
Publish a report instead of running `open`; an optional `title` labels its
frame, otherwise the filename is used. HTML renders with styles and scripts,
Markdown as sanitized formatted text with Mermaid diagrams, PDFs as pages,
images (including SVG) as images, and text/code as preformatted text. Other
formats show a file card. The artifact header shows its title, type, and size.

The frame's **Download** button saves the exact original vault bytes. Publishing
the same filename in the same conversation updates its existing view; a different
filename gets a separate view. The view moves to the latest tool call and reloads there. Each older call shows a quiet "newer version below" line. Click it to scroll to the view, or to highlight the view in the side panel.
Reload and saved-session replay restore views while their result records exist
(30 days). A missing or unreadable file leaves the text fallback and a short error.

On screens wider than 760 px, every live publish opens its view in the side panel,
replacing the previously expanded view, including when the same file is published
again. A compact "Open in side panel" card remains in the transcript; click it to
find the panel. Closing the panel restores the inline view. Reload, reconnect,
and session replay keep artifacts inline; narrow screens do not auto-open.

**PDFs.** PDF.js renders pages at the view width, with a page count and accessible
page labels. The first 50 pages are shown; download the file for all pages of a
longer document. Pages are drawn at high resolution for expansion, but are not
redrawn on resize. Inline PDFs have no text selection, search, zoom controls,
forms, or annotations; **Open in browser** provides the browser's PDF tools.
PDF.js loads from jsDelivr. Offline, after a 10-second import timeout, or when a
PDF is invalid, damaged, password-protected, or fails rendering, the file card
remains available. Fonts not embedded in the file may use a system font or fail
to display. Some CJK documents need unavailable CMaps. Images requiring Wasm
decoders (including some JPEG 2000 and JBIG2 images) may be absent even when a
page renders.

**Markdown.** Mermaid code blocks render with Pele from jsDelivr. A failed load
or invalid diagram leaves the source as code. Plain Markdown loads no CDN script
and works offline; raw HTML remains sanitized. Links and internal anchors work
as in HTML reports.

**Open in browser** opens HTML, rendered Markdown, PDF, images, and text/code in
a new tab on the sandbox origin. HTML keeps its styles, scripts, and allowed CDN
assets; PDFs use the browser's viewer, and text/code opens as plain text. Other
binary types (such as zip and docx) offer Download only. Each click obtains a
fresh URL, even after the embedded view has been open for hours. A blocked popup
or failed request shows a short error in the view header.

HTML, Markdown, and PDF reports run in an additional opaque-origin frame.
Only embedded Dar code and vendored Markdown/sanitization libraries run in the
viewer; PDF.js, Pele, and report scripts run in the opaque report frame.
Scripts, styles, and fonts
can load from jsDelivr, cdnjs, unpkg, and Google Fonts; fetch, XHR, and WebSocket
connections are blocked, including to those CDNs. This is not a complete guarantee
against data leaks. Reports can follow internal anchors and open HTTP(S) links
in a new tab. They cannot use the viewer's tool bridge. Only single files are
supported: relative assets, editing, and version selection are not included.
HTML, Markdown, and SVG tabs also have opaque origins; browser tabs do not grant
reports fetch, XHR, or WebSocket access. Artifact views cannot post chat messages
or update model context. Open requests and refused calls are audited.
Slack and WhatsApp still upload files; other surfaces keep text.

Tools from servers in `mcp.json` that declare an MCP App appear as a framed
view under their tool call, labeled with the server name. The view receives
the tool arguments before the full result, once its initialization finishes.
Inline height follows the view's size reports, within a host cap.
**Expand** displays the app in a resizable panel alongside the chat; the
composer remains usable. Close the panel to return inline. App-requested
fullscreen is available unless the app explicitly lists display modes without
fullscreen; omitting the list permits both inline and fullscreen views.

A second call to the same server and resource updates the open view. The new
tool row links to that view. Reload and saved-session replay restore the last
input and result for each reused view; user edits inside the app are not
persisted. Missing result records, a stopped server, or a missing HTML
resource retain the tool's text fallback.

```yaml
extensions:
  chat-web:
    apps:
      # Defaults to dashboard.bind.
      sandbox_bind: 0.0.0.0
      # Defaults to dashboard.port + 1; ephemeral when dashboard.port is 0.
      sandbox_port: 7879
      # Optional shared browser-visible origin, without a path.
      # sandbox_origin: https://agent-apps.example.net
      # Keys are mcp.json server IDs, or dar for built-in artifact views.
      sandbox_origins:
        tldraw: loopback
      # Servers whose apps require a fresh view for every call:
      no_reuse: [tldraw]
```

The tested tldraw app did not reliably execute a second result in the same
view; use a fresh view per execution. Other servers reuse views by default.
Both lists accept server IDs from `mcp.json` and `dar` for artifact views;
Dar has no built-in vendor exceptions.

The second HTTP listener serves the sandbox proxy and app documents, never
the chat. Dar assigns each server a browser-visible origin and checks that
assignment on both routes. With an ephemeral sandbox port, it publishes the
actual bound port. By default, local apps use
`http://<server>.localhost:<sandbox_port>`; views from one server share
origin and storage, while different servers use different hostnames.

The example sets `loopback` for the MCP server named `tldraw`. Dar then
uses `127.0.0.1` for its view when chat is on `localhost`, and `localhost`
when chat is on `127.0.0.1`. See [MCP](mcp.md) for the reasons. Ports do not isolate cookies, and sibling remote
hostnames can share cookies. Dar uses the Public Suffix List to reject
overlapping cookie domains between chat and apps, and between servers in
isolated mode. This also rejects parent-child hostname assignments. The
local `localhost`/`<server>.localhost` arrangement is verified in Chromium;
other browsers need separate validation. Invalid assignments show an error
and retain text fallback.

For TLS termination or remote access, proxy separate app cookie domains to the
sandbox listener and configure their browser-visible URLs in
`sandbox_origins`. An explicit global `sandbox_origin` is a shared-origin
exception: servers without per-server overrides share its storage and
cookies. App and chat cookie scopes must always remain separate, for example
`https://agent-apps.example.net` beside `https://agent.example.com`.
Local tldraw rendering does not guarantee rendering on a remote domain.
When Docker's host `SANDBOX_PORT` differs from the container port, use
browser-visible ports in these overrides; see [Docker sandbox](sandbox.md).

An app can post a user message, update selection context for the next turn,
call app-visible tools on its own server, read its server's resources, and
open HTTP(S) links in a new tab. Context is added at the common submission
point for pi, codex, opencode, and builtin, and cleared after the backend
accepts that message. A failed submission keeps the context for retry.
See [MCP servers](mcp.md#mcp-apps-in-web-chat) for visibility, audit, OAuth,
and compatibility limits.

When the agent is passive (no orchestration loop configured), the dashboard
opens on the Chat tab by default; the composer sends with Enter (Shift+Enter
inserts a newline), and while the chat tab is active the dashboard's periodic
content refresh is suspended so the conversation and draft are never torn
down.
