# Backlog

Items not scheduled into a specific phase, or discovered during implementation.
Ordered roughly by value. Phase-scheduled work is in [ROADMAP.md](ROADMAP.md).

## Correctness and methodology

- [ ] **Per-category stability**, replacing the single median-CV multiplier. The
      current model can under-weight one very unstable subsystem inside an
      otherwise steady run.
- [ ] **Validate category weights against real application benchmarks.** They are
      currently reasoned, not fitted. Any change is a scoring major bump.
- [ ] **Fit `cv_ceiling` and `weak_link_cap_factor` to a real corpus.** Both are
      published judgement calls today.
- [ ] Dynamic repetition: re-run a module whose CV exceeds its bound, up to a
      bounded multiple, while still publishing the final CV.
- [ ] Working-set sizing derived from measured cache topology rather than
      constants, for the memory module.
- [x] ~~Steady-state detection for storage~~ — the drift between the first and
      last third of the measured repetitions is published per write workload and
      warns below 0.70. Full SSD *preconditioning* is still open: it would take
      hours and write far more than a benchmark should, so the affordable half
      shipped first.
- [x] ~~Multi-endpoint network aggregation that does not let one fast CDN
      dominate~~ — latency, connect and jitter are sampled across three
      operators and reduced to one value per repetition. **Throughput is still
      single-provider**, and that is the part worth keeping open: a second bulk
      endpoint needs an operator whose published purpose covers being sent the
      traffic, which is a conversation rather than a coding task.
- [ ] Packet loss for `network.transfer`. Needs ICMP or raw sockets, so it is
      blocked on the privileged helper below. Declared as not measured today
      rather than inferred from TCP behaviour.
- [ ] Upload throughput for `network.transfer`, subject to finding an endpoint
      whose published purpose covers receiving bulk data.
- [ ] IPv6 measured as its own path rather than merely detected. Doubles the
      traffic sent to a third party, so it needs a reason beyond completeness.
- [ ] Network in the endurance profile, to catch bandwidth quotas and traffic
      shaping over an hour — a real phenomenon the profile currently cannot see.
      Blocked on an endpoint whose operator has agreed to an hour of traffic;
      the existing allow-list is bounded by a per-run ceiling that cycling would
      breach. This is a conversation, not a code change.
- [x] ~~A runtime load ceiling~~ — the telemetry sampler subtracts the agent's
      own CPU consumption from the machine's, so the guard sees only work the
      benchmark did not do. Sustained competition degrades the modules measured
      under it; heavy competition over five minutes stops the run. **Still open
      in it:** the guard is not enforced under container scope, because
      `/proc/stat` without a namespaced `/proc` describes the host and every
      other tenant would read as external load. Reading the cgroup's own
      `cpu.stat` would close that, and would also give the figure a denominator
      of the CPUs the run is entitled to rather than the ones the host has.
- [ ] Kernel writeback is charged to the runtime load ceiling as external work.
      `storage.mixed` fills its fixture with buffered writes, and the flush
      threads' CPU is not in the agent's own `utime`+`stime`, so a slow disk on
      a low-core host could in principle degrade a storage module for the
      agent's own I/O. Not observed, and it needs 10% of the machine sustained
      for 20 s; the fix is the same cgroup accounting the container-scope gap
      needs.
- [ ] A namespaced-`/proc` test for the runtime load ceiling. The guard is
      disarmed under container scope *or* a cgroup CPU quota below the host's
      CPU count, which covers the common cases; a container with no quota and no
      detectable marker is still missed, and the honest test is whether
      `/proc/stat` is namespaced rather than any inference from labels.
- [ ] The load ceiling's thresholds are a share of the whole machine, so 10%
      means 6.4 cores on a 64-core host and 0.2 on a 2-vCPU VPS - a ~30x spread
      in operational meaning across the fleet this tool targets. A share is the
      right *shape* (contention matters relative to capacity) but the small end
      deserves checking against a real `unattended-upgrades` run before 1.0.
- [x] ~~**`Scope::BareMetal` is decided without recording why.**~~ Every branch
      of `detect_scope` cited its evidence except the bare-metal fallback,
      whose condition was `!vendor.is_empty() || !product.is_empty()` - true on
      any host that can open `/sys/class/dmi/id`, which includes every VM.
      `platform::tests::scope_detection_is_evidence_backed` failed on the
      machine it was found on, which reports the SMBIOS placeholder
      `Default string` for both fields. `dmi_identity` now treats the known
      placeholders as absent: a host with a programmed DMI is `BareMetal` and
      says so with the vendor named, and one without is `Unknown`, which is
      what the evidence supports. This changes what a bundle *claims*, not what
      a run *enforces* - `Scope::Unknown` arms the runtime load ceiling exactly
      as `BareMetal` does, since only `Container` disarms it.
- [ ] A failed `/proc/stat` read yields a zeroed telemetry snapshot, which both
      publishes a fabricated `cpu_busy_pct: 0.0` and resets the load-ceiling
      tallies. Fails open on a guard that stops runs; the snapshot should carry
      whether it was actually read.
- [ ] `RunIndex::list` drops rows it cannot convert while `get` propagates the
      error, and `prune` selects by position in that list - so a dropped row
      would shift the `--keep-last` window by one. No trigger exists against the
      current schema; the asymmetry is still worth removing.
- [ ] Resource limits on child processes for the runtime modules. `rlimits` need
      `Command::pre_exec`, which is `unsafe`, and the workspace forbids it - so
      `php.runtime` bounds its children by wall clock only. A PHP whose
      `memory_limit` is `-1` can still be asked for more memory than the machine
      has. Needs a vetted wrapper crate or the privileged helper below.
- [ ] The Web category's basket differs depending on whether PHP is installed:
      `php.runtime` roughly doubles the metric weight in it, and contributes
      nothing on a machine without PHP. Two `web` runs on comparable hardware
      can therefore produce Web scores computed from different baskets, with
      nothing saying so. "Which modules contributed to a category" belongs in
      the comparability keys.
- [ ] `runtime_exec`'s ancestor walk covers the *resolved* path only, so an
      attacker who controls an ancestor of an allow-listed *name* can retarget a
      directory symlink. Not an escalation - the target must still pass the
      check, so they can only substitute another root-owned binary - but a
      `symlink_metadata` pass over the original chain would close it.
- [ ] Per-cycle telemetry alignment. The sustained diagnosis compares the
      opening and closing thirds of the whole telemetry series, which is right
      when cycles are evenly spaced and slightly off when one cycle ran long.
      Tagging each snapshot with its cycle would make the windows exact.
- [ ] Record external tool versions (fio, and any others) in the bundle;
      comparability depends on them. No module shells out to anything today, so
      this is currently vacuous — it becomes real with the first adapter.
- [ ] Optional fio adapter for `storage.mixed`, as a cross-check against the
      native implementation and as a route to io_uring queue depths. See the
      Phase 2 note in [ROADMAP.md](ROADMAP.md) for why native came first.
- [ ] `storage.mixed` reaches queue depth with one thread per outstanding
      operation. `io_uring` would be cheaper and would let depths above ~32 be
      measured without the scheduler dominating, but needs either `unsafe` or a
      vetted wrapper crate.

## Agent

- [x] ~~SQLite run index replacing the directory scan; retention and pruning.~~
      Bundles remain the source of truth; the index is rebuilt from them at
      startup by `reconcile`. **Still open:** pagination for the run list, which
      is bounded at 200 rows today, and the fleet-scale queries Phase 7 wants.
- [x] ~~**A fresh `serve` lists runs it cannot open.**~~ `get_run`, `get_bundle`,
      `get_report` and `stream_events` resolved only through `RunManager::get`,
      which searches the in-memory `runs` vector - populated solely by runs
      *this* process started. So a `serve` started after a CLI run listed the
      run and answered 404 for its bundle, its report and its event stream,
      while the files sat in the run directory the whole time. The same defect
      the index was introduced to fix for the *list* (ROADMAP Phase 2), left
      half-migrated. Each now falls back to disk on an in-memory miss, per the
      ADR-0005 hierarchy: bundles are the truth, the index caches them, memory
      caches that. The event stream replays `events.ndjson` and closes - there
      is no live stream to join once the process that ran it has exited - and
      it parses strictly, so an undecodable record is reported as
      `event_log_unreadable` rather than silently shortening the replay.
      `cancel` still returns 404: a finished run cannot be cancelled.
- [ ] `darcbench status` / `cancel` against a *running* agent over a Unix socket.
- [ ] Stale-run detection on startup. Crash *recovery* is now partly covered:
      `reconcile` reindexes bundles written by a process that died, and ignores
      run directories with no bundle. What is missing is noticing that such a
      directory belongs to a run that was interrupted, and saying so.
- [x] ~~Watchdog: max runtime, load ceiling, thermal guard, transfer ceiling.~~
- [ ] `--dry-run` that reports the full plan without executing.
- [ ] Privileged helper separated from the unprivileged HTTP process, for the
      Phase 2+ modules that need capabilities.
- [ ] Shell completions.
- [ ] The query-string token authenticates *every* read endpoint, not only the
      one that needs it. `EventSource` cannot set headers, which is the reason
      the mechanism exists, but the UI actually spends it once on
      `POST /api/v1/session?token=` and rides the cookie afterwards. Behind the
      reverse proxy of ADR-0014 a query token lands in the proxy's access log in
      plaintext, so the blast radius is wider than the need. Narrowing it to the
      session endpoint is a documented API change (`docs/API.md` lists `?token=`
      as ambient auth on the whole surface), not a bug fix.
- [ ] `EventRecord::append` encodes every event twice and allocates a
      `serde_json::Value` tree for the second: once with `to_vec` for the NDJSON
      log, then again through `canonical_json`, which goes via `to_value` before
      re-serialising. An endurance run emits thousands of events, and this
      module's own doc comment argues that the observer's cost is charged to the
      measurement. A canonical serialiser that writes straight to the hasher
      would remove the intermediate tree and one of the two encodings.
- [ ] `EventRecord::ndjson` accumulates the whole event log in memory for the
      lifetime of the run and is written out only at the end. Same exposure as
      the unbounded `RunManager::runs` above, and the same fix shape: stream it
      to `events.ndjson` as it is produced.

## Protocol and clients

- [ ] Generate TypeScript types from the Rust protocol once it stabilises;
      today a CI parity check covers the gap.
- [ ] `scripts/check-protocol-parity.sh` — currently referenced by
      `apps/web/src/types.ts` and needs to actually exist in CI.
- [ ] Fuzz the event and bundle decoders (`cargo-fuzz`).
- [ ] Compression for the SSE stream on slow links.

## UI

- [x] ~~Radar / balance visualisation~~ — hand-rolled SVG, no dependency, with
      the ranked category list beside it as the text equivalent. `balance_index`
      is derived client-side from the category scores because the score event
      does not carry it; publishing it on the event would remove the
      duplication.
- [x] ~~Run-to-run comparison view~~ — over `darcbench compare` and
      `GET /api/v1/runs/{a}/compare/{b}`. **Still open:** *server-to-server*
      comparison, which needs a second machine's bundle to be importable, and
      pagination of the run list, bounded at 200 rows today.
- [ ] Raw metric explorer with per-repetition drill-down.
- [ ] Printable report view distinct from the HTML export.
- [ ] Settings: telemetry rate, theme, redaction preference.
- [ ] Automated Playwright tests in CI; today browser verification is manual.

## Testing

- [ ] Low disk, missing dependency, occupied port, failed container, database
      startup failure, invalid WordPress installation, no internet — all
      currently untested paths that matter once Phase 2–4 modules exist.
- [x] ~~`scripts/e2e.sh` never fetches a per-run endpoint for a run it did not
      start inside the serving process~~ - which is why the `serve` defect above
      survived 45 green checks. It now asserts that `serve` returns the summary,
      bundle and report of a run made by an earlier process, that the replayed
      event stream matches `events.ndjson` line for line and ends on
      `run.completed`, and that cancelling a finished run is still refused.
- [ ] Root vs non-root test matrix.
- [ ] arm64 CI runners.
- [ ] Slow-SSE-client test with a deliberately throttled consumer.
- [ ] Tampered-bundle corpus as a fixture set.
- [ ] Coverage measurement (`cargo-llvm-cov`).
- [ ] Multi-machine reproducibility corpus — blocked on calibration hardware.
- [ ] Promote the `msrv` CI job from advisory to blocking, or raise
      `rust-version`. `Cargo.toml` promises 1.82 and, until that job was added,
      nothing built with it: CI used the pinned 1.97.1 and current stable, both
      far newer. The job follows the `lint-latest` precedent and does not block
      a merge yet, which means the promise is now *visible* rather than kept.
- [ ] `deny.toml` carries an `[advisories]` section with `yanked = "deny"` that
      is never evaluated: CI runs `cargo deny check licenses bans sources`, and
      advisories are covered separately by `cargo audit`, which does not deny
      yanked crates unless asked. Either add `advisories` to the `cargo deny`
      invocation or delete the dead config — the current state reads as a policy
      and behaves as a comment.

## Research

- [ ] Primary-source capture for the providers marked "not captured" in
      [MARKET-RESEARCH.md](MARKET-RESEARCH.md), notably Vultr (returned 403),
      Linode/Akamai, AWS, Azure, GCP, Scaleway, netcup, RackNerd, Leaseweb,
      Hivelocity, IONOS, Equinix Metal.
- [ ] Repeatable, dated scraping process for plan metadata.
- [ ] Decide whether the Phase 4 OLTP workload is TPC-C-derived at all; if so,
      apply the HammerDB naming discipline in full.
- [ ] Review published academic work on benchmark reproducibility for the
      calibration procedure.

## Documentation

- [ ] OpenAPI document generated from the router rather than hand-written.
- [ ] A worked "how to read a DARCBench report" guide for non-specialists.
- [ ] Provider/plan taxonomy specification.
- [ ] Public methodology page suitable for citation.

## Discovered during implementation

- [x] ~~Geometric mean alone does not prevent one catastrophic subsystem being
      hidden~~ — fixed with the weak-link cap; test added.
- [x] ~~`serde_json` without `float_roundtrip` breaks signature verification
      after a disk round-trip~~ — feature enabled workspace-wide; regression test
      added.
- [x] ~~Path normalisation dropped `..` but not percent-encoded traversal~~ —
      any segment containing `..` is now dropped.
- [x] ~~Server-side validation waved through bundles naming an unknown scoring
      model, making arbitrary scores `Validated`~~ — unrecognised model is now
      fatal; regression test added.
- [x] ~~Score recomputation compared only total, categories and stability, so
      clearing `missing_required_categories` dodged the `Partial` downgrade~~ —
      every field is now compared and eligibility comes from the recomputed
      card.
- [x] ~~The single-run guard checked and inserted under separate locks~~ — now a
      single atomic check-and-insert; concurrency test added.
- [x] ~~The SSE handler snapshotted the backlog before subscribing, so an event
      emitted in between was lost~~ — order corrected; the code comment had
      claimed the correct order while the code did the opposite.
- [x] ~~The session cookie was marked `Secure` on non-loopback binds even though
      the agent serves plain HTTP, breaking SSE with 401~~ — now derived from
      `X-Forwarded-Proto`.
- [ ] `AccessToken::matches` compares in constant time only after a length
      check. The length is public (64 hex chars) so this is fine, but it should
      be revisited if variable-length tokens are ever introduced.
- [x] ~~The replay buffer is a `Vec` with `remove(0)` on overflow — O(n) per
      eviction~~ — now a `VecDeque` ring buffer; replay resumes by binary search
      rather than a scan.
- [x] ~~The sequence number was allocated outside the replay-buffer lock, so the
      module thread and the telemetry task could interleave and emit a lower
      `seq` after a higher one~~ — allocation, append and broadcast now happen
      under one lock; concurrency test added.
- [x] ~~A run was marked terminal before `events.ndjson` was written, so a
      consumer that waited for completion could read a run directory with no
      event log~~ — the terminal state is now published last.
- [x] ~~The 1 Hz telemetry sampler re-walked `/sys/class/thermal` every tick and,
      on hosts without `cpufreq`, re-read all of `/proc/cpuinfo` — whose cost
      the kernel scales with core count~~ — both sources resolved once; per-tick
      cost down ~40%.
- [ ] `RunManager::runs` grows without bound: every run this agent has executed
      keeps its bundle, replay buffer and telemetry series in memory for the
      lifetime of the process. Bounded by the SQLite run index and retention
      work already scheduled in Phase 2; until then a long-lived `serve` process
      doing scheduled runs is the exposure.
- [ ] `ReferenceProfile::get` formats a `"<module>/<metric>"` key per lookup.
      Negligible at the current metric count; revisit if a profile ever carries
      hundreds of anchors.
- [ ] `memory.bandwidth` builds its permutation with two full-size arrays (the
      visit order, then the next-pointers). Peak construction memory is twice
      the chase working set, which the 3-buffer budget already covers, but a
      single-array construction would free real headroom on a small instance.
      Sharpened on review: the second array is not just an extra allocation, it
      is an extra *step*. Sattolo's algorithm run over an identity array already
      yields a single full-length cycle read as a next-pointer function, which
      is exactly what the chase wants — so `permutation()` could return `order`
      directly and drop both the `next` array and the O(n) pass that fills it.
      The rewrite changes which permutation a given seed produces, so it needs
      the pointer-chase latency to be re-measured on a known host before it
      lands, not merely a green test suite.
- [ ] The unit suite cannot run a full profile at a realistic working-set size:
      an unoptimised `memory.bandwidth` run took over eight minutes. Full-profile
      coverage lives in `scripts/e2e.sh` against a release build. A test hook for
      injecting `ModuleParams` would let the unit suite cover it cheaply.
- [x] ~~Preflight risk *fell* as a run got more invasive: the safety-class map
      dipped, sending `WritesTemporaryFiles` below `ComputeIntensive`~~ — the
      mapping is monotonic and a test pins that it stays so.
- [x] ~~`statvfs` on a state directory that does not exist yet reported unknown
      free space, which every guard treats as unsafe — so the first run on a
      fresh install would have been refused once any module wrote to disk~~ —
      free space now resolves against the nearest existing ancestor.
- [x] ~~`Profile::ReadOnly` reported itself standard, so a run that cannot
      measure writes could claim a comparable total~~ — now `Custom`, and the
      profile resolves to no module that writes.
