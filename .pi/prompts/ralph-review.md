---
description: Study the codebase, plan a multi-pass review, then run it in fresh Ralph sessions
argument-hint: "[scope, comparison range, or review goals]"
---
Perform a thorough, evidence-based code review and improvement pass on this repository using a fresh-context Ralph Wiggum loop.

Optional review context from the user:

`${ARGUMENTS:-No extra scope was supplied. Review the current codebase, prioritizing recently changed and architecturally important code.}`

Before starting the loop:

1. Read and follow these skills completely:
   - `/improve-codebase-architecture`
   - `/refactor`
   - `/simplify-code`
   - `/pi-ralph-wiggum`
2. Study the repository and its instructions, architecture docs, current git state, relevant history/diffs, tests, and major ownership boundaries. If a comparison range was supplied, inspect that range explicitly. Do not prescribe fixes before tracing the relevant code paths and gathering direct evidence.
3. Produce a prioritized findings inventory. Separate confirmed facts, inferences, and unknowns. Reject speculative or cosmetic work that does not materially improve correctness, architecture, maintainability, testability, or AI navigability.
4. Convert accepted findings into a durable, ordered Ralph checklist. Each item must be small enough for one fresh session, identify its evidence and affected area, state concrete acceptance criteria, and name the smallest relevant verification. Preserve behavior unless a confirmed bug requires a behavior change.
5. Include final checklist items for cross-cutting review, formatting/linting, and the full repository test suite. Require `CHANGELOG.md` updates for user-visible changes. Do not commit unless explicitly requested.

Then call `ralph_start` with a descriptive loop name and the complete task file. Configure the loop so every iteration:

- reads repository instructions and the durable Ralph files;
- handles exactly one checklist item, or one tightly coupled bounded batch;
- applies the three review skills where relevant;
- makes surgical changes only, with tests for bugs or behavior changes;
- records evidence, decisions, commands, outcomes, and remaining risks in the durable task/reflection files;
- runs the listed focused verification before marking an item complete;
- never skips or suppresses failing tests, lint, or type checks;
- revises or removes a planned change if deeper inspection shows it is unjustified;
- emits `<promise>COMPLETE</promise>` only after every checklist item and final verification pass succeeds.

Use enough iterations for the findings inventory; do not collapse the work into a single session. After starting the loop, report only the loop name, where its durable files live, and its current status.
