# Product

## Register

product

## Users

Developers running fleets of folder-scoped AI agents on their own machines.
They glance at the dar dashboard (per-agent, port 7878, or the unified
`dar dash` fleet view) to answer: is my agent healthy, what ran, what did it
produce, does anything need me. Sessions are short and observational; the
terminal is their home, the dashboard is the ambient monitor beside it.

## Product Purpose

Dar is a self-contained agent runtime: one binary, one folder, a loop that
dispatches AI coding agents against an issue tracker plus scheduled cron jobs.
The dashboard exists to make that loop observable and lightly controllable
(pause/resume/stop, run-now) without ever competing with the issue files and
config as the source of truth. Success: a user reads agent state in seconds
and trusts what they see.

## Brand Personality

Refined, technical, calm. Closer to Linear/Vercel dashboards than to raw
terminal dumps: softer surfaces, deliberate whitespace, restrained color, but
still dark, dense, and monospace-flavored — a tool built by an operator for
operators.

## Anti-references

- Enterprise SaaS admin templates: hero metrics, identical card grids,
  gradient accents.
- Raw unstyled log dumps; walls of same-weight text.
- Neon "hacker" aesthetics.

## Design Principles

- Answer first: the state of the world (running, ok, failed, next fire) reads
  in one glance before any detail.
- Files are the truth: the UI renders and links real files (issues, outputs);
  it never pretends to own state it doesn't.
- Controls are few and honest: only actions the runtime actually supports,
  wired to the real API, reflecting back real results.
- Density with hierarchy: small type is fine; flat hierarchy is not.
- One system: every tab (Runs, Cron, future extensions) reads as the same
  product, sharing tokens and rhythm.

## Accessibility & Inclusion

No formal WCAG target mandated; keep sensible contrast on the dark theme,
visible focus states, and honor prefers-reduced-motion for any animation.
