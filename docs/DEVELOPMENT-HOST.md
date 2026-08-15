# Development host

What a machine needs in order to move DARCBench forward, what is blocked
without one, and exactly what to do first when one is available.

This document existed because a large amount of Phase 4 was written, tested and
**unable to run**, all for the same reason: it needed a container daemon and
access to a registry, and the machine it was written on had neither.

**That block is cleared.** On 2026-08-14 the work moved to a host with Docker
29.1.3 and registry access. The digests are pinned, `Sandbox::launch` has run,
all three container modules are registered and scored, and the seven defects
that first contact found are fixed — see [ROADMAP.md](ROADMAP.md), *What the
first real daemon changed*.

**Phase 4 is complete.** Every deliverable in it now runs, is registered and is
scored. What this document is for from here is adding the *next* image or the
next container-based module, and the reasoning in §3 is what governs that.

---

## 0. Read this before running anything

**Do not use a machine that is serving anything you care about.**

This is not boilerplate caution. This repository's own threat model
([THREAT-MODEL.md](THREAT-MODEL.md)) is built around the fact that DARCBench is
installed on machines that are already doing something, and the code is careful
about it — but a *development* host is where things get run half-finished and
by hand. Three specific hazards:

| Command | What it does to the host |
|---|---|
| `darcbench proxy apply` | Writes into `/etc/nginx` or `/etc/apache2`. Inert by design (see [ADR-0014](adr/0014-reverse-proxy-integration.md)), but it is still a file in your web server's config root |
| `deployment.container` | Writes ~70 MB into the container daemon's storage on a host filesystem. It cannot use a tmpfs — the storage driver is daemon-wide |
| `database.*`, `web-target` | Open listening sockets and start containers |

A throwaway VPS is right. A VPS you use for anything else is not.

**Give the machine a real container runtime and let it reach a registry.** That
single fact is what unblocks almost everything below.

---

## 1. What the host needs

| Requirement | Why | How to check |
|---|---|---|
| Linux, x86-64 or aarch64 | Everything is Linux-specific: `/proc`, `/sys`, cgroups | `uname -a` |
| Rust with `rustfmt` and `clippy`. MSRV is **1.82** (`rust-version` in the workspace manifest); everything here was developed and gated on **1.97** | The gate in §5 | `cargo --version` |
| **Docker or Podman, with a reachable daemon** | Every Phase 4 module | `docker info` — must print a `Server:` block |
| **Outbound access to a container registry** | Resolving image digests | `docker pull hello-world` |
| ≥ 4 GB RAM, ≥ 20 GB free disk | Sandboxed databases, build layers | `free -g`, `df -h` |
| `cargo-deny` | Dependency policy gate | `cargo install cargo-deny` |
| Root, or a user in the `docker` group | Container control | |
| Optional: nginx | Exercising `darcbench proxy` end to end | |
| Optional: PHP, Node at root-owned paths | `php.runtime`, `node.runtime` | |

**Deliberately not required:** a C toolchain. This workspace is pure Rust apart
from bundled SQLite, and it stays that way.

### The trap the first host fell into

The host the container tier was written on had `/usr/bin/docker` — the
**client** — and no daemon. `docker version` answers happily from the client
binary alone. That is why `Runtime::discover` probes with `docker info` and
treats "no runtime found" and "runtime found, daemon silent" as two different
failures: an operator installs the first and starts the second.

If you see this, the host is not ready:

```
/usr/bin/docker is installed but its daemon did not answer: [...]
The module is reported as not measured; nothing on this host was used instead.
```

---

## 2. Where the work stands

Phases 0–3 are complete. Phase 2 has one open item — DARC-REF-1 calibration —
which needs three physical machines and is not what this document is about.

Phase 4:

| Deliverable | State | Blocked on |
|---|---|---|
| Container isolation tier | ✅ **Launch, readiness, exec, `Drop` and reaping all exercised against a real daemon.** Orphan reaping verified by killing a run mid-phase | — |
| `database.oltp` | ✅ **Registered, anchored, manifested.** Six metrics on real hardware | — |
| `database.cache` | ✅ **Registered, anchored, manifested.** Six metrics on real hardware | — |
| WordPress fixture generator | ✅ Complete and verified. Pure, deterministic, checksum-pinned | — |
| `wordpress.site` | ✅ **Registered, anchored, manifested.** Four metrics against a pinned WordPress + MariaDB stack, with cache disclosure | — |
| `deployment.container` | ✅ **Registered, anchored, manifested.** Seven metrics, including startup and health against a pinned BusyBox | — |

Read [ROADMAP.md](ROADMAP.md) Phase 4 for the reasoning behind each. Everything
marked "not delivered" is declared in the relevant module's `limitations`, not
left to be discovered.

---

## 3. What to do first, in order

Steps 1 to 3 are **done**, on 2026-08-14. They are kept below in outline
because the reasoning still governs anything added to the allow-list, and
because the reader of this document may be adding a third image.

### Step 1 — Resolve the digests · ✅ done

```bash
docker pull postgres:17-bookworm
docker image inspect postgres:17-bookworm --format '{{index .RepoDigests 0}}'
```

Both entries in `crates/darcbench-modules/src/container.rs` are now
`Pin::Pinned`, resolved against Docker Hub on 2026-08-14 with the date recorded
in the code beside each. A digest is a fact about a moment; six months later
somebody needs to know which moment.

Two tests guard the table and one more was added:
`every_allowed_image_is_pinned_by_digest` refuses a tag alongside a digest,
`a_pending_entry_names_a_tag_to_resolve_and_a_reason` covers whatever is left
pending, and `every_allowed_image_runs_as_a_service_account` refuses an entry
that would run as root.

### Step 2 — Run the containers · ✅ done, and it found seven defects

This was the step nobody had done, and the advice above it was right: **treat a
clean first run as suspicious rather than as success.** It was not clean.

The five are written up in [ROADMAP.md](ROADMAP.md) under *What the first real
daemon changed*, and they are worth reading before adding an image, because
three of them are properties of the tier rather than of PostgreSQL:

1. `--cap-drop ALL` stops an entrypoint that drops privileges itself. Fixed by
   running as the image's service account, not by adding capabilities back.
2. `--rm` deletes a failed container's log before anything reads it.
3. A TCP connect from the host is not a readiness signal — Docker's userland
   proxy answers it as soon as the container exists.
4. The memory limit and the tmpfs are **one budget**: tmpfs pages are charged
   to the container's cgroup.
5. Tool output differs from tool documentation (`--progress 0`,
   `redis-cli --latency` to a pipe).

A sixth appeared only once the modules were registered and driven by
`darcbench run`: the runtime load ceiling counted the module's own container as
a competing tenant and degraded it on an idle machine. A container is not this
process's child at any remove, so its CPU can never be subtracted. Any module
whose work runs outside this process must return `true` from
`workload_runs_outside_this_process`.

A seventh appeared only after deleting an image that had been pulled by hand.
`docker run` fetches an absent image, so all three modules were doing an
undeclared 156 MB / 17 MB / 1 MB transfer *inside* a measurement -
`deployment.container`'s startup metric came back at 147% variance because one
repetition of seven included a download. `Runtime::ensure_image_present` now
does it explicitly and untimed, and each allow-list entry declares what it
costs.

**That last one is the one to internalise.** The first six came from running
code that had never run. The seventh came from running code that already
worked, on a host deliberately put back into the state a new machine would be
in. Delete the images before you believe a container module.

The by-hand checks in this section were all performed and all pass:

- `docker ps -a` is empty after a run; `Drop` removed the container.
- `docker ps -a --filter label=com.getdarc.darcbench.owned=1` finds nothing.
- A run killed with `SIGKILL` mid-phase leaves its container, and the next run
  reaps it and discloses that it did —
  `containers_reaped_from_earlier_runs = 1` in the bundle context.
- The published port is on `127.0.0.1`, confirmed with `docker port`.

`crates/darcbench-modules/examples/probe.rs` drives one module end to end and
is how all of the above was done. It is still there and still useful for a
module that is not registered yet, which is what `wordpress.*` will be.

### Step 3 — Register the modules · ✅ done

Both are in `registry::builtin`, with scoring anchors in
`crates/darcbench-scoring/src/reference.rs`, manifests at
`benchmarks/database/database.{oltp,cache}.json`, and membership in **`deep`
only** — the same argument as `php.runtime`: most machines have no container
runtime, and a standard run coming back `Partial` on every machine that is not
a container host would report the profile's own assumptions as a fault of the
machine. They run last within `deep`, after `web.static`, because their failure
depends on a daemon rather than on this process.


### Step 4 — `wordpress.*` · ✅ done

Delivered as `wordpress.site`. Four things it settled that the next
container-based module will meet too:

**Two containers need a network, and it must not be `--internal`.** An internal
network has no port publishing, so the web server is unreachable from the
process measuring it. Block outbound at the application instead - WordPress has
`WP_HTTP_BLOCK_EXTERNAL`.

**A port below 1024 without root is a sysctl, not a capability.**
`net.ipv4.ip_unprivileged_port_start=0` is namespaced and grants nothing.

**Two containers sharing files needs `--volumes-from`, not a tmpfs.** A tmpfs is
visible to exactly one container. The WordPress entry is the only one in the
allow-list with `data_on_tmpfs: false` for that reason, and it discloses that
its files are in the daemon's storage.

**Getting data into a container is a pipe.** `runtime_exec::run_with_stdin` and
`Ephemeral::stdin` exist so a 1.6 MB fixture reaches WP-CLI without a host path
ever entering an argument vector. `docker cp` would have been the obvious route
and is strictly worse.

`wp import` was **not** used, and the reasoning is in `Fixture::to_php_import`:
the WordPress Importer is a plugin that would have to be downloaded from
wordpress.org at run time.

Both things the roadmap insisted on were done and both mattered:

- **Cache disclosure.** No page cache and no object cache are installed, the
  bundle says so, and `origin.cold`/`origin.warm` are explicitly named as *not*
  a cached-versus-uncached pair - because that is the obvious misreading and it
  would be off by two orders of magnitude.
- **The installation is verified before anything is timed**, by evidence rather
  than an exit code: the homepage has to be a page, of plausible size,
  containing a title the fixture generated.

**Two defects it found in itself, both by being run rather than read:**

`php_version` and `opcache` were asked of the WP-CLI container - a *different
image* - so PHP was reported as 8.3.33 where Apache ran 8.3.31, and opcache as
`disabled` because `opcache.enable_cli` is 0 while `opcache.enable` is 1. Both
are comparability keys. **Ask the container that served the request.**

`origin.warm` was measured immediately after the deliberate cold start and was
not warm: 94 ms at 64% variation, against a heavier page seconds later at 43 ms
and 5.5%. One warm-up pass over every path before timing any of them fixed it
to 42 ms at 10.7%. **Warm everything, then time everything.**

### Step 5 — `deployment.container`'s missing half · ✅ done

`startup.cold` and `health.to_serving` now run against a pinned BusyBox. The
build did **not** change and must not: see the module docs for why it stays
`FROM scratch` even now that a base image is available.

---

## 4. Things that will look broken and are the host

Ordered by how much time they will waste before you work it out.

| Symptom | Cause | Not a defect because |
|---|---|---|
| `nginx -t` fails with `socket() [::]:80 failed (97: Address family not supported)` | No IPv6 in the container/VPS | The config parsed; the bind failed |
| `node.runtime` reports every candidate rejected | Node installed via nvm/fnm/volta, under `$HOME`, user-owned | The safe-path check is working. Install a distro Node |
| `php.runtime` reports "not found" | No PHP at an allow-listed path | The allow-list is compile-time by design (ADR-0013) |
| `web.static` latency phase reports a low offered share | A local injector cannot offer 70% of a machine's capacity | Documented; the share offered is published. Use `web-target`/`web-drive` |
| `database.*` reports "not measured" | No daemon, or an unpinned image | Both are refusals by design; read the message, it says which |
| `database.oltp` prints `skipping` or withholds a phase | The server was killed, or a phase lost clients | Read the warning; it now says which. Silence here was a defect and is gone |
| `database.oltp` throughput varies by 2x between runs on the same machine | Two vCPUs running eight clients, eight pgbench threads and a server | Each metric is one phase and one figure, so there is no CV to flag it. Declared in the module's `limitations` |
| `cpu.mixed` prints `skipping the speedup assertion` | Either the machine is too noisy to reproduce itself, or it never got a second core | Both are measured preconditions, not assumptions. See below |
| `e2e.sh` fails only `doctor --json exits 0`, and passes on a re-run | The machine was still loaded from the release build the gate just did | `doctor` is right: it raises a load warning, and preflight does not pass a warning at `ProductionRisk` without `--force`. Run the gate's steps with a gap, or accept that the first e2e after a build may see the machine it just used. Observed 1 failure in 3 consecutive runs on 2 vCPUs |

### The one that was a defect

`multi_thread_shape_reports_more_total_throughput` used to be the entry above
that read *"tests flake under `-j` on a small VPS — re-run pinned to fewer
cores before concluding anything."* On a two-vCPU host that advice did not
work: the test failed under `--test-threads=2` and passed only in complete
isolation, and it passed and failed on identical code minutes apart.

It was a real defect in the test rather than a property of the host. The test
already refused to conclude anything when the machine was too *noisy* — but
noise is a shape that cannot reproduce itself, and what actually happened was
a shape that reproduced perfectly well while being handed one core, because
another test had the other one. No amount of averaging separates "the threaded
shape stopped being parallel", which is the defect worth failing on, from "the
machine had no second core to give it".

So the precondition is now measured: CPU seconds burned over wall seconds
elapsed, from `/proc/self/stat`, which is how many cores the process really got.
Below 1.5 the test says so and asserts only that throughput is positive and
finite. **A test that cannot pass on a small host teaches people to ignore the
suite**, which costs more than the coverage is worth.

## 5. The gate

Nothing gets committed unless all of this passes. It is not optional and it is
not slow.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/e2e.sh
./scripts/check-manifests.sh
cargo deny check licenses bans sources
```

At the time of writing: 593 tests across six crates, e2e 45/45, 9 manifests,
`cargo deny` clean on bans, licenses and sources.

On a two-vCPU host `cargo test --workspace` takes about four minutes and the
release build rather longer. Both are within the "not slow" claim above on a
machine with more cores; neither is a reason to skip a step.

`clippy -D warnings` is load-bearing rather than decorative. `unwrap_used`,
`expect_used` and `panic_in_result_fn` are denied outside tests, and
`unsafe_code` is **forbidden** workspace-wide. If you find yourself wanting
`unsafe`, the answer in this codebase has always been a safe wrapper crate
(`rustix`) or a different design.

---

## 6. Conventions worth knowing before you write anything

**Branch.** Work on the branch you were given. Never push
elsewhere without being asked. Never open a pull request unless asked.

**Commit messages carry the reasoning.** They are long here on purpose, and the
format is: what changed, *why the obvious alternative was rejected*, and what
is knowingly not done. A reader six months out should be able to reconstruct
the decision without the conversation that produced it.

**A claim in a commit message is not a property.**
`the_script_refuses_a_planted_symlink_and_is_never_briefly_readable`, in
`crates/darcbench-modules/src/runtime_exec.rs`, carries this comment:

> This test exists because the fix for it was written, described in a commit
> message, and silently not applied - a string replacement missed and nothing
> failed. A property that only a commit message asserts is not a property.

That happened. Commit `b04f0e0` claimed to close a root-code-execution path and
did not; it was caught two commits later by accident. **Verify every edit
landed.** If you script an edit, assert the replacement count.

**Comments explain why, never what.** The codebase reads like an argument. A
constant without a justification is a constant somebody will change casually.

**Declare what you did not do.** Every module has a `limitations` list, and
several of them say a deliverable's named feature is absent and why. That is
the house style, not an apology.

**Deterministic corpora are checksum-pinned.** If you touch
`wordpress_fixture`, the checksum test fails with instructions: bump
`FIXTURE_VERSION` *first*, then update the hash. In that order. The reverse
silently invalidates every historical comparison while every artifact still
claims the same fixture.

---

## 7. Standing constraints that do not relax on a development host

Full access to a machine is not permission to loosen the product's rules. These
are permanent and are in the threat model:

- **T-AMPLIFY.** HTTP load generation targets only a server the agent started.
  There is no "benchmark this URL" feature and there will not be one. The
  external generator mode is *consent*, not access control — it needs a ticket
  the target minted.
- **T-DB.** A database module creates and destroys its own instance or reports
  itself not measured. There is no fallback to a server on the host, and adding
  one is not a shortcut for testing.
- **T-CONFIG.** `darcbench proxy` never writes into a path the web server
  reads, and never reloads.
- **Images are pinned by digest.** A tag is a mutable pointer; a tag-pinned
  benchmark measures whatever the publisher pushed last.

If one of these makes a task awkward, the task is wrong, not the constraint.

---

## 8. First-session checklist

The 2026-08-14 session, done:

```
[x] docker info prints a Server block
[x] docker pull hello-world succeeds
[x] cargo test --workspace passes before changing anything
[x] postgres and valkey digests resolved and recorded with today's date
[x] Pin::Pending → Pin::Pinned for both
[x] Sandbox::launch exercised against a real daemon, defects found and fixed
[x] docker ps -a clean after a run; orphan reaping verified by killing a run
[x] Modules registered, anchors added, manifests written, profile set chosen
[x] Full gate green
[x] Roadmap updated: 🚧 → ✅ only for what actually runs
```

The next session, for `wordpress.*`:

```
[ ] WordPress and MariaDB digests resolved and recorded with that day's date
[ ] Each new image's run_as pair read from the image, not guessed
[ ] Each new image's ready_probe chosen and checked to exit non-zero when down
[ ] tmpfs and memory ceiling revisited: they are ONE budget (see §3, step 2)
[ ] workload_runs_outside_this_process returns true, or the load ceiling will
    degrade the module on an idle machine
[ ] download_bytes set on each allow-list entry, and max_network_bytes in the
    manifest matches - the image WILL be fetched on a machine that lacks it
[ ] docker rmi the images and run again: an implicit pull inside a measurement
    is invisible on the host that pulled them by hand
[ ] The module driven with examples/probe.rs before it is registered
[ ] AND driven again through `darcbench run --modules`, which is where the
    sixth defect appeared and the probe could not have
[ ] Installation verified before any number is recorded - Phase 4's exit
    criterion. A WordPress serving a setup screen produces fast, meaningless
    numbers
[ ] Cache disclosure: Origin and Cached are separate metrics, and which object
    and page cache were active is in `comparability`
```

The last line of the first list is the one that matters. Everything in this
repository that says ✅ is meant to be a thing somebody has watched work.
