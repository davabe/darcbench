# ADR-0014 — Reverse-proxy integration writes an inert file, never a live one

**Status:** Accepted
**Phase:** 3
**Supersedes:** nothing
**Related:** [T-CONFIG](../THREAT-MODEL.md), [T-EXEC](../THREAT-MODEL.md),
[ADR-0013](0013-executing-a-discovered-runtime.md)

## Context

The dashboard binds loopback and prints an SSH port-forward hint. That is the
right default and most operators should stop there. Some cannot: a machine
behind a bastion, a team sharing access, an operator who wants
`https://host/darcbench/` behind the certificate their web server already has.

The roadmap's Phase 3 deliverable is *"optional reverse-proxy integration:
generate, preview, validate, back up, roll back"*, and T-CONFIG constrains it:

> Corrupting a Plesk vhost or restarting nginx on a live box is worse than any
> benchmark result is worth.

Every other part of DARCBench reads the operator's configuration. This is the
only part that writes near it, on a machine that is probably serving customers
right now.

## Decision

**DARCBench never writes a byte into a path the web server reads.**

`darcbench proxy apply` writes exactly one file, at a compile-time constant
path in the server's configuration *root* — `/etc/nginx/darcbench-location.conf`,
not `/etc/nginx/conf.d/`. Nothing scans that location. The file is inert: the
running server is unaffected when it appears, changes, or is deleted.

The single line that makes it live is an `include` the **operator** adds inside
the `server` block they choose. DARCBench does not write that line, does not
know which file it went into, and cannot remove it.

Around that, five rules:

1. **A file already at our path is moved aside, never overwritten**, and if the
   backup itself exists the operation is refused rather than destroying
   somebody's only copy. This is the only case where "back up" from the
   deliverable's wording has anything to back up — because nothing the operator
   wrote is ever touched.
2. **Never reload.** `apply` prints the reload command and does not run it. The
   operator knows whether now is a good moment to bounce their web server;
   this program does not.
3. **Validate with the server's own validator, or say plainly that it was
   not validated.** `apply` checks the snippet *in isolation*, by wrapping it
   in a minimal complete configuration in a temporary directory. `proxy verify`
   runs the live validator afterwards, once the operator's `include` exists —
   that is the check covering the whole result.
4. **Refuse a rollback that would break the server.** Removing the snippet
   while something still includes it guarantees an outage at the next reload,
   which may be days later, for an unrelated reason, by somebody who has never
   heard of DARCBench. The configuration tree is searched for references first
   and a live one stops the rollback, naming the file and line.
5. **Refuse on panel-managed hosts** — Plesk, cPanel, DirectAdmin. No
   `--force`. They generate the web server's configuration and rewrite it on
   their own schedule; they have their own reverse-proxy feature and it is the
   right tool.

## Why the inert path, and how we learned it

The first implementation wrote `/etc/nginx/conf.d/darcbench.conf`, which is the
obvious choice and is wrong: `conf.d` is included at nginx's **http** level, so
a bare `location` directive there is a syntax error. The very first run against
a real nginx produced

    [emerg] "location" directive is not allowed here in
            /etc/nginx/conf.d/darcbench.conf:7

The safety machinery worked — the file was removed and the server was left
exactly as it was — but the fix is not a better template. Serving a `location`
from the operator's existing TLS server block **requires** one line inside that
block; nginx offers no way around it. So the choice is:

| Option | Why not |
|---|---|
| Edit the operator's `server` block | The thing T-CONFIG forbids |
| Write a whole `server { listen 443; ... }` into `conf.d` | Claims a port and a hostname, racing whatever they already serve |
| Serve on a dedicated port instead | Loses the TLS termination that was the point |
| **Write an inert snippet; the operator adds one `include`** | **Chosen** |

The chosen option makes the dangerous edit small, visible, reviewable and
undoable *by the person who owns the file*. And it produces a stronger
invariant than the original design could: a program that never stages anything
live cannot stage anything broken.

One consequence falls straight out of it. Because the file is inert, "could not
be validated" stops being a reason to delete it — an unvalidated file nothing
reads is harmless. `apply` still removes a snippet its *isolated* check
rejects, for a different reason: handing an operator a broken fragment to
`include` is a trap, even if the fragment sits inert until they spring it.

## Validating a fragment in isolation

`nginx -t -c <wrapper> -p <tmpdir> -e <tmpdir>/error.log`, where the wrapper is

```nginx
events { worker_connections 64; }
http { server { listen 127.0.0.1:59999; include <snippet>; } }
```

`worker_connections` must exceed the listening-socket count or nginx refuses
the config for a reason unrelated to the snippet. Nothing binds: `-t` parses
and exits. The wrapper's temporary path is scrubbed out of any message before
it reaches the operator, because a path that no longer exists is noise in an
error they have to act on.

Apache is reported as **unvalidated** rather than checked this way. `httpd -f`
needs a `ServerRoot`, a module set and load paths that vary per distribution,
and a wrapper assembled from guesses would fail for reasons having nothing to
do with the snippet — reporting a good file as broken. Saying "not checked" is
true; `proxy verify` covers it once the include is in place.

## Executing the validator

A configuration DARCBench believes is fine is worth nothing. `nginx -t` is the
only opinion that matters, and getting it means executing a binary the operator
installed — [T-EXEC](../THREAT-MODEL.md).

So it goes through `darcbench-modules::runtime_exec`, the layer
[ADR-0013](0013-executing-a-discovered-runtime.md) built for `php.runtime` and
`node.runtime`: a compile-time path allow-list, an ownership check on the
binary *and every ancestor directory*, a fixed argument vector, a cleared
environment and a hard timeout. The reasoning transfers exactly — this program
often runs as root on a shared host, and executing "whatever `nginx` resolves
to on `PATH`" hands root to whoever last wrote to a directory on it.

The argument vector is assembled in `proxy.rs` from constants and paths this
module created. Nothing an operator types reaches it.

## The one piece of caller input

`--location` is the only user-supplied value that reaches the generated file,
and its check is not cosmetic. An nginx `location` is terminated by `}`, so a
prefix containing one would close the block and everything after it would be
top-level directives of the caller's choosing — in a file written as root and
executed by the web server. The grammar is therefore an allow-list of
characters that cannot terminate anything (`[A-Za-z0-9/._~-]`), not a blocklist
of the ones somebody thought of.

## What the generated configuration contains

Two directives an operator writing this by hand would very likely omit:

- **`proxy_buffering off`.** The dashboard's live progress is a Server-Sent
  Events stream. A buffering proxy holds it until the buffer fills, so a
  running benchmark appears frozen and then jumps — which reads as the agent
  being broken.
- **`proxy_set_header X-Forwarded-Proto $scheme`.** The session cookie is
  marked `Secure` when, and only when, the browser reached the agent over TLS,
  and the agent always speaks plain HTTP so it cannot tell on its own. From
  `$scheme`, never hardcoded: hardcoding `https` on a plain-HTTP site makes the
  browser discard the cookie, and the event stream then has no way to
  authenticate.

## Consequences

**Good.** No DARCBench command can break a running web server. The dangerous
edit is one line, made and owned by the operator. Rollback is a deletion, and
it refuses to happen while it would be destructive. The validator runs under
the same constraints as every other binary this program executes.

**Bad.** It is not one command. The operator must add a line by hand, and a
setup guide that says "then edit your vhost" is a worse product experience than
one that says "done". That cost is accepted: the alternative is a program that
edits production web server configurations, and no benchmark result is worth
it.

**Accepted residual risk.** The dashboard becomes reachable on whatever
hostnames that server block answers, protected only by its token — and the
token appears in the web server's access log the first time a browser follows
the bootstrap URL (T-TOKEN-URL). `apply` says so in as many words and tells the
operator to put the prefix behind whatever authentication their other admin
paths use.
