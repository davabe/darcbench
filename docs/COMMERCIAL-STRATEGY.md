# Commercial strategy

## The constraint everything else obeys

**Neutrality is the asset.** The moment a provider can buy a better score, every
score DARCBench has ever published becomes worthless — including the ones that
were honest. Every revenue idea below is tested against that first, and several
obvious ones are rejected on it.

## Editions

| Edition | Licence | Price | Contains |
|---|---|---|---|
| **Agent / Standalone** | Apache-2.0 | Free, forever | Full benchmark suite, local dashboard, signed bundles, HTML/JSON export. No account, no network |
| **Community platform** | AGPL-3.0 (hosted) | Free | Accounts, result history, public share pages, community leaderboards, provider comparison |
| **Professional** | Commercial | Per seat | Private results, scheduled runs, regression alerts, API access, longer retention, comparison exports |
| **Team / Fleet** | Commercial | Per agent | Organisations, RBAC, fleet benchmarking, aggregate dashboards, SSO, audit log |
| **Provider** | Commercial | Annual | Verified provider testing, plan-level pages, white-label reports, embeddable badges |
| **Self-hosted control plane** | AGPL-3.0 or commercial | Free / commercial | For organisations that cannot send results outside |

The standalone agent is deliberately not crippled. A benchmark nobody can run
freely does not become a standard, and a standard is the only thing worth
owning here.

## What is sold, and what is not

**Sold:** operating the platform — history, comparison, fleet management,
alerting, verification infrastructure, support, and the convenience of not
running it yourself.

**Never sold, at any price:**

- A better score, a re-run of an unflattering result, or its removal.
- Position on a leaderboard.
- Exclusion of a competitor's results.
- Influence over category weights or the scoring model.
- Advance notice of a result before publication.

## Governance rules

These are commitments, published so that breaking them is visible.

**Sponsored provider benchmarking.** A provider may pay for a *verified testing
engagement*: DARCBench provisions machines and runs the suite. The result is
published **whatever it says**, and it is labelled `Official` and marked as
funded by the provider. Payment buys rigour and attribution, never outcome.
A provider who pays and dislikes the result gets a published result.

**Affiliate links.** Permitted only where the link is disclosed on the same
page, and never on a ranking or comparison view. If a link could plausibly
influence a ranking's presentation, it does not go there.

**Provider-submitted results.** Accepted, always labelled as such, and never
promoted above independently-run results of the same tier.

**Editorial independence.** Methodology and weight changes are proposed
publicly, with rationale, before release. A change that would predictably move a
paying provider's ranking requires an explicit public note saying so.

**Disputes.** A provider may challenge a result. The challenge is public, the raw
bundle is public, and the response is public. If a result was wrong we
invalidate it publicly and say why — invalidation is not an embarrassment, it is
the system working.

**Score manipulation.** Detected manipulation results in the run being marked
`Invalid` and retained. Repeated manipulation results in an account being barred
from submitting to leaderboards. Retained rather than deleted, because a
leaderboard where bad results silently vanish is a leaderboard nobody can audit.

## Why the open-core boundary sits where it does

The agent is Apache-2.0 so hosting providers can run it on their own fleets
without a legal conversation — that is how it becomes the thing buyers ask for.
The control plane is AGPL so a competitor cannot run a closed fork as a service,
while anyone may self-host for their own infrastructure.

The methodology and scoring formulas are published separately under CC BY 4.0.
A benchmark whose scoring is a black box is a marketing instrument. Anyone must
be able to reimplement the model and check our arithmetic — that verifiability
*is* the product's defensibility.

## What the moat actually is

Not the code; it is Apache-licensed and forkable by anyone.

1. **The verified result corpus.** Verified results accumulate and cannot be
   fabricated retroactively.
2. **Methodological credibility.** Slowly earned, instantly lost.
3. **The reference calibration.** DARC-REF-1 is a published specification; being
   the maintainer of the canonical calibration matters.
4. **The variance dataset.** Longitudinal CV and steal-time data per provider
   segment is genuinely hard to assemble and is what buyers most need.

## Cost-efficiency scoring — deliberately withheld

"Score per euro" is the metric everyone asks for. It is not in the scoring model
because it requires a price the agent cannot know, changes without the machine
changing, and baking a vendor's price list into a benchmark score would
compromise the neutrality the whole product rests on.

It belongs in the control plane, computed from a **user-supplied** price, clearly
labelled as such, and never part of the DARCBench Total Score.

## Risks

| Risk | Response |
|---|---|
| A provider disputes a bad result publicly | Raw bundle is published; methodology is open; invite an independent re-run |
| Accusation of pay-to-play | Sponsored engagements labelled; governance rules public; results published regardless of outcome |
| A fork with different weights confuses buyers | Scoring model version is in every score; cross-version comparison refused, not approximated |
| Providers optimise for the benchmark | Partly the point. Mitigate by versioning workloads and rotating corpora at major versions |
| Nobody adopts it | The real risk. Mitigated by making standalone genuinely free and genuinely good |
