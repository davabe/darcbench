# ADR-0013: Executing a discovered language runtime, under a path allow-list and a safe-path check

**Status:** Accepted · **Date:** 2026-08-10

## Context

`php.runtime` and `node.runtime` (Phase 3) measure how a machine runs PHP and
JavaScript. Unlike every module before them, that cannot be done without
executing a program the operator installed.

This collides with a mitigation the project has relied on since Phase 0.
`docs/THREAT-MODEL.md`, T-CONFIG:

> Detection is purely filesystem existence checks and `/proc` reads; discovered
> binaries are **never executed to ask for a version**.

And with the module contract in `crates/darcbench-modules/src/lib.rs`:

> a module never receives a command line and never constructs one … There is no
> code path from an HTTP request to a shell, which is what makes it safe to
> expose a start-a-run button to a browser at all.

Both are about a real threat and neither can simply be relaxed. The agent often
runs as root on a machine with other tenants on it, so *what* it executes and
*where that came from* is the whole security question.

There is also a measurement question underneath. `web.static` serves its objects
from an origin DARCBench starts, precisely so that two machines are compared
under the same server. PHP cannot work that way: DARCBench does not ship a PHP
interpreter and could not, and "how fast does this machine run PHP" is a question
about the PHP that is on it.

## Decision

**Execute the operator's runtime, under five constraints, and disclose exactly
what was executed.**

1. **A compile-time allow-list of absolute paths.** The same mechanism as the
   network endpoint table, for the same reason. `$PATH` is never consulted:
   it is environment, it is attacker-influenceable, and a benchmark that
   executes "whatever `php` resolves to" executes whatever the environment says.
2. **A safe-path check before execution.** The resolved binary and *every*
   ancestor directory must be owned by uid 0 and must not be group- or
   world-writable. This is the classic check, and it is the one that matters on
   the machines DARCBench targets: on shared hosting a compromised account that
   can write `/usr/local/bin/php` would otherwise get its code run by a root
   process. A path that fails is skipped, and the reason is reported.
3. **Fixed argv, no shell, no inherited environment.** The interpreter is
   invoked with a fixed argument vector; nothing from any caller reaches it. The
   environment is cleared rather than passed through, because PHP reads
   `PHP_INI_SCAN_DIR` and Node reads `NODE_OPTIONS` from it, and both turn an
   environment variable into code execution.
4. **The script is ours, written by us, read only by the interpreter.** It is
   emitted into the agent's own scratch directory under a compile-time name,
   `0600`, and passed by absolute path. No caller-supplied string enters it.
5. **A hard timeout, and every wait bounded.** The child is killed when it
   overruns; the wait for it to die is itself bounded, because `SIGKILL` cannot
   interrupt uninterruptible sleep and an unbounded `wait()` there would be the
   hang the timeout exists to prevent. Both output streams are drained on
   threads from the moment the child starts — a child that fills a 64 KiB pipe
   buffer with nobody reading blocks in `write()` and looks exactly like a
   timeout — and a reader still blocked because a *grandchild* inherited the
   pipe is abandoned rather than joined.

**The script is guarded as carefully as the binary.** A root interpreter pointed
at somebody else's script is root code execution just as surely as somebody
else's interpreter is. The scratch directory is refused if anyone but its owner
can write it, and the script is created `O_EXCL|O_NOFOLLOW` with its mode set at
creation — so a planted symlink is refused rather than followed, and the file is
never briefly world-readable between a write and a `chmod`.

**Disclosure is part of the deliverable, not a nicety.** The methodology already
requires it: *"PHP runs must disclose the runtime (native, container,
panel-managed, FPM, Apache module, LiteSpeed), OPcache state, worker count and
resource limits."* Every run records the interpreter path, its version, its
SAPI, whether OPcache is loaded and enabled, and its memory limit — because two
PHP results from differently configured interpreters are not comparable, and the
comparison must be refusable on evidence rather than on trust.

## Rationale

### Why not ship a runtime

Vendoring PHP would make results comparable and would measure the wrong thing.
An operator asking "how will my WordPress site run here" is asking about the PHP
their host installed, with the extensions and limits their host set. A score
produced by an interpreter nobody on that machine will ever use answers a
question nobody asked.

It is also not remotely affordable: PHP is a large C project with a long
dependency list, and the product ships as a single static binary.

### Why the allow-list, given that the operator controls the machine anyway

The adversary here is **a2 — a local unprivileged user on the host**, not the
operator. Shared and reseller hosting is the common case in this market, and
DARCBench frequently runs as root. The question is not whether the operator can
harm their own machine; it is whether *somebody else's account on it* can get
code executed by the root process the operator just started. The allow-list plus
the safe-path check is what makes the answer no.

### Why the environment is cleared rather than filtered

A filter is a list of things you thought of. `PHP_INI_SCAN_DIR`,
`PHP_INI_OVERRIDE`, `LD_PRELOAD` and `NODE_OPTIONS` are the ones that are
obvious today. Clearing is the only form that does not depend on the list being
complete.

### Why this does not reopen "no path from a request to a shell"

It does not: there is still no path from any caller-supplied string to an
executed program. The module chooses from a fixed table, the script is a
compile-time constant, and the argv is built from neither. What changes is that
the *set* of things the agent may execute grows from "nothing" to "one of a
handful of interpreters at root-owned paths". That is a real widening, which is
why it is written down here rather than absorbed silently.

## Consequences

- **A new threat entry**, T-EXEC in `docs/THREAT-MODEL.md`, and a clarification
  to T-CONFIG: discovery still never executes anything, and measurement now may.
- **Runtime modules are not in the `standard` profile.** Most machines have no
  PHP, and a standard run must not be downgraded to `Partial` for a machine that
  is not a PHP host. They belong to `web` and `deep`, which the operator selects
  because they want them.
- **Comparability is conditional on the runtime.** `php.runtime` results carry
  the interpreter version, SAPI and OPcache state in `comparability`, so the
  comparison layer can refuse rather than mislead.
- ADR-0006's isolation table gains its first real subprocess module, and this
  implements most but not all of the tier it described — *"fixed argv, no shell,
  rlimits, timeout, working directory under the state dir"*. Fixed argv, no
  shell and the timeout are there, and the working directory is set to `/`
  rather than the state directory, because nothing here should be able to reach
  a relative path at all and `/` is the one directory guaranteed to exist and be
  root-owned.

  **`rlimits` are not implemented.** Setting them on a child needs
  `Command::pre_exec`, which is `unsafe`, and `unsafe_code = "forbid"` is
  workspace-wide (ADR-0001). Doing it properly means either a vetted wrapper
  crate or the privileged helper already on the agent backlog. The gap is real:
  a PHP whose `memory_limit` is `-1` can be asked for more memory than the
  machine has, and the timeout here is wall-clock rather than a CPU or address
  space bound. It is recorded in `docs/BACKLOG.md` rather than left implied by
  a tier description this does not fully meet.
- **Container isolation is not used**, though ADR-0006 offers it. Running the
  operator's PHP inside a container would measure the container, and the whole
  point is to measure the interpreter as installed and configured.
