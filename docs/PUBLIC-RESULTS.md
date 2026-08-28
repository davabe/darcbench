# Public results: darcbench.com

How anyone submits a server benchmark, how a submission earns the right to be
ranked, and what must exist before any of it ships.

**Status: design. Nothing here is implemented.** The sequencing section says why
that is deliberate rather than pending, and what it is waiting for.

---

## 1. What this is for

The goal is the thing 3DMark is for gaming PCs and Geekbench is for laptops, for
**servers, measured as a whole machine**. Not a CPU number. A machine has a CPU,
memory, a disk, a network path and a software stack, and it is bought to run
WordPress or Postgres or a Node API — so the number that describes it has to
come from all of that.

[PRODUCT-BIBLE.md](PRODUCT-BIBLE.md) already states this positioning; this
document is about the half that makes it public: a site where an operator runs
one command, uploads a result, and sees where their machine stands.

Two properties decide whether that site is worth building at all.

**A submitted number has to mean something.** A leaderboard of numbers anyone
can invent is a leaderboard nobody cites. Everything in section 3 exists to
make a rank a claim we can defend.

**It has to survive being popular.** The moment a ranking is worth having,
somebody tries to fake one, and somebody else tries to knock the endpoint over.
[THREAT-MODEL.md](THREAT-MODEL.md) has carried `T-SCORE-FRAUD` since the first
week of the project for exactly this reason.

### What we are deliberately not copying

[COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md) records this per competitor;
the two that matter most here:

**PassMark averages submissions per CPU model.** That destroys the thing this
product measures. Two machines with the same CPU and different disks are
different servers, and averaging them produces a number describing neither. We
never average across machines, and a leaderboard row is always **one run on one
machine**, not a model aggregate.

**AnTuTu's problem is not its maths, it is that its results are worth faking.**
Vendors have shipped firmware that detects the benchmark. Our exposure is
smaller — the operator owns the machine, so there is no vendor to detect us —
but the incentive is identical the moment a provider's sales page can cite a
rank. The answer is the tier ladder in section 3, not detection heuristics.

---

## 2. Why the hard part is not the website

The website is a few pages. The hard part is already built, and it was built in
this order on purpose.

A result is a **signed bundle**: Ed25519 over DARCBench Canonical JSON, covering
every raw metric, the environment snapshot and the verdict
([ADR-0008](adr/0008-result-verification.md)). The server never trusts a
bundle's own scores. It **recomputes every score from the raw metrics** with the
named model and rejects a mismatch. Editing a score without editing the metrics
is caught by recomputation; editing the metrics breaks the signature.

That is why the interesting question is not "how do we accept uploads" but "what
does accepting one entitle it to", which is section 3.

---

## 3. The tier ladder is the anti-fraud design

`ResultState` already encodes it, `is_rankable()` already enforces it, and
`darcbench-report` already implements the checks that move a bundle between
tiers. None of this is new work; it is the reason the site is buildable.

| Tier | What it required | Ranked? |
|---|---|---|
| `Local` | Never left the machine | — |
| `SelfReported` | A valid agent signature, nothing else | **No** |
| `Validated` | Server recomputed every score from raw metrics; all invariants held | Yes |
| `Verified` | Validated **plus** a server-issued nonce, a redeemed run token, and an agent build hash matching a published release | Yes |
| `Official` | Verified **plus** DARCBench-controlled provisioning | Yes |
| `Invalid` | Failed validation. Retained, never aggregated | No |
| `Partial` | Required modules missing | No |
| `Custom` | Non-standard module set or parameters | No |

**The residual risk is stated, not hidden.** An operator who controls the
machine can patch the agent and sign fabricated numbers with their own key. This
is unfixable without hardware attestation, which DARCBench will not require. It
is handled by *classification*: such a bundle can never exceed `SelfReported`,
and `SelfReported` is not rankable. We do not build invasive hardware
fingerprinting to chase it, and the site must never imply we did.

**Consequence for the site's copy:** the leaderboard shows the tier on every
row. A page that displays a rank without saying which tier earned it is
misrepresenting the evidence, and reviewers will notice before users do.

---

## 4. The submission path

### 4.1 Anonymous by default

An operator should be able to publish a result without creating an account. An
account requirement on the first interaction costs more submissions than it
prevents fraud, and fraud is not deterred by a free signup anyway.

So: **no account for `SelfReported` and `Validated`.** An account is required
only to *claim* a result — to attach it to a provider profile, to dispute one,
or to hold the run tokens that `Verified` needs.

### 4.2 The flow

```
                         (optional, needed only for Verified)
  darcbench nonce  ─────────────────────────────────────────▶  POST /api/v1/nonce
        │                                                          │
        │  ◀──────────── nonce + run_token, TTL ~2h ────────────────┘
        ▼
  darcbench run --profile standard --nonce <nonce>
        │   the nonce is embedded in the run record and covered by the signature
        ▼
  darcbench submit  ───────────────────────────────────────▶  POST /api/v1/results
        │                                                          │
        │                                          1. size + schema gate
        │                                          2. signature check
        │                                          3. enqueue
        │  ◀────────────── 202 Accepted, result id ─────────────────┘
        │
        ▼                                       (worker, off the request path)
  poll or share link                            4. recompute every score
                                                5. redeem nonce, exactly once
                                                6. assign tier, publish
```

**The agent always initiates.** The control plane never connects inward to an
agent ([ADR-0010](adr/0010-control-plane-deployment.md)). That is what keeps
"no unauthenticated remote command execution" true by construction, and it must
not be weakened for the convenience of a submission flow.

### 4.3 The nonce is what kills replay

Today, replaying an old good result is listed in `T-SCORE-FRAUD` as unmitigated
and deferred. The mechanism that fixes it:

- The nonce is issued **before** the run, is single-use, and expires.
- It is written into the run record, so it is **covered by the signature** — a
  bundle cannot borrow another run's nonce without breaking its own signature.
- The server redeems it in the same transaction that publishes the result, so
  two concurrent submissions of the same bundle cannot both win.
- A bundle whose nonce is unknown, expired or already redeemed is not rejected —
  it is **downgraded to `Validated`**. It may still be a perfectly honest run
  that took too long; it simply cannot claim `Verified`.

That last point is the design rule for the whole endpoint: **downgrade rather
than refuse, wherever the evidence is merely absent instead of contradictory.**
A refusal teaches an operator to stop submitting. A downgrade teaches them what
the next tier costs.

### 4.4 Build attestation

`Verified` requires the agent's build hash to match a published release. This
needs two things, and only one of them involves the server:

- The agent records a hash of its own executable in the bundle
  (`meta.agent_build_hash`), which is signed along with everything else.
- Releases publish the hash of every artifact, and the server compares.

The agent half is buildable now and useful now — see section 7.

**What it does and does not prove.** It proves the bundle was produced by a
binary byte-identical to one we published. It does not prove that binary was
running unmodified in memory, and nothing short of hardware attestation would.
It raises the cost of faking from "edit a JSON file" to "reproduce a build and
patch it", which is the intended effect, and the tier ladder is what carries the
rest.

---

## 5. Keeping the endpoint standing

The submission endpoint is the first part of DARCBench that strangers can reach.
Everything the agent does today is loopback-first and token-gated; this is not.

### 5.1 Cost asymmetry is the thing to design against

Score recomputation is CPU-bound and runs over every metric in a bundle. If it
happened on the request path, a single client could spend our CPU at the cost of
one HTTP request. So:

**Recomputation never happens inline.** Submission does the cheap checks and
enqueues. This is already ADR-0010's shape — "score recomputation must be a
batch job from day one" — and this is the reason.

The ordering of the cheap checks matters, cheapest first:

1. **Content-length and a hard body cap**, before reading a byte of it.
2. **Decompressed-size cap with an incremental limit**, not a post-hoc check —
   a gzip bomb is small until it is not, so the limit is enforced as the stream
   is decompressed and it aborts mid-stream.
3. **Schema shape gate**: module count, metric count and sample count bounded
   before any structural work. A bundle with ten million samples is not a run.
4. **Signature verification.** One Ed25519 verification is cheap and it is the
   check that makes everything after it worth doing.
5. **Only then**: enqueue.

### 5.2 Rate limits

| Scope | Why this scope |
|---|---|
| Per source IP | The blunt instrument. Necessary, and insufficient — IPs are cheap. |
| Per agent public key | A key is free to generate, but every submission from one key is linkable, so abuse from a key is attributable and bannable. |
| Per nonce | A nonce is issued to a requester and redeemed once. This is the only limit that costs an attacker anything real. |
| Global queue depth | A back-pressure valve: when the validation queue is deep, new submissions are accepted but queued, and the response says so. Never dropped silently. |

**Why not a CAPTCHA or proof-of-work on submission.** The submitter is usually a
CLI on a headless server, which is exactly the client a CAPTCHA cannot serve.
Proof-of-work punishes an honest operator's server — the machine being measured —
which is both rude and self-defeating on a benchmark tool.

### 5.3 Never execute, never fetch

A bundle is **data**. The server never executes anything from it, never follows a
URL in it, and never renders an unescaped string from it. `T-XSS` and
`T-SSRF-METADATA` already cover the agent side; the same rules apply server-side
and must be written into the control plane's own tests rather than inherited by
assumption.

The environment snapshot in a bundle is attacker-controlled text. Hostnames, DMI
strings, distribution names, container runtime names — all of it. It is escaped
on output and never used to build a path, a query or a request.

### 5.4 Privacy is enforced server-side

Redaction already exists in the agent, and public share pages are opt-in
([PRIVACY.md](PRIVACY.md)). But a submitted bundle may carry unredacted fields
if the operator asked for them locally. **The server re-applies the redaction
policy on publication rather than trusting the bundle's own `redacted` flag.**
The raw bundle stays in object storage for verification and dispute; the public
page is rendered from a redacted projection.

---

## 6. The leaderboard

### 6.1 A row is one run on one machine

Never a model aggregate, never a provider average. If somebody wants the median
of thirty Hetzner CPX41 results, that is a *view over rows*, computed and
labelled as such, with the spread shown beside it. The rows stay addressable.

### 6.2 Partitioning is mandatory, not a filter

A leaderboard is only meaningful within a partition where the numbers are
comparable. The partition key is not a UI convenience; it is a correctness
requirement, and it comes from facts the bundle already carries:

- **Scoring model version.** Scores from `dbs/1.0.0` and `dbs/2.0.0` are not
  comparable. Rescoring a corpus under a new model is a batch job that produces
  a new partition; the old one is retained and labelled, never silently
  overwritten.
- **Profile.** Only `standard` totals are rankable at all. `Custom` never is.
- **Scope.** Bare metal, VM and container are different questions.
- **Build target.** The same workload compiled for a different architecture is
  not the same workload.

And now also, from work already landed: **the category basket**. A Web score
computed with `php.runtime` present and one computed without it are different
measurements, and `CategoryOutcome.modules` is what lets a leaderboard see that
rather than rank them against each other.

### 6.3 What every row shows

Tier, scoring model version, and the balance of the machine — not just the
total. A single number is what makes PassMark's corpus hard to reason about; the
radar already exists in the local console and the same shape belongs here. A
server that scores 900 by being uniformly good and one that scores 900 with a
brilliant CPU and a terrible disk are different purchases, and `balance_index`
and the weak-link cap already encode that distinction.

### 6.4 Disputes are public

Already policy in [COMMERCIAL-STRATEGY.md](COMMERCIAL-STRATEGY.md): a challenge
is public, the raw bundle is public, the response is public, and a wrong result
is invalidated publicly rather than explained away. Invalid results are
**retained** — "a leaderboard where bad results silently vanish is a leaderboard
nobody can audit."

---

## 7. Sequencing: what has to exist first

**The recommendation is to build the explanatory site now and the submission
endpoint later.** Not because the endpoint is hard, but because shipping it
early would publish numbers we would then have to retract at scale.

### 7.1 The blocker that is not about security

`dbs/0.2.0-dev` reports itself **uncalibrated** in every bundle, report and API
response. [ROADMAP.md](ROADMAP.md) is explicit: *"Calibration gates everything
that claims comparability."*

A leaderboard launched today would rank uncalibrated scores. When calibration
lands, every score changes and every rank moves. The project's own dispute
policy says we invalidate publicly and say why — and this would be that event,
self-inflicted, on every result ever submitted, on day one. It is the single
worst launch available.

Calibration needs three physical DARC-REF-1-class machines. That is the critical
path, and no amount of further coding moves it.

### 7.2 The blocker that is about security

ROADMAP.md, Phase 6, verbatim: *"Do not ship leaderboards before nonce and
attestation."*

Without the nonce, replay is unmitigated. Without attestation, a patched agent
is indistinguishable from a published one. Both are listed as ⏳ in
`T-SCORE-FRAUD` today. Shipping a ranking before them means the first serious
submitter is also the first successful attacker.

### 7.3 The order

| Stage | Needs | Ship the site's… |
|---|---|---|
| Now | Nothing new | **Explanatory half**: what DARCBench measures, the methodology, the download, the report format. No submission. |
| Phase 5 | Control plane: upload, object storage, server-side rescoring | Private result history. Share links, opt-in, unranked. |
| Phase 2 exit | **Calibration** → `dbs/1.0.0`, `calibrated: true` | Scores that will not be retracted. |
| Phase 6 | Nonce + run token, build attestation | `Verified`. **Only now: the public leaderboard.** |

The explanatory site is not a consolation prize. It is where the methodology
becomes citable, and a benchmark nobody can audit is one nobody trusts — the
citation page has to exist before the rankings do, not after.

### 7.4 What can be built now, and is not blocked

Small, useful on their own, and prerequisites for the above:

- **`meta.agent_build_hash`** — the agent hashing its own executable. Useful
  immediately for comparability (two builds of the same version are
  distinguishable) and a hard prerequisite for `Verified`.
- **A nonce field in the run record**, plumbed through the bundle schema and
  covered by the signature, with no server to issue one yet. Adding it later
  means a schema change during a launch; adding it now costs an optional field.
- **Provider and plan as declared, unverified operator input.** A leaderboard
  needs "Hetzner CPX41" and the agent cannot know it. Declared input, labelled
  as declared, is honest and useful; inferring it from DMI strings is a guess
  wearing a fact's name.
- **The OpenAPI document**, generated rather than hand-written. Already on the
  backlog, and it is what a submission client is written against.

---

## 8. Two domains, and which is which

They coexist, and the split is deliberate rather than a leftover.

**`darcbench.com` is the product.** What DARCBench measures, how to run it, the
download, the methodology, the leaderboards and the result pages. It is where a
stranger with a server lands.

**`getdarc.com` is the owner.** getDARC / Tombatossals Softworks LLC. It carries
the things that identify the *vendor* rather than the product, and those are not
interchangeable with the product's brand.

The rule for deciding where a reference belongs: **does it serve someone looking
for the tool, or does it identify who is accountable for it?**

| Reference | Domain | Why |
|---|---|---|
| `Cargo.toml` `homepage` | `darcbench.com` | The crate's homepage is the product's page. `authors` already carries the owner. |
| CLI `--help` banner | `darcbench.com` | A user reading it wants where to learn more, not who owns the copyright — the company name is right there beside it. |
| `security@getdarc.com` | `getdarc.com` | A security contact identifies who receives and triages, which is the company. It is also a published commitment in SECURITY.md and must only change alongside a working mailbox. |
| `com.getdarc.darcbench.owned` label | `getdarc.com` | Reverse-DNS labels are conventionally the *organisation's* domain, so this is more correct as-is. See the warning below. |

### The container label must not be changed casually

`reap` matches this label to find containers a previous run abandoned - the
mitigation for "the release profile aborts on panic, and nothing runs on
`SIGKILL`, so a run that dies leaves its container behind". Changing the string
strands every container labelled by an older agent, on the machines least able
to notice.

If it ever has to change, the reaper matches **both** for at least one release
cycle, and the old one is retired only once no supported version writes it.
There is no reason to change it: a label is an identifier, not a brand, and this
one is already the owner's domain, which is what the convention asks for.

### One constraint the site does not get to relax

`darcbench.com` should present results as well as anything in this market does -
that is a stated goal, and [COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md)
already records why: *"a benchmark people enjoy reading gets run."*

But the **HTML report the agent generates stays self-contained and offline**. It
embeds no external URL, and `scripts/e2e.sh` fails the build if one appears. An
operator runs this on a server that may have no egress at all, and a report that
degrades without the network is a report that cannot be filed, mailed or read in
a datacentre.

So the animated, spectacular presentation lives **on the site**, rendered from
an uploaded bundle - not in the artifact the agent writes. They are two
renderers over one signed document, which is also what keeps the local report
honest: it shows what the bundle contains, with nothing fetched.

## 9. Open questions

- **Does a submission need an account to be `Verified`?** Nonces have to be
  rate-limited to somebody, and an anonymous nonce is a free ticket. Leaning
  towards: anonymous nonces at a low rate, higher rates for accounts.
- **What happens to a result whose provider disputes it successfully?** Policy
  says retained and publicly invalidated. Whether it stays visible in the
  partition or moves to an invalidated view is unresolved.
- **How is a plan identified across providers that rename them?** Part of the
  provider/plan taxonomy already on the backlog.
- **Do we accept results from a self-hosted control plane?** AGPL makes
  self-hosting a promise; federated submission is a different trust model and is
  not currently designed for.
