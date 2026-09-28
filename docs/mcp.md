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

Runners reach these tools through the host bridge they already use, so `pi`, `codex`, `opencode`, and `builtin` need no extra setup. The `pi` runner needs `pi-mcp-adapter` installed on the host (it provides `--mcp-config`); without it pi exits with `Unknown option: --mcp-config`. MCP servers a runner already loads from its own config keep working alongside these.
