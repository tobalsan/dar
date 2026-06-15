#!/usr/bin/env python3
import sys, json
def send(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
TOOL={"name":"always_fail","description":"Always returns an error","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}}}
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    try: msg=json.loads(line)
    except: continue
    mid=msg.get("id"); method=msg.get("method")
    if method=="initialize": send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"toyerr","version":"0.1"}}})
    elif method=="tools/list": send({"jsonrpc":"2.0","id":mid,"result":{"tools":[TOOL]}})
    elif method=="tools/call": send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"boom: simulated tool failure"}],"isError":True}})
    elif mid is not None: send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
