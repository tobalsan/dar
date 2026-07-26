# CLI reference

The `dar` command line covers building, running, and inspecting an agent folder.

```bash
# Bootstrap the per-agent composition crate (.dar/) — one-time setup.
dar init-build --dir ./my-agent
dar init-build --dir ./my-agent --vendor   # vendor deps for offline use

# Build the agent's own binary → <folder>/bin/dar.
dar build --dir ./my-agent
dar build --dir ./my-agent --vendor --offline   # air-gapped build

# Refresh the per-agent Cargo.lock (deliberate dep bump; commit result).
dar lock-refresh --dir ./my-agent

# Offline self-rebuild: recompose, build, doctor-gate, and atomic swap.
# Does not restart a running agent.
dar self rebuild --dir ./my-agent
dar self rebuild --dir ./my-agent --vendor --offline

# Live self-rebuild: find a running agent by agent.yaml id, rebuild, and restart it.
dar self rebuild my-agent
dar self rebuild my-agent --workflow ./my-agent/workflows/release

# Scaffold the default WORKFLOW.md prompt in an agent folder.
dar init-workflow --dir ./my-agent
dar init-workflow --dir ./my-agent --force                      # overwrite existing
dar init-workflow --dir ./my-agent --linear-project-slug abc123 # seed Linear frontmatter
dar init-workflow --dir ./my-agent --expose-graphql-tool        # enable linear_graphql tool

# Validate agent.yaml. If the resolved WORKFLOW.md has a valid loop config,
# also validates the tracker. Exit code only.
dar doctor --dir ./my-agent

# Run the agent host (long-running). When the resolved WORKFLOW.md has a
# valid loop config, this runs the issue loop; otherwise it runs
# foreground/custom extensions only.
cd my-agent && dar run
dar run --dir ./my-agent          # or point at a folder

# Run a non-default workflow: one agent identity, several WORKFLOW.md hats.
# --workflow accepts a directory (its WORKFLOW.md is used) or an explicit
# .../WORKFLOW.md path; also accepted by doctor and export.
dar run --dir ./my-agent --workflow ./workflows/triage
dar run --dir ./my-agent --workflow ./workflows/triage/WORKFLOW.md

# Export the configured tracker's project and issues to data/.
dar export --dir ./my-agent
dar export --dir ./my-agent --workflow ./workflows/triage

# Quick start with the bundled example:
dar doctor --dir ./example-agent
dar run   --dir ./example-agent
open http://127.0.0.1:7878/
```

`run` loops until Ctrl-C or SIGTERM (children are killed on shutdown).
