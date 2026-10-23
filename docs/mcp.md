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

The server name `dar` is reserved for Dar's built-in tools and artifact viewer.
An `mcp.json` entry with that name is rejected at startup and by `dar doctor`.

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

Dar's built-in `dar` app displays `artifact_publish` results even without
`mcp.json`. Its `ui://dar/artifact-viewer` page uses the same sandbox listener,
origin isolation, view reuse, replay, and audit log as upstream apps. It can
read only the artifact referenced by its own session result and cannot call
tools. Downloads require that same session record and are always attachments
with `application/octet-stream` and `nosniff` on the chat origin.

HTML reports run inside a further opaque-origin frame, separated from the
viewer bridge. Resource domains are limited to `https://cdn.jsdelivr.net`,
`https://cdnjs.cloudflare.com`, `https://unpkg.com`,
`https://fonts.googleapis.com`, and `https://fonts.gstatic.com`; connection
domains are empty. Fetch, XHR, and WebSocket calls are blocked even to those
hosts. Allowed asset requests and external HTTP(S) links remain possible;
this does not promise complete protection against data leaks. Markdown is
sanitized; SVG is displayed as an image, never inserted as live markup.
See [artifact views](chat.md#mcp-app-views) for supported formats and limits.

With chat-web enabled, the same `mcp.json` entries can provide interactive
HTML views using the stable `io.modelcontextprotocol/ui` extension
(2026-01-26, `text/html;profile=mcp-app`). Tools associate a view through
`_meta.ui.resourceUri`. Dar also reads the deprecated `_meta["ui/resourceUri"]`.
Most servers need no extra setup. Origin overrides are described below.
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

### Why each view needs its own host

A view is HTML and JavaScript from the MCP server vendor. Dar does not trust it.
Dar reads the view with the MCP `resources/read` call. Then Dar serves it
from the sandbox port, a second listener that never serves chat.

If the view shared the chat origin, it could read chat data and call the Dar
API. A different origin stops this. A port alone does not isolate cookies.
So the host name must also differ. Dar uses the Public Suffix List (PSL) to
compare cookie scopes. It rejects an app origin with the same scope as chat.
It also rejects overlap between servers, except for the global `sandbox_origin`.
Claude uses the same design with `*.claudemcpcontent.com`.

Dar checks the assigned host on both the proxy and the content routes. The
proxy and its app document form one untrusted server boundary. Views from one
server share an origin and storage. Different servers stay isolated.

Network and asset access follow the resource's declared CSP domains. No CSP
declaration means no external network access. Declared CSP also permits blob
scripts and images. Workers stay blocked. Dar withholds camera, microphone,
geolocation, and clipboard permissions. The host reports no device grants.

### How Dar picks the host

For local chat, each server gets `http://<server>.localhost:<sandbox_port>`.
Local chat means chat on `localhost`, on a `*.localhost` name, or on a loopback IP.

For remote chat, Dar cannot know the name that the browser sees. You must set
`sandbox_origins.<server>` for each server, or one shared `sandbox_origin`.
The key is the server ID from `mcp.json`, or `dar` for built-in artifact views.
Dar has no built-in vendor names.
An invalid assignment leaves the text fallback visible.
Existing remote setups with only per-server overrides must also add
`sandbox_origins.dar`, even when opening an upstream app: origin isolation
checks include the built-in server. A shared `sandbox_origin` covers `dar`
when it has no individual override.

The value `loopback` is valid only in `sandbox_origins.<server>`. Dar resolves
it for each request from the chat host and the bound sandbox port:

- Chat on `localhost` or a `*.localhost` name gives `http://127.0.0.1:<port>`.
- Chat on a loopback IP gives `http://localhost:<port>`.
- Any other chat host is an error.

With Docker mapped ports, the host port can differ from the container port.
`loopback` uses the bound port, so set explicit origins in that case.

### Setups

| Setup | Most apps (Excalidraw, Canva) | tldraw |
| --- | --- | --- |
| Chat on `localhost` | Default `<server>.localhost` | `tldraw: loopback` |
| Chat on `127.0.0.1` | Default `<server>.localhost` | `tldraw: loopback` |
| Tailnet, plain HTTP | `sandbox_origin: http://100.x.y.z:7879` | Untested |
| HTTPS with Tailscale Serve | Domain with a different cookie scope | No license for the host |
| Public HTTPS | Separate registrable domain | No license for the host |

Tailnet plain HTTP: open chat on the MagicDNS name. Serve apps on the tailnet
IP. Set `sandbox_bind` to that IP or to `0.0.0.0`. WireGuard encrypts the
traffic. Plain HTTP is not a secure context, so some browser features are off.

Tailscale Serve: `ts.net` is on the PSL. All hosts in one tailnet share the
cookie scope `<tailnet>.ts.net`. Apps need a different cookie scope, for
example your own domain. A
browser blocks an HTTP frame in an HTTPS page.

Public HTTPS: put apps on a separate registrable domain, for example
`https://agent-apps.example.net` beside `https://agent.example.com`. Proxy
both ports.

### tldraw

The tldraw MCP App ships a license key for `*.claudemcpcontent.com`,
`*.web-sandbox.oaiusercontent.com`, and `vscode-webview://` only. On other
hosts the tldraw SDK renders only in development mode. That mode needs plain
`http:` on any host except `*.localhost`, or `https:` on a loopback host.
Without a valid key on a production host, tldraw stops rendering after 5 seconds.

For local use, set `sandbox_origins.tldraw: loopback` and `no_reuse: [tldraw]`.
Chat can then run on `localhost` or on `127.0.0.1`.

tldraw does not render on remote HTTPS without a tldraw license for that
host. You can self-host the MIT tldraw MCP App and build it with your own
hobby or commercial key. Do not patch or bypass the key. The license forbids it.
Tailnet tldraw is untested.

Excalidraw and other apps with no license check have no such limit.

See [web chat](chat.md) for configuration and view controls.

Dar uses separate MCP connections for the model bridge and the web Apps
catalog. Servers that maintain client-local state may treat them as separate
sessions. The catalog caches HTML for its connection lifetime. Tool results
are retained in `data/mcp-apps/results/` for 30 days. Missing records or
unavailable resources leave the text fallback visible with a short error.
The model sees a small result-record marker in tool text. Web transcripts
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
by default. This is configuration, not a built-in vendor exception.
OpenAI Apps SDK (`window.openai`),
picture-in-picture, partial tool inputs, host theming, and app-provided tools
are not supported. Edits made inside a view need not survive reload.
