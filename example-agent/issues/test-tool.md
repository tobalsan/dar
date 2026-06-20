---
id: test-tool
identifier: test-tool
title: "Call the echo_upper host tool"
state: todo
priority: 1
created_at: 2026-01-15T10:00:00Z
---

A host-registered tool named `echo_upper` is available to you over MCP. It takes
a single string argument `text` and returns that text uppercased.

Call `echo_upper` with the argument `{"text": "hello from spike"}`. Then write
the tool's exact output back into this issue file at
`../../issues/test-tool.md`, appending a line of the form:

    result: <the uppercased text the tool returned>

Do not uppercase the text yourself — you must obtain the result by calling the
`echo_upper` tool. When the result line is written, change this issue's
frontmatter `state:` field to `done` and stop.
