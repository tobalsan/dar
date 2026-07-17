---
name: dar dashboard
description: The always-on operator console for a folder-scoped agent runtime, dense, dark, and quiet beside the terminal.
colors:
  void-black: "#111317"
  panel-charcoal: "#1b1f26"
  panel-slate: "#202630"
  chrome-black: "#15181d"
  row-charcoal: "#171b21"
  hairline-border: "#303844"
  fog-white: "#eceff3"
  muted-slate: "#9ba6b5"
  patina-teal: "#62b3b0"
  signal-green: "#57c785"
  signal-green-tint: "#57c78526"
  signal-amber: "#f2b84b"
  signal-amber-tint: "#f2b84b26"
  signal-red: "#f26d6d"
  signal-red-tint: "#f26d6d26"
  signal-blue: "#7aa2f7"
  signal-blue-tint: "#7aa2f726"
typography:
  title:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "1.05rem"
    fontWeight: 700
    lineHeight: 1.45
    letterSpacing: "normal"
  section-label:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: ".76rem"
    fontWeight: 700
    lineHeight: 1.45
    letterSpacing: ".08em"
  body:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "normal"
  meta:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: ".78rem"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "normal"
  pill:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: ".72rem"
    fontWeight: 700
    lineHeight: 1.45
    letterSpacing: "normal"
  micro-label:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: ".68rem"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: ".06em"
rounded:
  sm: "5px"
  md: "6px"
  lg: "8px"
  pill: "999px"
spacing:
  list-gap-sm: ".3rem"
  list-gap-md: ".45rem"
  panel-padding: ".9rem"
  page-padding: "1rem 1.25rem"
  cron-row-padding: "1.1rem 0"
components:
  button:
    backgroundColor: "{colors.panel-slate}"
    textColor: "{colors.fog-white}"
    rounded: "{rounded.md}"
    padding: ".45rem .7rem"
  button-danger-hover:
    textColor: "{colors.signal-red}"
    rounded: "{rounded.md}"
    padding: ".45rem .7rem"
  pill-ok:
    backgroundColor: "{colors.signal-green-tint}"
    textColor: "{colors.signal-green}"
    typography: "{typography.pill}"
    rounded: "{rounded.pill}"
    padding: ".12rem .45rem"
  pill-bad:
    backgroundColor: "{colors.signal-red-tint}"
    textColor: "{colors.signal-red}"
    typography: "{typography.pill}"
    rounded: "{rounded.pill}"
    padding: ".12rem .45rem"
  pill-warn:
    backgroundColor: "{colors.signal-amber-tint}"
    textColor: "{colors.signal-amber}"
    typography: "{typography.pill}"
    rounded: "{rounded.pill}"
    padding: ".12rem .45rem"
  pill-live:
    backgroundColor: "{colors.signal-blue-tint}"
    textColor: "{colors.signal-blue}"
    typography: "{typography.pill}"
    rounded: "{rounded.pill}"
    padding: ".12rem .45rem"
  pill-other:
    backgroundColor: "#9ba6b526"
    textColor: "{colors.muted-slate}"
    typography: "{typography.pill}"
    rounded: "{rounded.pill}"
    padding: ".12rem .45rem"
  run-row:
    backgroundColor: "{colors.row-charcoal}"
    rounded: "{rounded.md}"
    padding: ".6rem"
  panel:
    backgroundColor: "{colors.panel-charcoal}"
    rounded: "{rounded.lg}"
    padding: "{spacing.panel-padding}"
  dash-tab:
    backgroundColor: "{colors.panel-slate}"
    textColor: "{colors.muted-slate}"
    rounded: "6px 6px 0 0"
    padding: ".4rem .8rem"
  dash-tab-active:
    backgroundColor: "{colors.panel-charcoal}"
    textColor: "{colors.fog-white}"
    rounded: "6px 6px 0 0"
    padding: ".4rem .8rem"
  drawer:
    backgroundColor: "{colors.panel-charcoal}"
    padding: "1rem 1.1rem"
    width: "min(34rem, 100%)"
  cron-output-row:
    rounded: "{rounded.sm}"
    padding: ".35rem .5rem"
---

# Design System: dar dashboard

## 1. Overview

**Creative North Star: "The Operator's Console"**

The dar dashboard is a calm instrument panel beside the terminal: dense, legible, always-on. It is watched in the corner of a dark editor setup, not stared at, so it must earn its place without competing for attention. Every pixel answers one question first: is the agent healthy, and does anything need a human. Detail is one click away in the drawer; the resting state is a glance.

This system explicitly rejects enterprise SaaS admin templates: hero metrics, identical card grids, gradient accents. It rejects raw unstyled log dumps and walls of same-weight text; density here is structured, not undifferentiated. It rejects neon "hacker" aesthetics; the palette is muted and technical, never lit up. The dashboard must never glow brighter than the terminal it sits beside.

**Key Characteristics:**
- Flat surfaces, borders carry all depth, one deliberate shadow exception (the drawer).
- One monospace type family end to end, size and weight do the differentiating, not decoration.
- Patina Teal appears only on hover and active states; nothing is filled with it at rest.
- Status color is communicated as tinted pill backgrounds, never as page-wide alarm color.

## 2. Colors

The palette is a narrow stack of near-black surfaces with a single quiet accent and four functional status hues; nothing else is permitted to carry color.

### Primary
- **Patina Teal** (#62b3b0): the system's one accent, aged-copper calm, technical without neon. Used exclusively for hover/active borders (buttons, run rows, cron rows, dash tabs) and for the active tab's border. Never a fill, never body text at rest.

### Status
- **Signal Green** (#57c785): "ok" / "completed" state. Full-strength as pill text over a 15%-alpha tint of itself (`rgba(87,199,133,.15)`).
- **Signal Amber** (#f2b84b): "warn" / "interrupted" state, same tint-plus-full-strength-text pattern.
- **Signal Red** (#f26d6d): "bad" / "failed" state, and the danger button's hover color (text and border both shift to it, background stays neutral).
- **Signal Blue** (#7aa2f7): "info" / "live" state, used for the currently-running pill.

### Neutral
- **Near-Black Void** (#111317): page background, the darkest surface in the system.
- **Chrome Black** (#15181d): header bar and dashboard-tab strip, the chrome that frames content.
- **Panel Charcoal** (#1b1f26): panel background, the primary content surface, and the drawer.
- **Elevated Slate** (#202630): buttons and pills, one step lighter than a panel to read as interactive.
- **Row Charcoal** (#171b21): list-row background (run rows, event rows), one step darker than a panel to read as contained data.
- **Hairline Border** (#303844): the one border color in the system, at 1px, everywhere.
- **Fog White** (#eceff3): primary text.
- **Muted Slate** (#9ba6b5): secondary text, labels, meta lines, and the "other" status pill.

### Named Rules
**The Hover-Only Accent Rule.** Patina Teal never fills a surface and never sits at rest. It appears exclusively as a border color on `:hover`/active state and on the one active tab. If teal is visible anywhere without a cursor nearby or an active selection, it is a bug.

**The Tinted Pill Rule.** Every status color follows one formula and no other: a 15%-alpha tint of itself as background, the same color at full strength as text. No status color is ever used as a solid fill or as body text outside a pill.

## 3. Typography

**Body/Label/Display Font:** `ui-monospace, SFMono-Regular, Menlo, Consolas, monospace` (one stack, no fallback to a second family)

**Character:** A single monospace voice throughout, technical and even-tempered. Hierarchy comes from size, weight, letter-spacing, and case, never from switching typefaces.

### Hierarchy
- **Title** (700, 1.05rem, 1.45 line-height): page/drawer h1, one per view.
- **Section-label** (700, .76rem, 1.45 line-height, .08em letter-spacing, uppercase): panel headers like "Active runs", "Recent runs", drawer h2.
- **Body** (400, 14px, 1.45 line-height): the base readable size for row titles, prompts, and general content.
- **Meta** (400, .78rem, 1.45 line-height): secondary detail lines under a row title (workspace path, pid, age).
- **Pill** (700, .72rem, 1.45 line-height): status pill labels, always uppercase-by-content not by CSS transform.
- **Micro-label** (400, .68rem, 1.45 line-height, .06em letter-spacing, uppercase): kv-grid keys, "RECENT OUTPUTS" headers, the smallest legible unit in the system.

### Named Rules
**The One Font Rule.** There is exactly one font family in this system. A second family, serif or humanist sans for "warmth," is forbidden; monospace is the brand, not a technical default to escape.

## 4. Elevation

The system is flat at rest. Every surface separates from its neighbor with a 1px Hairline Border (#303844), not a shadow. There is exactly one exception: the run-detail drawer, which slides in over the page and needs to visually detach from it, so it alone carries a shadow.

### Shadow Vocabulary
- **Drawer shadow** (`box-shadow: -8px 0 24px rgba(0,0,0,.45)`): used only on `.drawer`, cast leftward since the drawer is fixed to the right edge. Signals "this is temporarily on top," nothing else.

### Named Rules
**The One Shadow Rule.** Exactly one shadow exists in the entire system, on the drawer, because it is the one surface that is genuinely on top of another. Every other panel, row, button, and tab is flat; depth there is drawn with borders, not shadows. Adding a second shadow anywhere else is prohibited.

## 5. Components

Every component is refined and restrained: quiet 1px borders, accent appears only on hover/active, no fills, no decoration beyond what a state requires.

### Buttons
- **Shape:** 6px radius (`{rounded.md}`), padding `.45rem .7rem`.
- **Primary:** Elevated Slate background (#202630), 1px Hairline Border, Fog White text.
- **Hover / Focus:** border shifts to Patina Teal (#62b3b0); no background or text-color change.
- **Danger variant:** identical at rest; on hover, both border and text shift to Signal Red (#f26d6d), background stays Elevated Slate. Used only for Stop.

### Status Pills
- **Style:** pill radius (999px), padding `.12rem .45rem`, `.72rem`/700-weight text (`{typography.pill}`).
- **State:** five variants, one per run/job status: completed (green), failed (red), interrupted (amber), live (blue), other (muted). Each follows the Tinted Pill Rule exactly.

### Run Row / Cron Output Row
- **Shape:** 6px radius for run rows (`{rounded.md}`), 5px for cron output and event rows (`{rounded.sm}`).
- **Background:** Row Charcoal (#171b21) for run rows and event rows; transparent at rest for cron output rows.
- **Border:** 1px Hairline Border at rest.
- **Hover:** clickable rows (`cursor: pointer`) shift the border to Patina Teal; nothing else moves.

### Panel
- **Corner Style:** 8px radius (`{rounded.lg}`).
- **Background:** Panel Charcoal (#1b1f26).
- **Shadow Strategy:** none; see Elevation.
- **Border:** 1px Hairline Border.
- **Internal Padding:** `.9rem`.
- **Header:** a section-label h2 ("Active runs", "Recent runs", "Cron jobs") always leads the panel.

### Dash Tabs
- **Style:** folder-tab shape, `6px 6px 0 0` radius, sitting on a Chrome Black (#15181d) strip with a Hairline Border bottom rule.
- **Default:** Elevated Slate background, Muted Slate text.
- **Hover:** text shifts to Fog White, border shifts to Patina Teal.
- **Active:** Panel Charcoal background, Fog White text, Patina Teal border; the only tab allowed to hold accent color at rest.

### Drawer
- **Position:** fixed to the right edge, `min(34rem, 100%)` wide, full viewport height.
- **Background:** Panel Charcoal, 1px Hairline Border on the left edge, the one shadow exception (see Elevation).
- **Header:** a section-label h2 plus a right-aligned close button.
- **Body:** a kv-grid (7rem micro-label key column, value column) for scalar fields, followed by an event list of Row-Charcoal rows.

### Kv Grid (signature component)
The drawer's scalar-field layout: a fixed 7rem key column in uppercase Micro-label style (Muted Slate, `.68rem`, `.06em` letter-spacing) beside a Fog White value column that wraps anywhere. Used for run metadata (status, workspace, pid) and job detail fields alike; it is the one place labels are always uppercase micro-text next to plain-case values.

## 6. Do's and Don'ts

### Do:
- **Do** keep the palette to Near-Black Void (#111317), Panel Charcoal (#1b1f26), Elevated Slate (#202630), Chrome Black (#15181d), and Row Charcoal (#171b21) as the only surface colors; nothing new joins this stack without a real new context.
- **Do** use exactly one 1px Hairline Border (#303844) for every seam, panel edge, button, row, and tab; that border, not a shadow, is what carries depth.
- **Do** hold Patina Teal (#62b3b0) to hover and active states only, per the Hover-Only Accent Rule.
- **Do** build every status indicator as a 15%-alpha tint background with full-strength text, per the Tinted Pill Rule, and only from the four signal colors (green, amber, red, blue).
- **Do** set every piece of text in the single monospace stack (`ui-monospace, SFMono-Regular, Menlo, Consolas, monospace`); differentiate hierarchy with size, weight, and letter-spacing only.
- **Do** use hex sRGB as the canonical color format (matching the project's own `:root` custom properties); OKLCH is prose-only reference in the sidecar.

### Don't:
- **Don't** build enterprise SaaS admin templates: hero metrics, identical card grids, gradient accents.
- **Don't** ship raw unstyled log dumps or walls of same-weight text; every block of text needs a hierarchy role (title, section-label, body, meta, pill, or micro-label).
- **Don't** reach for neon "hacker" aesthetics; every color in this system is deliberately muted, never saturated past what the status palette already defines.
- **Don't** use a side-stripe border: no colored `border-left`/`border-right` heavier than the standard 1px Hairline Border. Depth comes from the full-perimeter hairline, never a colored accent stripe.
- **Don't** use gradient text anywhere; text is always a single flat color from the palette.
- **Don't** nest cards inside cards; a panel holds rows and lists directly, never another bordered panel.
- **Don't** use pure #000 or pure #fff; the darkest surface is Near-Black Void (#111317) and the lightest text is Fog White (#eceff3).
- **Don't** introduce a second font family for "warmth" or emphasis; see the One Font Rule.
- **Don't** swap the whole `<body>` via htmx; the dashboard polls `GET /content` into the `#content` div with an innerHTML swap only. A `<body>` outerHTML swap breaks htmx bindings and blanks the page.
