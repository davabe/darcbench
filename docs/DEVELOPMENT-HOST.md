# Development host

What a machine needs in order to move DARCBench forward, what is blocked
without one, and exactly what to do first when one is available.

This document exists because a large amount of Phase 4 is written, tested and
**unable to run**, all for the same reason: it needs a container daemon and
access to a registry, and the machine it was written on has neither. It is
addressed to whoever — or whatever — picks the work up next.

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

### The trap this machine fell into

The host this was developed on has `/usr/bin/docker` — the **client** — and no
daemon. `docker version` answers happily from the client binary alone. That is
why `Runtime::discover` probes with `docker info` and treats "no runtime found"
and "runtime found, daemon silent" as two different failures: an operator
installs the first and starts the second.

If you see this, the host is not ready:

```
/usr/bin/docker is installed but its daemon did not answer: [...]
The module is reported as not measured; nothing on this host was used instead.
```

---

## 2. Where the work stands

Phases 0–3 are complete. Phase 2 has one open item — DARC-REF-1 calibration —
which needs three physical machines and is not what this document is about.

Phase 4 is where the block is:

| Deliverable | State | Blocked on |
|---|---|---|
| Container isolation tier | Argument boundary, refusals, naming, port parsing and reaping all tested. **`Sandbox::launch` has never run.** | A daemon |
| `database.oltp` | Complete. Not registered. | A pinned `postgres` digest |
| `database.cache` | Complete. Not registered. | A pinned `valkey` digest |
| WordPress fixture generator | ✅ **Complete and verified.** Pure, deterministic, checksum-pinned | — |
| `wordpress.*` | Not written | Pinned WordPress + MariaDB images, and the two above working |
| `deployment.container` | Build, cache and image save/load delivered. Startup and health are not | A pinned *runnable* base image |

Read [ROADMAP.md](ROADMAP.md) Phase 4 for the reasoning behind each. Everything
marked "not delivered" is declared in the relevant module's `limitations`, not
left to be discovered.

---

## 3. What to do first, in order

Each step depends on the one before it. Do not skip ahead: registering a module
whose image is unpinned puts a guaranteed precondition failure into every
profile that contains it, which is exactly why they are not registered yet.

### Step 1 — Resolve the two digests

```bash
docker pull postgres:17-bookworm
docker image inspect postgres:17-bookworm --format '{{index .RepoDigests 0}}'
# → postgres@sha256:....

docker pull valkey/valkey:8-alpine
docker image inspect valkey/valkey:8-alpine --format '{{index .RepoDigests 0}}'
# → valkey/valkey@sha256:....
```

Then in `crates/darcbench-modules/src/container.rs`, replace each
`Pin::Pending { .. }` with `Pin::Pinned("postgres@sha256:...")`.

Two tests guard this and will tell you if you got it wrong:
`every_allowed_image_is_pinned_by_digest` refuses a tag alongside a digest, and
`a_pending_entry_names_a_tag_to_resolve_and_a_reason` covers whatever is left
pending.

**Record the digest and the date in the commit message.** A digest is a fact
about a moment; six months later somebody needs to know which moment.

### Step 2 — Run the containers for the first time

This is the step nobody has done. `Sandbox::launch`, `wait_ready`, `exec` and
`Drop` have never touched a daemon.

```bash
cargo test -p darcbench-modules container -- --nocapture
cargo test -p darcbench-modules database_oltp -- --nocapture
cargo test -p darcbench-modules database_cache -- --nocapture
```

Then drive the modules for real. There is no CLI entry point for a single
unregistered module, so the quickest honest route is a throwaway example:

```rust
// crates/darcbench-modules/examples/probe.rs — delete it afterwards
fn main() {
    let m = darcbench_modules::database_oltp::DatabaseOltp::new();
    // build a ModuleParams with a scratch dir and a no-op reporter, then run
}
```

**Expect this step to find defects.** Every other part of this codebase found
some the first time it met reality — the reverse-proxy generator was rejected
by real nginx on its first run, and `web.static`'s response reader turned out
to be failing most large-object requests. Treat a clean first run as suspicious
rather than as success.

While you are there, verify by hand what the tests cannot:

- `docker ps -a` is empty afterwards. `Drop` removed the container.
- `docker ps -a --filter label=com.getdarc.darcbench.owned=1` finds nothing.
- Kill the process mid-run, then start another. `reap` must clear the orphan
  and the bundle must disclose that it did.
- The published port is on `127.0.0.1`, not `0.0.0.0`. `docker port <name>`.

### Step 3 — Register the modules

`crates/darcbench-modules/src/registry.rs`, in `builtin()`, following the
existing pattern. Then:

- **Scoring anchors** in `crates/darcbench-scoring/src/reference.rs`. A module
  whose metrics have no anchors shows up in `ScoreCard::unreferenced_metrics`,
  which is how you will notice if you miss one.
- **A manifest file** at `benchmarks/database/database.oltp.json` and
  `benchmarks/database/database.cache.json`. `./scripts/check-manifests.sh`
  fails if a registered module has none.
- **Profile membership.** `database.*` belongs in `deep`, not in `standard`:
  most machines have no container runtime, and a standard run coming back
  `Partial` on every machine that is not a container host would report the
  profile's own assumptions as a fault of the machine. Same argument as
  `php.runtime`.

### Step 4 — `wordpress.*`

Now it can be written against a base that is known to work.

The fixture already exists and is done:
`darcbench_modules::wordpress_fixture::Fixture::generate(FixtureSize::Standard)`
produces WXR with a checksum pinned by a test. What is missing is the module
that stands up WordPress + MariaDB, imports it with `wp import`, and measures
Origin, Cached, Database and Admin.

Two things to get right, both of which the roadmap already argues:

- **Cache disclosure is the whole point.** `docs/BENCHMARK-METHODOLOGY.md`:
  *"WordPress performance without a cache disclosure is meaningless."* The
  Cached and Origin numbers must be separate metrics, and which object cache
  and page cache were active must be in `comparability`.
- **Verify the installation before recording anything.** That is Phase 4's exit
  criterion, in as many words. A WordPress that returned a setup screen for
  every request would produce fast, meaningless numbers.

### Step 5 — `deployment.container`'s missing half

With a pinned runnable base image, startup and health become measurable. Note
that this changes what the build measures — see the module docs for why the
build itself must stay `FROM scratch` even after a base image is available.

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
| Tests flake under `-j` on a small VPS | Timing tests oversubscribed | Re-run pinned to fewer cores before concluding anything |

---

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

At the time of writing: 553 tests across six crates, e2e 45/45, 7 manifests.

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

```
[ ] docker info prints a Server block
[ ] docker pull hello-world succeeds
[ ] cargo test --workspace passes before changing anything
[ ] postgres and valkey digests resolved and recorded with today's date
[ ] Pin::Pending → Pin::Pinned for both
[ ] Sandbox::launch exercised against a real daemon, defects found and fixed
[ ] docker ps -a clean after a run; orphan reaping verified by killing a run
[ ] Modules registered, anchors added, manifests written, profile set chosen
[ ] Full gate green
[ ] Roadmap updated: 🚧 → ✅ only for what actually runs
```

The last line is the one that matters. Everything in this repository that says
✅ is meant to be a thing somebody has watched work.
