# ALG-254 toy-tool experiment artifacts

Reproducible MCP toy tools used to prove native dynamic-tool support per backend.
Backend versions tested: pi 0.79.3, codex-cli 0.139.0, opencode 1.17.4.

## Tools
- `toy_mcp.py` — stdio MCP server exposing `echo_upper(text)` → uppercases text.
- `toy_err.py` — stdio MCP server exposing `always_fail(x)` → returns `isError:true`.

## Setup

The committed `*-mcp-config.json` files reference absolute paths under
`/tmp/alg254-toy/`. To reproduce as-is, copy the toy servers there first:

    mkdir -p /tmp/alg254-toy && cp toy_mcp.py toy_err.py /tmp/alg254-toy/

Or edit the config paths to point at this directory.

## Reproduce

### codex (MCP server config)
    codex mcp add toy -- python3 $PWD/toy_mcp.py
    codex exec --skip-git-repo-check -c approval_policy='"never"' \
      -c sandbox_mode='"danger-full-access"' \
      "Use the echo_upper tool with text 'hello from spike' and report what it returns."
    codex mcp remove toy   # cleanup (global registration)

### pi (--mcp-config)
    pi --mode text --mcp-config $PWD/pi-mcp-config.json \
      -p "Call echo_upper with text 'hello from spike' and report what it returns."

### opencode (mcp block in opencode.json)
    OPENCODE_CONFIG=$PWD/opencode-mcp-config.json opencode run --print-logs \
      "Call the echo_upper tool with text 'hello from spike' and report what it returns."

All three returned `HELLO FROM SPIKE`. The `toy_err.py`/`always_fail` variant
surfaced a structured failure (codex: `(failed)`) without stalling the run.
