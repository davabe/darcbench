# ADR-0017 — The client UI is a separate process, and it is silent while measuring

**Status:** Accepted
**Date:** 2026-08-29
**Phase:** 3 (client line)
**Supersedes:** nothing for the client line
**Related:** [ADR-0003](0003-local-web-architecture.md), [ADR-0004](0004-realtime-transport.md),
[ADR-0015](0015-two-product-lines-one-engine.md)

## Context

The client product competes with 3DMark and Geekbench, where presentation is
part of the value. It needs a polished native application: animated progress,
live telemetry, results worth reading. [COMPETITIVE-ANALYSIS](../COMPETITIVE-ANALYSIS.md)
already records the lesson — *"a benchmark people enjoy reading gets run."*

There is a conflict hiding in that requirement. **The pretty UI is load on the
machine under test.** A webview compositing at 60fps during `float.matmul` is a
second process competing for cache, memory bandwidth and scheduler time. This
agent already aborts a run when it detects external CPU load. So an animated UI
either trips our own watchdog, or — worse — does not trip it and quietly inflates
or deflates the numbers.

For a product whose entire claim is that it refuses to publish a number it
cannot defend, this is not a polish question. It is a measurement-integrity
question, and it decides the application architecture.

The server line's answers do not carry over. [ADR-0004](0004-realtime-transport.md)
chose SSE over HTTP because the traffic must survive middleboxes and reach a
browser through whatever proxy already fronts the server. Nobody proxies to their
own laptop.

## Decision

**The shell and the measurement engine are separate processes. The shell is
quiescent while the engine measures.**

```
darcbench.exe                     shell: native window, animated, results and history
  |                               -> hidden and rAF-stopped before each measurement phase
  +-- spawn -> darcbench.exe --engine
                                  engine: no chrome, clean environment, own priority class
                                  GPU tests: winit window + native swapchain, fullscreen
```

1. **One binary, two modes.** The shell re-spawns itself with `--engine`. One
   file to sign, one installer, one version — but a genuinely separate process
   with an environment the shell does not contaminate.

2. **The shell goes quiet.** Before a CPU, memory or storage phase: window
   hidden, `requestAnimationFrame` stopped, progress updates throttled and
   rendered without transitions. Animation resumes on results screens, where it
   costs nothing.

3. **The GPU test is the animation.** A fullscreen render with a native
   swapchain *is* the workload. The spectacle and the measurement are the same
   object, so it contaminates nothing — this is 3DMark's trick and it is free.
   It lives in the engine process, not the shell.

4. **Transport: the event protocol, not the HTTP stack.**
   `darcbench-protocol::events` with its monotonic `seq` is reused verbatim and
   serialises over stdio (NDJSON) or a named pipe. No listening socket, no
   token, no chance of a firewall dialog greeting a consumer on first run.
   ADR-0004's reasoning is retained for the server line and dropped for the
   client line, because its premises are server premises.

5. **Shell technology: Tauri 2**, with the constraints above binding. Rust
   backend links the engine crates directly; the existing React dashboard is
   reused; WebView2 and WKWebView mean no bundled browser engine; native
   installers, Authenticode signing and macOS notarisation are supported paths.

### The architecture polices itself

The runtime load ceiling computes external load as total busy time **minus this
process's own**. With the shell in a separate process, the shell's CPU time is
counted as external load and trips the guard. A UI that misbehaves during a
measurement is caught by the benchmark it is displaying. That property only
exists because the processes are split, and it is a reason to split them
independent of the ones above.

## Alternatives

**One process, UI and engine together.** Rejected. It makes the contamination
undetectable by construction: work done by the rendering thread is charged to the
process being excluded from the external-load figure, so the guard cannot see the
thing it exists to catch.

**Electron.** Rejected. A Node runtime resident beside the measurement is the
opposite of the requirement, and the binary size is indefensible for a tool
whose server sibling ships as a single static musl binary.

**Slint, or another pure-Rust native toolkit.** Not rejected — the strongest
alternative if the WebView2 dependency proves objectionable, and it removes a
whole process class from the machine under test. Revisit if the suspended
webview turns out to be a measurable neighbour rather than a theoretical one.
That is a measurement, not an opinion, and it should be taken.

**Native Win32/WinUI 3.** Rejected on cost. Shells are per-platform under
[ADR-0015](0015-two-product-lines-one-engine.md) so it would not violate the
architecture, but the interop cost from Rust buys little over a native-framed
webview.

## Consequences

- The engine must be fully usable headless, driven by flags and emitting events
  on stdout. That is a requirement, not a side effect, and it is also what makes
  calibration and CI possible.
- The engine/shell contract has to be fixed early, before the shell exists, or
  the shell's needs leak back into the engine as ad-hoc surface.
- Two processes means crash handling, orphan cleanup and cancellation
  propagation across a process boundary. Cancellation already exists in the
  agent and has to be reworked to cross it.
- The client line will not reuse `darcbench-agent`'s HTTP server, token auth or
  SSE stack. Those stay server-line assets.

## Revisit if

A suspended WebView2 measurably perturbs the shared kernels on the anchor host,
in which case the shell moves to a pure-Rust toolkit and this ADR is superseded
rather than amended.
