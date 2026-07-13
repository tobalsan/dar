---
tracker:
  kind: files
  path: ./issues
  active_states: [todo, in_progress]
  terminal_states: [done, cancelled]

polling:
  interval_ms: 10000
  max_concurrent: 1
  max_retries: 3
  retry_backoff_ms: 30000

workspace:
  root: ./workspaces
---

You are working on issue {{ issue.identifier }}: {{ issue.title }}

{{ issue.description }}

Do the task above in your current working directory (this is your isolated workspace).

When you finish the task, edit the issue file at `../../issues/{{ issue.identifier }}.md`
and change its frontmatter `state:` field to `done`. Then stop.
