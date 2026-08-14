/**
 * Run-to-run comparison: two runs of this machine, lined up metric by metric.
 *
 * # Why this panel exists
 *
 * A single run answers "how fast is this machine". Almost every question an
 * operator actually has is a *difference*: is this kernel slower than the last
 * one, did the noisy neighbour move out, is the new instance type worth what it
 * costs. `docs/API.md` documents the endpoint that answers it and the CLI
 * already prints the answer; this is the same answer in the console, so it can
 * be reached without a shell on the machine under test.
 *
 * # Why it is fetched, not derived
 *
 * Nothing here comes out of the SSE reducer. The dashboard re-renders once a
 * second for the whole of a run, and a comparison recomputed on every telemetry
 * frame would make the browser a measurable load on the machine it is
 * measuring - the same reasoning that produced the coalescing in
 * `useRunStream.ts` and the `memo` boundary on the radar. So: one request when
 * the operator asks for one, a `memo` boundary whose only prop is a string that
 * changes at most twice per run, and no polling of any kind.
 *
 * # What it must never do
 *
 * Two failure modes are specific to comparisons and both are handled here as
 * first-class content rather than as edge cases:
 *
 *  - **Reading a changed measurement as a changed machine.** The agent labels a
 *    comparison `comparable: false` when the two runs disagree about the
 *    machine, the profile, the scoring model or the agent build. That label
 *    never withholds the comparison - comparing a run from before a kernel
 *    upgrade with one from after is a legitimate thing to do - so every reason
 *    is rendered above the numbers, where it is read before them.
 *  - **Describing a subset as if it were the whole.** Metrics the agent could
 *    not line up are named in `unmatched`, and are listed here for the same
 *    reason the agent bothers to name them: a comparison that quietly drops the
 *    module that failed looks complete while omitting the more interesting
 *    finding.
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api, ApiError } from './api';
import type { MetricDelta, RunComparison, RunListEntry, RunState } from './types';

/**
 * Run states that can appear in the picker.
 *
 * A comparison is answered from the run index, and a run enters the index when
 * its result bundle is written - so only a run that has finished can be one
 * side of one. `failed` and `cancelled` are offered alongside `completed`
 * deliberately: both still produce a bundle, both are indexed, and a partial
 * run is often exactly what an operator wants to look at. Their result state is
 * shown in the option itself so nobody picks one without seeing what it is.
 */
const COMPARABLE_STATES: readonly RunState[] = ['completed', 'failed', 'cancelled'];

/** A fetch that has not been asked for, is in flight, failed, or produced a value. */
type Loadable<T> =
  | { readonly status: 'idle' }
  | { readonly status: 'loading' }
  | { readonly status: 'failed'; readonly message: string }
  | { readonly status: 'ready'; readonly value: T };

/**
 * Turns a thrown value into something worth putting on screen.
 *
 * `unknown_run` gets its own sentence because the generic message ("one or both
 * runs are not in this agent's state directory") is true but leaves the reader
 * without the one fact that explains it: the index is written when the bundle
 * is, so a run still in flight has no row yet and a pruned run no longer has
 * one. Everything else keeps the agent's own wording - `request()` in `api.ts`
 * has already folded `detail` into it - because inventing UI copy for an error
 * we have not anticipated is how a console starts lying about causes.
 */
function describeFailure(caught: unknown, fallback: string): string {
  if (caught instanceof ApiError && caught.code === 'unknown_run') {
    return (
      'One or both of these runs are not in this agent’s state directory. A run is added to the ' +
      'index when its result bundle is written, so a run that is still in flight cannot be ' +
      'compared yet, and one whose bundle has been pruned can no longer be compared at all.'
    );
  }
  if (caught instanceof ApiError && caught.status === 401) {
    return 'Not authenticated. Open the URL the agent printed, including its ?token= parameter.';
  }
  return caught instanceof Error ? caught.message : fallback;
}

/**
 * A measurement, printed at a precision that suits its magnitude.
 *
 * One column carries a 0.41 ms fsync and a 12 400 MB/s sequential read. A fixed
 * two decimals renders the first as `0.41` and the second as `12400.00`, which
 * is four digits of noise on one row and a loss of everything interesting on
 * the other. This is presentation only: the value in the bundle is what is
 * true, and the ratio beside it is computed from the full-precision number.
 */
function formatMeasurement(value: number): string {
  const magnitude = Math.abs(value);
  if (magnitude !== 0 && (magnitude < 0.001 || magnitude >= 1e7)) return value.toExponential(2);
  if (magnitude >= 1000) return value.toFixed(0);
  if (magnitude >= 1) return value.toFixed(2);
  return value.toFixed(4);
}

function formatTimestamp(iso: string | null | undefined): string {
  if (!iso) return 'unknown';
  const parsed = new Date(iso);
  return Number.isNaN(parsed.getTime()) ? iso : parsed.toLocaleString();
}

/**
 * Enough of a run id to tell two runs apart in a dropdown.
 *
 * Twelve characters is `run_` plus eight hex digits. The full 32-hex id is
 * always shown against the comparison itself, so this is a label, never an
 * identifier: nothing is ever selected or requested by the short form.
 */
function shortRunId(runId: string): string {
  return runId.length > 12 ? `${runId.slice(0, 12)}…` : runId;
}

/**
 * One line of the picker.
 *
 * Carries the run's result state as well as its score, because `invalid` and
 * `partial` runs are offered here on purpose and a comparison against one of
 * them means something different. Choosing a run without being told what kind
 * of run it is would be the picker withholding the one fact that changes how
 * its answer should be read.
 */
function pickerLabel(run: RunListEntry): string {
  const when = formatTimestamp(run.finished_at ?? run.created_at);
  const score =
    run.total_score === null || run.total_score === undefined
      ? 'no score'
      : `${Math.round(run.total_score)}`;
  const state = (run.result_state ?? run.state).replace(/_/g, ' ');
  return `${shortRunId(run.run_id)} · ${run.profile} · ${when} · ${score} · ${state}`;
}

interface ChangeReading {
  /** Signed percentage change, rounded to exactly the precision it is shown at. */
  readonly percent: number;
  /** `+12.4%`, `-8.1%`, `0.0%`. */
  readonly text: string;
  readonly word: 'better' | 'worse' | 'no change';
  readonly toneClass: string;
}

/**
 * A ratio, read as a percentage change.
 *
 * Rendered as a percentage rather than as a bare ratio because "+12%" is read
 * correctly by everyone and "1.12" is not - the same call the CLI's `compare`
 * makes, and deliberately the same arithmetic, so the two agree to the digit.
 *
 * The direction is the agent's, not ours. `ratio` arrives direction-adjusted:
 * above 1.0 means the candidate did better whether the metric counts throughput
 * or latency. Nothing here inspects the metric to decide which way is up,
 * because that decision has already been made once, correctly, by the code that
 * knows each metric's declared direction.
 *
 * The word is derived from the *rounded* percentage rather than from the raw
 * ratio, so a change that displays as `0.0%` is never captioned "better". A
 * sixth-decimal-place difference between two runs of a benchmark is not a
 * finding, and printing it as one would be the UI manufacturing a signal out of
 * float noise.
 */
function readChange(ratio: number): ChangeReading {
  const percent = Math.round((ratio - 1) * 1000) / 10;
  if (percent > 0) {
    return { percent, text: `+${percent.toFixed(1)}%`, word: 'better', toneClass: 'delta-better' };
  }
  if (percent < 0) {
    return { percent, text: `${percent.toFixed(1)}%`, word: 'worse', toneClass: 'delta-worse' };
  }
  return { percent, text: '0.0%', word: 'no change', toneClass: 'delta-flat' };
}

/**
 * The change cell: number, then word.
 *
 * The word is not decoration and it is not a tooltip. Colour on this cell is
 * redundant encoding - a regression has to be readable in monochrome, in print,
 * and to a reader who cannot distinguish `--ok` from `--bad` - so the sign and
 * the word carry the meaning and the colour only reinforces it.
 */
function ChangeCell({ reading }: { reading: ChangeReading }) {
  return (
    <span className={`delta ${reading.toneClass}`}>
      <span className="delta-value">{reading.text}</span>
      <span className="delta-word">{reading.word}</span>
    </span>
  );
}

interface ComparisonTally {
  readonly rows: readonly (MetricDelta & { readonly reading: ChangeReading })[];
  readonly better: number;
  readonly worse: number;
  readonly unchanged: number;
}

/**
 * Reads every ratio once, and orders the table worst change first.
 *
 * The agent returns the metrics in key order, which is the right order for
 * looking one of them up and the wrong order for the question a comparison is
 * opened to answer. "What got worse" is why anyone runs this, so the largest
 * regression is the first row; the caption says so, because an order that
 * changes meaning silently is worse than an arbitrary one.
 */
function tally(metrics: MetricDelta[]): ComparisonTally {
  const rows = metrics
    .map((metric) => ({ ...metric, reading: readChange(metric.ratio) }))
    .sort((a, b) => a.reading.percent - b.reading.percent);
  return {
    rows,
    better: rows.filter((row) => row.reading.word === 'better').length,
    worse: rows.filter((row) => row.reading.word === 'worse').length,
    unchanged: rows.filter((row) => row.reading.word === 'no change').length,
  };
}

/** The tile describing the total score, when there is one to describe. */
function TotalChangeTile({ comparison }: { comparison: RunComparison }) {
  const baseline = comparison.baseline.total_score;
  const candidate = comparison.candidate.total_score;

  // A total is missing whenever a run produced no score - an invalid run, or one
  // whose modules all failed. Saying so is the finding; printing a change
  // against nothing would not be.
  if (baseline === null || candidate === null || baseline <= 0) {
    return (
      <div className="tile">
        <div className="tile-label">Total score change</div>
        <div className="tile-value">—</div>
        <div className="muted">
          At least one of these runs has no positive total score, so there is no change to state.
          The metric rows below are unaffected: they are raw measurements and do not depend on the
          scoring model having produced a total.
        </div>
      </div>
    );
  }

  // The total is higher-is-better by construction, so the quotient needs no
  // direction adjustment - unlike the metric ratios, which arrive already
  // adjusted from the agent.
  const reading = readChange(candidate / baseline);
  return (
    <div className="tile">
      <div className="tile-label">Total score change</div>
      <div className={`tile-value ${reading.toneClass}`}>{reading.text}</div>
      <div className="muted">
        {reading.word}
        {comparison.comparable
          ? ''
          : ' — these runs are labelled not directly comparable above, and a score difference is' +
            ' the first thing that becomes meaningless when they are.'}
      </div>
    </div>
  );
}

function RunTile({
  role,
  runId,
  profile,
  finishedAt,
  totalScore,
}: {
  role: string;
  runId: string;
  profile: string;
  finishedAt: string;
  totalScore: number | null;
}) {
  return (
    <div className="tile">
      <div className="tile-label">{role}</div>
      <div className="tile-value">{totalScore === null ? '—' : Math.round(totalScore)}</div>
      <div className="muted">
        {profile} · {formatTimestamp(finishedAt)}
      </div>
      {/* The full id, not the short one: this is the value that identifies the
          run in the bundle, in the CLI and in a bug report. */}
      <div className="muted">
        <code>{runId}</code>
      </div>
    </div>
  );
}

function ComparisonResult({ comparison }: { comparison: RunComparison }) {
  const { rows, better, worse, unchanged } = useMemo(
    () => tally(comparison.metrics),
    [comparison.metrics],
  );

  return (
    <>
      {/* First, above everything it qualifies. A reader who scrolls to the
          table and stops must not be able to miss the reason the numbers in it
          are not what they look like. */}
      {comparison.incomparable_reasons.length > 0 ? (
        <div className="banner" role="note">
          <strong>Not directly comparable.</strong> The comparison below is still real — these are
          the measurements both runs produced — but part of the difference is the difference between
          the runs themselves, not between the machine at two points in time:
          <ul className="reasons">
            {comparison.incomparable_reasons.map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
        </div>
      ) : (
        <p className="muted">
          Same machine, profile, scoring model and agent build: the agent found nothing that would
          make these two runs disagree for a reason other than the machine.
        </p>
      )}

      <div className="tile-grid">
        <RunTile
          role="Baseline total"
          runId={comparison.baseline.run_id}
          profile={comparison.baseline.profile}
          finishedAt={comparison.baseline.finished_at}
          totalScore={comparison.baseline.total_score}
        />
        <RunTile
          role="Candidate total"
          runId={comparison.candidate.run_id}
          profile={comparison.candidate.profile}
          finishedAt={comparison.candidate.finished_at}
          totalScore={comparison.candidate.total_score}
        />
        <TotalChangeTile comparison={comparison} />
      </div>

      <p className="muted">
        Change is <strong>direction-adjusted</strong>: a positive change always means the candidate
        did better, whichever way the metric runs. A throughput that rose and a latency that fell
        both read as a gain, so no row has to be reinterpreted against the meaning of its unit.
        {rows.length > 0 && (
          <>
            {' '}
            {rows.length} {rows.length === 1 ? 'metric' : 'metrics'} lined up — {better} better,{' '}
            {worse} worse, {unchanged} unchanged — largest regression first.
          </>
        )}
      </p>

      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th scope="col">Metric</th>
              <th scope="col" className="num">
                Baseline
              </th>
              <th scope="col" className="num">
                Candidate
              </th>
              <th scope="col" className="num">
                Change
              </th>
              <th scope="col">Unit</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="muted">
                  No metric appears in both runs as a positive measurement, so there is nothing to
                  line up.
                  {comparison.unmatched.length > 0
                    ? ' Every metric either run did produce is named below, under “Not compared”.'
                    : ' Neither run left any metric in the index, which usually means both bundles' +
                      ' were written by an agent that did not index metrics, or that every module' +
                      ' failed.'}
                </td>
              </tr>
            )}
            {rows.map((row) => (
              <tr key={`${row.module}/${row.metric_key}`}>
                <td>
                  <code>
                    {row.module}/{row.metric_key}
                  </code>
                </td>
                <td className="num">{formatMeasurement(row.baseline)}</td>
                <td className="num">{formatMeasurement(row.candidate)}</td>
                <td className="num">
                  <ChangeCell reading={row.reading} />
                </td>
                <td>{row.unit}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {comparison.unmatched.length > 0 && (
        <div className="unmatched">
          <p className="tile-label">
            Not compared · {comparison.unmatched.length}{' '}
            {comparison.unmatched.length === 1 ? 'metric' : 'metrics'}
          </p>
          <ul className="reasons">
            {comparison.unmatched.map((entry) => (
              <li key={entry}>
                <code>{entry}</code>
              </li>
            ))}
          </ul>
          <p className="muted">
            These are named rather than dropped. A metric present in one run only usually means a
            module failed, was not in the other run’s profile, or changed its metric set between
            agent versions — which is often a more interesting finding than any row in the table
            above.
          </p>
        </div>
      )}

      <p className="muted">
        Only the first cycle of each run is compared. A cycling profile’s later cycles measure a
        machine that has already been under load for an hour, so lining cycle 7 of one run up
        against cycle 2 of another would compare two different questions; what an endurance run
        retained is its own published number.
      </p>
    </>
  );
}

export interface RunComparisonPanelProps {
  /**
   * The id of a run that has just reached a terminal state, or `null`.
   *
   * This is the panel's only input, and it is a plain string on purpose: it is
   * what makes the `memo` below hold through a run. It changes when a run
   * finishes, which is exactly when the history has gained a row worth
   * refetching, and not once during the thousands of events before that.
   */
  completedRunId: string | null;
}

/**
 * History and comparison.
 *
 * `memo` is a correctness constraint rather than an optimisation, for the same
 * reason it is on the radar: `App` re-renders on every telemetry frame, and
 * this panel holds fetched state that has nothing to do with the live run.
 * Re-rendering it at 1 Hz would be pure overhead on the machine under test.
 */
export const RunComparisonPanel = memo(function RunComparisonPanel({
  completedRunId,
}: RunComparisonPanelProps) {
  const [runs, setRuns] = useState<Loadable<RunListEntry[]>>({ status: 'loading' });
  const [baselineId, setBaselineId] = useState('');
  const [candidateId, setCandidateId] = useState('');
  const [comparison, setComparison] = useState<Loadable<RunComparison>>({ status: 'idle' });
  const [reloads, setReloads] = useState(0);

  /**
   * Drops the answer to a request the operator has already moved on from.
   *
   * Two selects and a button can outrun a request on a busy machine, and a
   * late-arriving response for the previous pair would render a comparison of
   * runs nobody has selected any more.
   */
  const requestTicket = useRef(0);

  useEffect(() => {
    let cancelled = false;
    setRuns({ status: 'loading' });
    (async () => {
      try {
        const list = await api.runs();
        if (cancelled) return;
        setRuns({ status: 'ready', value: list });
      } catch (caught) {
        if (cancelled) return;
        setRuns({
          status: 'failed',
          message: describeFailure(caught, 'Could not load the run history.'),
        });
      }
    })();
    return () => {
      cancelled = true;
    };
    // Refetched when a run finishes - the history has just gained a row - and
    // when the operator asks. Never on a timer: the list is not live data and
    // polling it would be the console spending the machine's CPU on itself.
  }, [completedRunId, reloads]);

  // Newest first, by the time the run ended where there is one. A run still in
  // flight has no `finished_at`, so it falls back to when it was created, which
  // keeps it at the top where it belongs.
  const selectable = useMemo(() => {
    if (runs.status !== 'ready') return [];
    const endedAt = (run: RunListEntry) => Date.parse(run.finished_at ?? run.created_at) || 0;
    return runs.value
      .filter((run) => COMPARABLE_STATES.includes(run.state))
      .sort((left, right) => endedAt(right) - endedAt(left));
  }, [runs]);

  // Default to the two most recent runs, candidate newest: "did the thing I
  // just did make it worse" is the question this panel is opened with. A
  // selection the operator has already made survives a refresh, as long as the
  // run it names is still in the list.
  useEffect(() => {
    if (selectable.length === 0) return;
    const present = (id: string) => selectable.some((run) => run.run_id === id);
    setCandidateId((current) => (present(current) ? current : (selectable[0]?.run_id ?? '')));
    setBaselineId((current) =>
      present(current) ? current : (selectable[1]?.run_id ?? selectable[0]?.run_id ?? ''),
    );
  }, [selectable]);

  // A displayed comparison belongs to the pair that produced it, so changing
  // either side clears it rather than leaving numbers under freshly changed
  // labels. Bumping the ticket in the same place means an in-flight request for
  // the old pair is discarded when it lands.
  useEffect(() => {
    requestTicket.current += 1;
    setComparison({ status: 'idle' });
  }, [baselineId, candidateId]);

  const compare = useCallback(async () => {
    if (!baselineId || !candidateId || baselineId === candidateId) return;
    const ticket = (requestTicket.current += 1);
    setComparison({ status: 'loading' });
    try {
      const result = await api.compare(baselineId, candidateId);
      if (ticket !== requestTicket.current) return;
      setComparison({ status: 'ready', value: result });
    } catch (caught) {
      if (ticket !== requestTicket.current) return;
      setComparison({
        status: 'failed',
        message: describeFailure(caught, 'Could not compare these two runs.'),
      });
    }
  }, [baselineId, candidateId]);

  // Re-reads the list on demand. Worth a control of its own because the history
  // includes runs from *other* agent processes: a run finished in a terminal
  // next to this browser tab exists on disk without anything having happened in
  // this page for the panel to notice.
  const reload = useCallback(() => setReloads((count) => count + 1), []);

  const sameRunSelected = baselineId !== '' && baselineId === candidateId;
  const canCompare = baselineId !== '' && candidateId !== '' && !sameRunSelected;

  return (
    <section className="panel" aria-labelledby="comparison-heading">
      <h2 id="comparison-heading">History and comparison</h2>

      {runs.status === 'loading' && <p className="muted">Reading the run history…</p>}

      {runs.status === 'failed' && (
        <>
          <div className="banner banner-error" role="alert">
            <strong>The run history could not be read.</strong> {runs.message}
          </div>
          <button onClick={reload}>Try again</button>
        </>
      )}

      {runs.status === 'ready' && selectable.length === 0 && (
        <>
          <p className="muted">
            No finished runs recorded yet. A run joins the history when its result bundle is
            written, so the first comparison becomes possible after two runs have completed on this
            machine.
          </p>
          <button onClick={reload}>Refresh history</button>
        </>
      )}

      {runs.status === 'ready' && selectable.length === 1 && (
        <>
          <p className="muted">
            One finished run recorded, <code>{selectable[0]?.run_id}</code>, and a comparison needs
            two. Run the same profile again — after a change worth measuring — and this panel will
            line the two up metric by metric.
          </p>
          <button onClick={reload}>Refresh history</button>
        </>
      )}

      {selectable.length >= 2 && (
        <>
          <div className="controls">
            <label htmlFor="comparison-baseline">Baseline</label>
            <select
              id="comparison-baseline"
              value={baselineId}
              onChange={(event) => setBaselineId(event.target.value)}
            >
              {selectable.map((run) => (
                <option key={run.run_id} value={run.run_id}>
                  {pickerLabel(run)}
                </option>
              ))}
            </select>

            <label htmlFor="comparison-candidate">Candidate</label>
            <select
              id="comparison-candidate"
              value={candidateId}
              onChange={(event) => setCandidateId(event.target.value)}
            >
              {selectable.map((run) => (
                <option key={run.run_id} value={run.run_id}>
                  {pickerLabel(run)}
                </option>
              ))}
            </select>

            <button className="primary" onClick={compare} disabled={!canCompare}>
              Compare
            </button>
            <button onClick={reload}>Refresh history</button>

            {/* Not a spinner. The state of the request is a sentence, in a live
                region, which a screen reader announces and a still image
                records. */}
            <span className="status" role="status" aria-live="polite">
              {comparison.status === 'loading'
                ? 'comparing…'
                : comparison.status === 'ready'
                  ? `${comparison.value.metrics.length} ${
                      comparison.value.metrics.length === 1 ? 'metric' : 'metrics'
                    } compared`
                  : comparison.status === 'failed'
                    ? 'comparison failed'
                    : 'not compared yet'}
            </span>
          </div>

          {sameRunSelected && (
            <p className="muted">
              Baseline and candidate are the same run. Every metric would compare to itself and
              report no change, which measures nothing — pick two different runs.
            </p>
          )}

          {comparison.status === 'idle' && !sameRunSelected && (
            <p className="muted">
              Choose a baseline and a candidate, then compare. The comparison is fetched when you
              ask for it rather than kept up to date in the background, because this console runs on
              the machine being measured and should cost it as close to nothing as possible.
            </p>
          )}

          {comparison.status === 'failed' && (
            <div className="banner banner-error" role="alert">
              <strong>These two runs could not be compared.</strong> {comparison.message}
            </div>
          )}

          {comparison.status === 'ready' && <ComparisonResult comparison={comparison.value} />}
        </>
      )}
    </section>
  );
});
