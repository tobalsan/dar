#!/usr/bin/env python3
# Minimal stdio MCP server exposing one tool: echo_upper
import sys, json
def send(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
TOOL={"name":"echo_upper","description":"Uppercase the input text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    try: msg=json.loads(line)
    except: continue
    mid=msg.get("id"); method=msg.get("method")
    if method=="initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"toy","version":"0.1"}}})
    elif method=="notifications/initialized":
        pass
    elif method=="tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[TOOL]}})
    elif method=="tools/call":
        args=msg.get("params",{}).get("arguments",{})
        txt=str(args.get("text","")).upper()
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":txt}],"isError":False}})
        sys.stderr.write(f"[toy_mcp] echo_upper called args={args}\n"); sys.stderr.flush()
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
