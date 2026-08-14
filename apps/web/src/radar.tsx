/**
 * The category radar: the *shape* of the machine, next to the number for it.
 *
 * # Why a chart at all
 *
 * A total score answers "how fast", and answers it badly on its own. Two
 * machines can total the same and be nothing alike - one even across every
 * subsystem, one carried by a fast CPU while its disk drags. That difference is
 * the thing an operator is actually buying, and `docs/SCORING-SYSTEM.md` §2.3
 * already quantifies it as `balance_index`. This panel is the visual
 * counterpart of that single number: one polygon over the scored categories, so
 * the lopsidedness is seen before it is read.
 *
 * # Why it is hand-rolled
 *
 * The agent serves this bundle under `script-src 'self'; connect-src 'self'`.
 * A charting library would be a second bundle to embed in the binary and a
 * dependency to audit, for a shape that is twenty lines of trigonometry. The
 * same reasoning produced `Sparkline` in `components.tsx`, and this component
 * follows its conventions: static SVG, `aria-hidden`, and never the only place
 * a number appears.
 *
 * # Why the chart is not the deliverable
 *
 * The polygon is supplementary. Every axis carries its category name *and* its
 * score as text, the readout beside it repeats all of them as a list, and both
 * survive the chart being invisible - to a screen reader, in a monochrome
 * print, or with fewer than three categories where there is no polygon to draw
 * at all. Nothing here is encoded by colour or position alone.
 */

import { memo, useMemo } from 'react';
import type { CategoryScore } from './types';

/**
 * `weak_link_cap_factor` from `crates/darcbench-scoring/src/model.rs`.
 *
 * Mirrored rather than transmitted: the `darcbench.events/1` score event does
 * not carry it, and changing the protocol to add one constant is not worth a
 * version bump. If the model ever moves off 4.0 this becomes wrong, so it is
 * named after the field it mirrors to make the grep obvious.
 */
const WEAK_LINK_CAP_FACTOR = 4.0;

/** The anchor every score is expressed against: 1000 = DARC-REF-1. */
const REFERENCE_ANCHOR = 1000;

/** Below three axes a polygon degenerates into a line or a point. */
const MINIMUM_AXES_FOR_A_POLYGON = 3;

export interface CategoryBalance {
  /** Categories only - facets arrive on the same array carrying weight 0. */
  readonly scoredCategories: CategoryScore[];
  readonly weakest: CategoryScore | null;
  readonly strongest: CategoryScore | null;
  /** Weakest ÷ geometric mean, in (0, 1]. `null` when it is undefined. */
  readonly balanceIndex: number | null;
  /** `WEAK_LINK_CAP_FACTOR × weakest`: the ceiling the total cannot exceed. */
  readonly weakLinkCap: number | null;
  /** Whether the published total is sitting on that ceiling. */
  readonly totalIsAtTheWeakLinkCap: boolean;
}

/**
 * Unweighted geometric mean, matching `stats::geometric_mean` exactly.
 *
 * Including its refusals: an empty set, or any non-finite or non-positive
 * member, yields nothing rather than a number. A zero category score would
 * annihilate the product, and reporting a machine's balance as 0.00 because one
 * subsystem failed to produce a score would be inventing a finding.
 */
function geometricMeanOf(values: number[]): number | null {
  if (values.length === 0 || values.some((value) => !Number.isFinite(value) || value <= 0)) {
    return null;
  }
  const logSum = values.reduce((accumulator, value) => accumulator + Math.log(value), 0);
  return Math.exp(logSum / values.length);
}

/**
 * Recomputes the balance figures from the category scores on the wire.
 *
 * **Derived client-side, deliberately.** `ScoreCard` publishes `balance_index`,
 * `uncapped_total` and `weak_link_applied`, but the `score.provisional` /
 * `score.final` events of `darcbench.events/1` do not carry them - see
 * `types.ts`, which is parity-checked against the Rust protocol crate by
 * `scripts/check-protocol-parity.sh`. Widening the protocol from the UI would
 * be the tail wagging the dog, so this recomputes the same formula from the
 * same inputs instead. It is the identical arithmetic on the identical
 * category scores, so it agrees with the authoritative value in the bundle;
 * should the model's formula ever change, this is the copy that goes stale, and
 * the bundle remains the thing that is true.
 *
 * The cap is inferred rather than reported. The total published in the event is
 * already `min(uncapped, cap)`, so a total that has landed exactly on the cap is
 * what a bound cap looks like from out here - stated as "the total is at the
 * cap", which is what we can actually see, rather than as a claim about a
 * deduction we did not witness.
 */
export function deriveCategoryBalance(
  categories: CategoryScore[],
  total: number | null,
): CategoryBalance {
  // Facets (`single_core`, `multi_core`) ride the same array with weight 0.0 so
  // the dashboard can tile them beside the categories. They are re-cuts of
  // compute, not axes of their own: drawing them would count one subsystem
  // three times and pull the polygon toward whichever category has facets.
  const scoredCategories = categories.filter((category) => category.weight > 0);
  const scores = scoredCategories.map((category) => category.score);

  const weakest = scoredCategories.reduce<CategoryScore | null>(
    (lowest, category) => (lowest === null || category.score < lowest.score ? category : lowest),
    null,
  );
  const strongest = scoredCategories.reduce<CategoryScore | null>(
    (highest, category) => (highest === null || category.score > highest.score ? category : highest),
    null,
  );

  const geometricMean = geometricMeanOf(scores);
  const balanceIndex =
    weakest !== null && geometricMean !== null && geometricMean > 0
      ? weakest.score / geometricMean
      : null;
  const weakLinkCap = weakest === null ? null : weakest.score * WEAK_LINK_CAP_FACTOR;

  // A relative tolerance, not an absolute one: these are IEEE doubles that have
  // been through JSON, and the comparison has to hold at 80 and at 8000 alike.
  const totalIsAtTheWeakLinkCap =
    total !== null &&
    weakLinkCap !== null &&
    Math.abs(total - weakLinkCap) <= Math.abs(weakLinkCap) * 1e-9;

  return { scoredCategories, weakest, strongest, balanceIndex, weakLinkCap, totalIsAtTheWeakLinkCap };
}

/**
 * A word for the index, because a bare 0.41 means nothing on first reading.
 *
 * These thresholds are presentation only. They are not model parameters, they
 * change no score, and they were picked to match the language the scoring
 * document already uses about uneven machines - not fitted to anything.
 */
function describeBalance(balanceIndex: number): string {
  if (balanceIndex >= 0.85) return 'even';
  if (balanceIndex >= 0.6) return 'uneven';
  if (balanceIndex >= 0.35) return 'lopsided';
  return 'one subsystem far behind the rest';
}

/** Ring spacings, in score points. Round numbers only - a web ruled every 375
 *  is a web nobody can read a vertex off. */
const RING_STEP_CANDIDATES = [100, 250, 500, 1000, 2500, 5000, 10000];
const MAXIMUM_RINGS = 5;

/**
 * The scale: ring spacing first, outer edge second.
 *
 * Choosing the step and letting the maximum follow keeps every ring on a round
 * number at the cost of a little unused radius, which is the right trade for a
 * chart people read positions off by eye.
 *
 * The peak is floored at the reference anchor so the 1000 ring is always on the
 * chart. A radar whose outer edge was the best category on this machine would
 * silently redefine "full marks" as "as good as this machine's best part" -
 * the rolling-maximum mistake the fixed reference exists to avoid.
 */
function scaleFor(scores: number[]): { axisMaximum: number; ringStep: number } {
  const peak = scores.reduce((highest, score) => Math.max(highest, score), REFERENCE_ANCHOR);
  const ringStep =
    RING_STEP_CANDIDATES.find((step) => peak / step <= MAXIMUM_RINGS) ??
    Math.ceil(peak / MAXIMUM_RINGS);
  return { axisMaximum: Math.ceil(peak / ringStep) * ringStep, ringStep };
}

// Geometry in user units. The viewBox is fixed and the SVG scales to its
// column, so these are ratios as much as pixels: the label ring has to clear
// the polygon, and the box has to clear the labels.
const VIEW_WIDTH = 380;
const VIEW_HEIGHT = 286;
const CENTRE_X = 190;
const CENTRE_Y = 138;
const OUTER_RADIUS = 88;
const LABEL_RADIUS = 104;

interface RadarAxis {
  readonly key: string;
  readonly label: string;
  readonly score: number;
  readonly vertexX: number;
  readonly vertexY: number;
  readonly spokeX: number;
  readonly spokeY: number;
  readonly labelX: number;
  readonly labelY: number;
  readonly textAnchor: 'start' | 'middle' | 'end';
}

interface RadarGeometry {
  readonly axes: RadarAxis[];
  readonly axisMaximum: number;
  readonly ringStep: number;
  readonly ringPolygons: string[];
  readonly referenceRing: string | null;
  readonly shapePolygon: string;
}

function buildGeometry(scoredCategories: CategoryScore[]): RadarGeometry {
  const { axisMaximum, ringStep } = scaleFor(scoredCategories.map((category) => category.score));
  const count = scoredCategories.length;
  const ringCount = Math.round(axisMaximum / ringStep);

  const pointsAtRadius = (radius: number) =>
    scoredCategories
      .map((_, index) => {
        // Start at twelve o'clock and go clockwise, in the order the scoring
        // model emitted the categories. Axis order is arbitrary in a radar -
        // the shape it produces is an artefact of that order, not a property
        // of the machine - so it is at least kept stable and identical to the
        // order the rest of the dashboard already tiles them in.
        const angle = -Math.PI / 2 + (index * 2 * Math.PI) / count;
        return `${(CENTRE_X + Math.cos(angle) * radius).toFixed(1)},${(CENTRE_Y + Math.sin(angle) * radius).toFixed(1)}`;
      })
      .join(' ');

  const axes = scoredCategories.map((category, index) => {
    const angle = -Math.PI / 2 + (index * 2 * Math.PI) / count;
    const cos = Math.cos(angle);
    const sin = Math.sin(angle);
    // Clamped: a category above the outer ring would otherwise draw outside the
    // web, and `scaleFor` has already guaranteed it cannot happen. The clamp is
    // here so that a future change to the scale cannot produce a shape that
    // overflows its own frame.
    const fraction = Math.min(Math.max(category.score / axisMaximum, 0), 1);
    const horizontallyCentred = Math.abs(cos) < 0.35;
    return {
      key: category.key,
      label: category.label,
      score: category.score,
      vertexX: CENTRE_X + cos * OUTER_RADIUS * fraction,
      vertexY: CENTRE_Y + sin * OUTER_RADIUS * fraction,
      spokeX: CENTRE_X + cos * OUTER_RADIUS,
      spokeY: CENTRE_Y + sin * OUTER_RADIUS,
      labelX: CENTRE_X + cos * LABEL_RADIUS,
      // Two stacked lines per label, so lift the pair above a vertex in the top
      // half and drop it below one in the bottom half rather than letting the
      // text sit on the spoke it belongs to.
      labelY: CENTRE_Y + sin * LABEL_RADIUS + (sin < -0.2 ? -12 : sin > 0.2 ? 8 : 0),
      textAnchor: horizontallyCentred ? 'middle' : cos > 0 ? 'start' : 'end',
    } satisfies RadarAxis;
  });

  return {
    axes,
    axisMaximum,
    ringStep,
    ringPolygons: Array.from({ length: ringCount }, (_, index) =>
      pointsAtRadius((OUTER_RADIUS * (index + 1)) / ringCount),
    ),
    // The reference ring is drawn separately whenever the scale has grown past
    // it: on a machine scoring 3000 across the board, "where is 1000" is the
    // only fixed landmark on the chart.
    referenceRing:
      axisMaximum > REFERENCE_ANCHOR
        ? pointsAtRadius((OUTER_RADIUS * REFERENCE_ANCHOR) / axisMaximum)
        : null,
    shapePolygon: axes
      .map((axis) => `${axis.vertexX.toFixed(1)},${axis.vertexY.toFixed(1)}`)
      .join(' '),
  };
}

export interface CategoryBalancePanelProps {
  /** `RunView.categories` verbatim - facets are filtered out in here. */
  categories: CategoryScore[];
  /** `RunView.total`, used only to see whether it landed on the cap. */
  total: number | null;
}

/**
 * Category radar plus the balance index it visualises.
 *
 * `memo` is not an optimisation here, it is a correctness constraint. `App`
 * re-renders on every telemetry frame - once a second, for the whole run - and
 * the reducer hands back the same `categories` array identity until a score
 * event actually arrives. So the memo boundary means the trigonometry and the
 * SVG run twice per run rather than once per second, and the dashboard does not
 * become part of what it is measuring. `useMemo` inside covers the same ground
 * for the case where a parent passes a fresh array.
 */
export const CategoryBalancePanel = memo(function CategoryBalancePanel({
  categories,
  total,
}: CategoryBalancePanelProps) {
  const balance = useMemo(() => deriveCategoryBalance(categories, total), [categories, total]);
  const drawable = balance.scoredCategories.length >= MINIMUM_AXES_FOR_A_POLYGON;
  const balanceDescribesSomething = balance.scoredCategories.length >= 2;
  const geometry = useMemo(
    () => (drawable ? buildGeometry(balance.scoredCategories) : null),
    [drawable, balance.scoredCategories],
  );

  // Before the first score event there is no shape and no balance to discuss.
  // An empty panel with a heading would read as a thing that failed rather than
  // a thing that has not happened yet.
  if (balance.scoredCategories.length === 0) return null;

  // The text equivalent of the chart, and more than that: ranked, where the
  // polygon is drawn in the model's own category order. Sorting is what makes
  // this worth reading beside the picture rather than a second printing of it,
  // and it is the order a reader wants anyway - the question a radar provokes
  // is "which part is holding this machine back", and the answer is the last
  // row. Every category on an axis appears here, so nothing is lost when the
  // chart is not seen.
  const ranked = [...balance.scoredCategories].sort((a, b) => b.score - a.score);
  const readout = (
    <>
      <p className="tile-label">Categories, strongest first</p>
      <dl className="kv">
        {ranked.map((category) => (
          <div key={category.key}>
            <dt>{category.label}</dt>
            <dd>
              {Math.round(category.score)}
              {balance.weakest?.key === category.key && balance.scoredCategories.length > 1 && (
                <span className="pill">weakest</span>
              )}
            </dd>
          </div>
        ))}
      </dl>
    </>
  );

  return (
    <section className="panel" aria-labelledby="balance-heading">
      <h2 id="balance-heading">Category balance</h2>

      {balance.totalIsAtTheWeakLinkCap && balance.weakest && balance.weakLinkCap !== null && (
        // Stated whenever the total is sitting on the ceiling, in the same
        // terms the HTML report uses. A cap that quietly took points off a
        // machine would be worse than no cap: the reader has to be able to see
        // that the number in front of them is the weakest subsystem's number,
        // not the aggregate's.
        <div className="banner" role="note">
          <strong>Weak-link cap applied.</strong> The total is not the aggregate of these
          categories — it is capped at {WEAK_LINK_CAP_FACTOR}× the weakest one,{' '}
          {balance.weakest.label} at {Math.round(balance.weakest.score)}, giving{' '}
          {Math.round(balance.weakLinkCap)}. A machine is claimed to be at most four times as good
          as its worst measured part, because a subsystem several times slower than the rest
          dominates end-to-end time regardless of how fast everything else is.
        </div>
      )}

      <div className="radar-layout">
        {geometry ? (
          <div className="radar-figure">
            {/*
              Decorative, exactly as `Sparkline` is: every category name and
              score inside this SVG is repeated as text in the readout beside
              it, so hiding it from assistive technology loses no information
              and spares a screen reader a pile of unordered coordinates.
            */}
            <svg
              className="radar"
              viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`}
              aria-hidden="true"
              focusable="false"
            >
              {geometry.ringPolygons.map((points, index) => (
                <polygon key={`ring-${index}`} className="radar-web" points={points} />
              ))}
              {geometry.referenceRing && (
                <polygon className="radar-web radar-web-reference" points={geometry.referenceRing} />
              )}
              {geometry.axes.map((axis) => (
                <line
                  key={`spoke-${axis.key}`}
                  className="radar-spoke"
                  x1={CENTRE_X}
                  y1={CENTRE_Y}
                  x2={axis.spokeX.toFixed(1)}
                  y2={axis.spokeY.toFixed(1)}
                />
              ))}
              <polygon className="radar-shape" points={geometry.shapePolygon} />
              {geometry.axes.map((axis) => (
                <circle
                  key={`vertex-${axis.key}`}
                  className="radar-vertex"
                  cx={axis.vertexX.toFixed(1)}
                  cy={axis.vertexY.toFixed(1)}
                  r="2.5"
                />
              ))}
              {geometry.axes.map((axis) => (
                <text
                  key={`label-${axis.key}`}
                  className="radar-axis-label"
                  x={axis.labelX.toFixed(1)}
                  y={axis.labelY.toFixed(1)}
                  textAnchor={axis.textAnchor}
                >
                  {axis.label}
                  <tspan className="radar-axis-score" x={axis.labelX.toFixed(1)} dy="1.25em">
                    {Math.round(axis.score)}
                  </tspan>
                </text>
              ))}
            </svg>
            <p className="muted">
              Rings every {geometry.ringStep}, outermost {geometry.axisMaximum}
              {geometry.referenceRing
                ? `; the dashed ring is ${REFERENCE_ANCHOR}, the DARC-REF-1 reference.`
                : `, which is the DARC-REF-1 reference.`}
            </p>
          </div>
        ) : (
          <p className="muted">
            No shape drawn: {balance.scoredCategories.length} scored{' '}
            {balance.scoredCategories.length === 1 ? 'category' : 'categories'}. A polygon needs at
            least {MINIMUM_AXES_FOR_A_POLYGON} axes, and a two-sided one would be a line whose angle
            carried no meaning. The scores below are the whole finding.
          </p>
        )}

        <div className="radar-readout">
          {readout}
          {balance.balanceIndex === null ? (
            <p className="muted">
              Balance index unavailable: it is the weakest category divided by the geometric mean of
              all of them, and at least one category has no positive score to divide by.
            </p>
          ) : (
            <>
              <p className="balance-index">
                <span className="tile-label">Balance index</span>
                <span className="balance-value">{balance.balanceIndex.toFixed(2)}</span>
                {/* The word is withheld below two categories rather than shown
                    as "even": with one score the index is that score divided
                    by itself, and calling that balanced would be the UI
                    inventing a finding out of arithmetic. */}
                {balanceDescribesSomething && (
                  <span className="balance-word">{describeBalance(balance.balanceIndex)}</span>
                )}
              </p>
              <p className="muted">
                Weakest category ÷ geometric mean of all of them; 1.00 is perfectly balanced.
                {balanceDescribesSomething
                  ? ` Computed in the browser from the ${balance.scoredCategories.length} category scores above — the authoritative value is in the result bundle.`
                  : ' With one scored category it is that score divided by itself, so it is 1.00 by construction and says nothing about this machine. It becomes informative once a second category is measured.'}
              </p>
            </>
          )}
          {/* Only claimed against a total we have actually been sent. Before
              the first score event there is no aggregate to compare with the
              ceiling, and "not binding" would be a statement about nothing. */}
          {total !== null && !balance.totalIsAtTheWeakLinkCap && balance.weakLinkCap !== null && (
            <p className="muted">
              The weak-link cap is not binding: the total is below {WEAK_LINK_CAP_FACTOR}× the
              weakest category ({Math.round(balance.weakLinkCap)}), so nothing has been deducted for
              imbalance.
            </p>
          )}
        </div>
      </div>

      <p className="muted">
        The shape describes this run's category scores relative to each other and to the reference
        anchor. Those scores are uncalibrated, so a shape from another machine or another build is
        not something to lay over this one — and a category that was never measured has no axis
        here at all, rather than an axis at zero.
      </p>
    </section>
  );
});
