# MCP servers

Dar can expose tools from user-configured MCP servers through its host bridge. Add `mcp.json` at agent root:

```json
{
  "mcpServers": {
    "local": { "command": "my-server", "args": ["--stdio"], "env": { "TOKEN": "$env:TOKEN" } },
    "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "$env:MCP_AUTH" } },
    "linear": { "url": "https://mcp.linear.app/mcp" }
  }
}
```

Entries support stdio (`command`, optional `args` and `env`) or streamable HTTP (`url`, optional `headers`). `$env:NAME` values resolve from bridge environment. Server names may contain only ASCII letters, digits, and `-` (`^[A-Za-z0-9-]+$`). Underscores are rejected so `<server>__<tool>` names remain unambiguous.

Upstream tool names become `<server>__<tool>`. Names outside MCP-compatible `[A-Za-z0-9_-]{1,64}` after prefixing are skipped. Unreachable servers are warned about and do not stop bridge.

Tools are proxied only; MCP resources, prompts, and sampling are not. Tool calls time out after 120 seconds; server discovery after 15 seconds. Stdio server stderr is discarded so it cannot leak secrets into logs.

Credentials supplied through headers remain in host bridge process and are redacted from bridge output. HTTP servers without a stored login connect with just their configured headers (public servers, or custom headers such as `X-API-Key`). If the server answers 401, it needs OAuth: omit a static `Authorization` header and run:

```sh
dar mcp login <server-name> --dir /path/to/agent
```

Dar discovers OAuth metadata, dynamically registers public client `dar`, opens PKCE authorization in browser, validates callback state, and stores rotating credentials under `data/mcp-auth/<server>.json`. Directory mode is `0700`; credential and lock files use `0600`. Bridge refreshes stored credentials under cross-process file lock. `dar doctor` reports tool count, login requirement, invalid config, or connectivity warning for each server.

## Automatic login

On an interactive terminal, `dar run` probes configured HTTP servers without static authorization before boot. Servers that reject the unauthenticated probe with an authorization error open the same OAuth login flow, sequentially. Login failures warn but never prevent agent boot. Non-interactive runs skip this check.

## Dashboard

Agents with configured MCP servers expose an **MCP** dashboard tab. It shows transport and local credential status without contacting or spawning servers. OAuth-capable HTTP servers can be connected, reconnected, or disconnected there. OAuth state expires after ten minutes. Running agents' runners use changed credentials on their next MCP bridge start.

Runners reach these tools through the host bridge they already use, so `pi`, `codex`, `opencode`, and `builtin` need no extra setup. The `pi` runner needs `pi-mcp-adapter` installed on the host (it provides `--mcp-config`); without it pi exits with `Unknown option: --mcp-config`. MCP servers a runner already loads from its own config keep working alongside these.

## MCP Apps in web chat

With chat-web enabled, the same `mcp.json` entries can provide interactive
HTML views using the stable `io.modelcontextprotocol/ui` extension
(2026-01-26, `text/html;profile=mcp-app`). Tools associate a view through
`_meta.ui.resourceUri`; the deprecated `_meta["ui/resourceUri"]` is also read.
Most servers need no extra setup; local origin overrides are described below.
Remote servers use the existing OAuth
login and credential refresh flow. Vendor access restrictions still apply:
successful MCP Apps hosting does not grant access to a vendor's OAuth client
allowlist or paid account.

The model always receives the tool's text fallback. Tools whose
`_meta.ui.visibility` excludes `"model"` stay out of its tool list. A view
can call only its own server's tools with `"app"` visibility and read its
own server's resources. Missing visibility means `["model", "app"]`.
View calls require no per-call approval; Dar logs their method, server,
tool, and status, together with each view's enforced CSP.

Views run through a sandbox proxy and real HTTP app document with a separate
cookie scope from chat. By default, each server gets `<server>.localhost` on the
sandbox port; views from the same server share an origin and storage.
The backend enforces each server's assigned host for both proxy and content
requests. The proxy and its app document form one untrusted server boundary.
Different servers remain isolated unless the operator explicitly configures
the global shared `sandbox_origin` override.

Network and asset
access follow the resource's declared CSP domains; no CSP declaration means
no external network access. Declared CSP also permits blob scripts and images,
while workers remain blocked. Camera, microphone, geolocation, and clipboard
permissions are currently withheld; the host reports no device grants.

For local tldraw, reserve `http://localhost:<sandbox_port>` for that server
with `extensions.chat-web.apps.sandbox_origins.tldraw`, and open chat at
`http://127.0.0.1:<chat_port>`. Leave other servers on their own hostnames.
The override key is the server ID from `mcp.json`, not a vendor name built
into Dar. Ports and sibling remote hostnames do not isolate cookies: Dar rejects
overlapping cookie domains using the Public Suffix List, including overlap
between chat and any app. The local `localhost`/`<server>.localhost` setup
has been verified in Chromium; other browsers need separate validation.
An invalid origin assignment leaves the text fallback visible. This local
setup does not establish tldraw compatibility
on a remote HTTPS domain.
See [web chat](chat.md) for configuration and view controls.

Dar uses separate MCP connections for the model bridge and the web Apps
catalog. Servers that maintain client-local state may treat them as separate
sessions. The catalog caches HTML for its connection lifetime. Tool results
are retained in `data/mcp-apps/results/` for 30 days; missing records or
unavailable resources leave the text fallback visible with a short error.
The model sees a small result-record marker in tool text; web transcripts
hide it. TUI, Telegram, and Discord continue using text.
Pi uses exact aliases recorded by the bridge when all its tool names are
unambiguous ASCII names that need no shortening. If any tool name is non-ASCII,
collides after sanitization or needs shortening, all Pi app results from that bridge
retain the text fallback.

View reuse across tool calls is a Dar extension to the stable spec. Servers
that expect one result per view can be listed in
`extensions.chat-web.apps.no_reuse`. Set `no_reuse: [tldraw]` when that is
the server ID: repeated-result testing showed that the tldraw app needs a
fresh view per execution. Other servers reuse views
by default; this is configuration, not a built-in vendor exception.
OpenAI Apps SDK (`window.openai`),
picture-in-picture, partial tool inputs, host theming, and app-provided tools
are not supported. Edits made inside a view need not survive reload.
