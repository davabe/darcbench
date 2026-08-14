# UX design

## Identity

```
DARC//BENCH
Deployment · Application · Runtime · Compute
```

Dark, precise, high-contrast. A measuring instrument, not a gaming product.

| Token | Value | Use |
|---|---|---|
| `--bg` | `#070a12` | Page |
| `--panel` | `#0e1422` | Panels |
| `--line` | `#1d2740` | Borders |
| `--fg` | `#e8eef8` | Text |
| `--dim` | `#93a1bd` | Secondary text |
| `--cyan` | `#22e0ff` | Primary accent, values |
| `--blue` → `--violet` | `#3b7dff` → `#9b6bff` | Brand gradient, primary action |
| `--ok` / `--warn` / `--bad` | `#3ddc97` / `#ffb547` / `#ff5c73` | State — **always paired with a word** |

The brand gradient appears exactly twice: the wordmark and the primary button.
Restraint is what makes it read as instrumentation.

## Views

| View | Status | Purpose |
|---|---|---|
| Welcome / safety | ✅ | What this is; a bare `darcbench` never starts a benchmark |
| This machine | ✅ | Inventory: CPU, topology, memory, scope, kernel, governor |
| Run control | ✅ | Profile selection, start, cancel, warning acknowledgement, progress |
| Preflight | ✅ | Risk class, plain-language explanation, findings with severity |
| Live telemetry | ✅ | CPU busy/steal, load, memory, clock, temperature, sparklines |
| Scores | ✅ | Total, categories, facets, provisional/final |
| Raw measurements | ✅ | Median, unit, n, CV, 95% CI per metric |
| Event log | ✅ | Sequence-numbered stream |
| Artifacts | ✅ | HTML report, JSON bundle, bundle digest |
| Comparison, history, share page, settings | ⏳ Phase 2–6 | |

## Principles

**Show uncertainty, not just a number.** Every metric row carries n, CV and a
confidence interval. High CV is highlighted. The total score sits next to its
result state, so `280 · Partial` reads as one fact rather than a score with a
footnote.

**Never let the UI lie by omission.** The uncalibrated banner is the first thing
on the page and cannot be dismissed. Warm-up samples are streamed but never
charted as results. A profile with no implemented modules is shown disabled and
labelled "not implemented yet" rather than hidden.

**The observer must not perturb the measurement.** Sample events are coalesced
into a per-metric latest value; telemetry is a bounded 180-point ring buffer;
the log is capped at 300 lines. A deep run emits thousands of events, and a
dashboard that re-renders for each one becomes a measurable load on the machine
it is measuring.

**No decorative animation.** Sparklines are static SVG paths redrawn at 1 Hz.
There is no spinner competing for the CPU under test.

## Accessibility

- **No meaning by colour alone.** Every state carries a word: risk class, result
  state, run state, finding severity. Colour is redundant encoding.
- **Semantic structure.** One `<h1>`, `<section aria-labelledby>` per panel,
  `<dl>` for key-value data, real `<table>` with `<th scope="col">`.
- **Live regions.** `aria-live="polite"` on scores, status and the log;
  `aria-live="off"` on telemetry, which updates every second and would otherwise
  flood a screen reader.
- **Sparklines are `aria-hidden`** and always accompanied by the numeric value.
- **Keyboard.** Native controls throughout; visible `:focus-visible` outline at
  2px cyan.
- **`prefers-reduced-motion`** and **`prefers-contrast: more`** both honoured.
- **Contrast.** Body text `#e8eef8` on `#070a12` is roughly 16:1; the dimmest
  text is above 4.5:1 and lifts further under high contrast.

## Responsive and remote

Fluid grids with `minmax()`; tables scroll horizontally inside their own
container so the page body never does; the layout collapses cleanly at 640px.

For slow links: the bundle is ~220 KB, there are no webfonts, no images and no
external requests at all. The SSE stream is the only ongoing traffic, and it
reconnects with replay rather than restarting.

## The fallback console

When the React bundle was not built, the agent serves a small vanilla console.
It is a *functional fallback*, not a second dashboard: start, watch, cancel,
read results. It builds DOM with `textContent` rather than HTML strings, and
loads its script from its own route so CSP needs no `unsafe-inline`.

## Reports

The HTML report is fully self-contained — no external reference of any kind,
because reports are opened offline and a report that phones home leaks the fact
that someone read it. Print styles switch to a light theme.

## Copy

Plain, specific, never reassuring about something we do not know.

> **Provisional scoring model.** `dbs/0.1.0-dev` has not been calibrated against
> a physical DARC-REF-1 reference machine — its reference values are declared
> targets, not measurements. The raw measurements below are real and
> reproducible; the scores derived from them are development output.

> **Heavy load** · about 16s
> Saturates the CPU for the duration of the run. Nothing is written to disk.

Not "Something went wrong" but "Another benchmark run is already in progress.
Two concurrent runs would measure each other."
