/**
 * Presentation components for the command centre.
 *
 * Accessibility rules applied throughout:
 *  - No meaning conveyed by colour alone: every state also carries a word.
 *  - `aria-live` on the regions that update during a run.
 *  - Sparklines are decorative (`aria-hidden`) and always accompanied by the
 *    numeric value they depict.
 *  - Motion is opt-out via `prefers-reduced-motion` in the stylesheet.
 */

import { useEffect, useRef, useState } from 'react';

import type { CategoryScore, Metric, PreflightFinding, RiskClass } from './types';
import type { LogLine, TelemetryPoint } from './useRunStream';

/**
 * Whether this dashboard may spend anything on decoration right now.
 *
 * Two conditions, and the second is the unusual one:
 *
 *  - `prefers-reduced-motion` is honoured, as everywhere.
 *  - Animation is suppressed **while a run is in flight**. This page is open on
 *    the machine being measured. The stylesheet already refuses to animate the
 *    radar on exactly this ground - "CPU spent on decoration by a page whose
 *    whole job is not to disturb the machine under it" - and an eased score
 *    counter during the measurement window would be the same mistake with a
 *    nicer name. Once the run is over the machine is idle again, the numbers
 *    are final, and a reveal costs the operator nothing that matters.
 *
 * So the animation appears precisely where it is free, which is also where it
 * is useful: the moment a result lands.
 */
export function useMotionAllowed(runActive: boolean): boolean {
  const [reduced, setReduced] = useState(() =>
    typeof window === 'undefined'
      ? true
      : window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  );

  useEffect(() => {
    const query = window.matchMedia('(prefers-reduced-motion: reduce)');
    const onChange = () => setReduced(query.matches);
    query.addEventListener('change', onChange);
    return () => query.removeEventListener('change', onChange);
  }, []);

  return !reduced && !runActive;
}

/** Cubic ease-out: fast to start, settling rather than stopping. */
function easeOut(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

const COUNT_UP_MS = 620;

/**
 * Eases a number toward `target`, or snaps to it when motion is not allowed.
 *
 * The animation is driven by `requestAnimationFrame` and stops as soon as it
 * arrives - it is a one-shot reveal, not a loop, so an idle dashboard is doing
 * nothing at all rather than repainting forever.
 */
export function useCountUp(target: number | null, allowed: boolean): number | null {
  const [shown, setShown] = useState(target);
  const shownRef = useRef<number>(target ?? 0);

  useEffect(() => {
    if (target === null) {
      setShown(null);
      return;
    }
    if (!allowed) {
      shownRef.current = target;
      setShown(target);
      return;
    }

    const from = shownRef.current;
    if (from === target) return;

    let frame = 0;
    const start = performance.now();
    const step = (now: number) => {
      const progress = Math.min(1, (now - start) / COUNT_UP_MS);
      const value = from + (target - from) * easeOut(progress);
      shownRef.current = value;
      setShown(value);
      if (progress < 1) frame = requestAnimationFrame(step);
    };
    frame = requestAnimationFrame(step);
    return () => cancelAnimationFrame(frame);
  }, [target, allowed]);

  return shown;
}

export function Brand({ meta }: { meta: { agent_version: string; scoring_model: string } | null }) {
  return (
    <header className="brand-bar">
      <div>
        <h1 className="brand">DARC//BENCH</h1>
        <p className="brand-sub">Deployment · Application · Runtime · Compute</p>
      </div>
      {meta && (
        <dl className="brand-meta">
          <div>
            <dt>Agent</dt>
            <dd>{meta.agent_version}</dd>
          </div>
          <div>
            <dt>Scoring</dt>
            <dd>{meta.scoring_model}</dd>
          </div>
        </dl>
      )}
    </header>
  );
}

export function CalibrationBanner({ model }: { model: string }) {
  return (
    <div className="banner" role="note">
      <strong>Provisional scoring model.</strong> <code>{model}</code> has not been calibrated
      against a physical DARC-REF-1 reference machine — its reference values are declared targets,
      not measurements. The raw measurements below are real and reproducible; the scores derived
      from them are development output and are not comparable with any future calibrated release.
    </div>
  );
}

const RISK_COPY: Record<RiskClass, { label: string; tone: string; detail: string }> = {
  safe: { label: 'Safe', tone: 'ok', detail: 'Read-only workloads. No writes, no service impact.' },
  moderate_load: {
    label: 'Moderate load',
    tone: 'ok',
    detail: 'Writes temporary files or generates network traffic.',
  },
  heavy_load: {
    label: 'Heavy load',
    tone: 'warn',
    detail: 'Saturates the CPU for the duration of the run. Nothing is written to disk.',
  },
  production_risk: {
    label: 'Production risk',
    tone: 'bad',
    detail: 'This machine looks like it is serving live traffic. Running will degrade it.',
  },
  unsupported: { label: 'Unsupported', tone: 'bad', detail: 'This environment cannot be measured.' },
};

export function RiskPanel({
  risk,
  findings,
  estimatedSeconds,
}: {
  risk: RiskClass | null;
  findings: PreflightFinding[];
  estimatedSeconds: number | null;
}) {
  if (!risk) return null;
  const copy = RISK_COPY[risk];
  return (
    <section className="panel" aria-labelledby="risk-heading">
      <h2 id="risk-heading">Preflight</h2>
      <p className={`risk risk-${copy.tone}`}>
        <span className="risk-label">{copy.label}</span>
        {estimatedSeconds !== null && <span className="risk-eta"> · about {estimatedSeconds}s</span>}
      </p>
      <p className="muted">{copy.detail}</p>
      {findings.length > 0 && (
        <ul className="findings">
          {findings.map((finding, index) => (
            <li key={`${finding.check}-${index}`} className={`finding finding-${finding.severity}`}>
              <span className="finding-severity">{finding.severity.toUpperCase()}</span>
              <code>{finding.check}</code>
              <span>{finding.message}</span>
              {finding.blocking && <span className="finding-blocking">blocking</span>}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function ScoreBoard({
  total,
  categories,
  provisional,
  verdict,
  runActive = false,
}: {
  total: number | null;
  categories: CategoryScore[];
  provisional: boolean;
  verdict: string | null;
  runActive?: boolean;
}) {
  const motion = useMotionAllowed(runActive);
  const shownTotal = useCountUp(total, motion);

  return (
    <section className="panel" aria-labelledby="scores-heading" aria-live="polite">
      <h2 id="scores-heading">Scores</h2>
      <div className={`total-card${motion && total !== null ? ' is-revealed' : ''}`}>
        <div className="tile-label">
          DARCBench Total {provisional ? '(provisional)' : ''}
        </div>
        {/*
          The animated value is hidden from assistive technology and a plain,
          final number is exposed instead. A screen reader announcing an
          `aria-live` region that ticks through six hundred intermediate values
          is worse than useless, and the count-up is decoration by definition.
        */}
        <div className="total-value" aria-hidden="true">
          {shownTotal === null ? '—' : Math.round(shownTotal)}
        </div>
        <span className="sr-only">
          {total === null ? 'No total score yet' : `Total score ${Math.round(total)}`}
        </span>
        {verdict && <div className="muted">Result state: {verdict.replace(/_/g, ' ')}</div>}
      </div>
      <div className="tile-grid">
        {categories.map((category, index) => (
          <CategoryTile
            key={category.key}
            category={category}
            motion={motion}
            index={index}
          />
        ))}
        {categories.length === 0 && <p className="muted">No scores yet.</p>}
      </div>
    </section>
  );
}

function CategoryTile({
  category,
  motion,
  index,
}: {
  category: CategoryScore;
  motion: boolean;
  index: number;
}) {
  const shown = useCountUp(category.score, motion);
  return (
    <div
      className={`tile${motion ? ' is-revealed' : ''}`}
      // Staggered so the grid resolves left to right instead of flashing as one
      // block. Capped, because a machine with many categories should not make
      // the last tile wait half a second for its turn.
      style={motion ? { animationDelay: `${Math.min(index, 6) * 45}ms` } : undefined}
    >
      <div className="tile-label">{category.label}</div>
      <div className="tile-value" aria-hidden="true">
        {Math.round(shown ?? category.score)}
      </div>
      <span className="sr-only">{`${category.label} ${Math.round(category.score)}`}</span>
      {category.weight > 0 && (
        <div className="muted">weight {Math.round(category.weight * 100)}%</div>
      )}
    </div>
  );
}

/** A tiny inline sparkline. Decorative: the number is always shown beside it. */
function Sparkline({ points, max }: { points: number[]; max: number }) {
  if (points.length < 2) return null;
  const width = 100;
  const height = 24;
  const step = width / (points.length - 1);
  const path = points
    .map((value, index) => {
      const x = index * step;
      const y = height - Math.min(value / (max || 1), 1) * height;
      return `${index === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
  return (
    <svg className="spark" viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" aria-hidden="true">
      <path d={path} fill="none" stroke="currentColor" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

const gib = (bytes: number) => (bytes / 1024 ** 3).toFixed(1);

export function Telemetry({
  latest,
  history,
}: {
  latest: TelemetryPoint | null;
  history: TelemetryPoint[];
}) {
  return (
    <section className="panel" aria-labelledby="telemetry-heading" aria-live="off">
      <h2 id="telemetry-heading">Live telemetry</h2>
      <div className="tile-grid">
        <div className="tile">
          <div className="tile-label">CPU busy</div>
          <div className="tile-value">{latest ? `${latest.cpuBusy.toFixed(1)}%` : '—'}</div>
          <Sparkline points={history.map((p) => p.cpuBusy)} max={100} />
        </div>
        <div className="tile">
          {/* The tile that says whether the run is alone on the machine. CPU
              busy cannot answer that - a benchmark drives it to 100% by
              design - so this one carries the only load number an operator can
              act on while the run is in flight. */}
          <div className="tile-label">Other work</div>
          <div className={`tile-value ${latest && latest.cpuExternal >= 10 ? 'alert' : ''}`}>
            {latest ? `${latest.cpuExternal.toFixed(1)}%` : '—'}
          </div>
          <Sparkline
            points={history.map((p) => p.cpuExternal)}
            max={Math.max(20, ...history.map((p) => p.cpuExternal))}
          />
          {latest && latest.cpuExternal >= 10 && (
            <div className="muted">
              Something other than this benchmark is using the CPU. Sustained, it degrades the
              modules measured while it lasts.
            </div>
          )}
        </div>
        <div className="tile">
          <div className="tile-label">CPU steal</div>
          <div className={`tile-value ${latest && latest.cpuSteal > 1 ? 'alert' : ''}`}>
            {latest ? `${latest.cpuSteal.toFixed(2)}%` : '—'}
          </div>
          <Sparkline points={history.map((p) => p.cpuSteal)} max={Math.max(5, ...history.map((p) => p.cpuSteal))} />
          {latest && latest.cpuSteal > 1 && (
            <div className="muted">Hypervisor is taking CPU from this guest.</div>
          )}
        </div>
        <div className="tile">
          <div className="tile-label">Load 1m</div>
          <div className="tile-value">{latest ? latest.load1.toFixed(2) : '—'}</div>
          <Sparkline points={history.map((p) => p.load1)} max={Math.max(4, ...history.map((p) => p.load1))} />
        </div>
        <div className="tile">
          <div className="tile-label">Memory</div>
          <div className="tile-value">{latest ? `${gib(latest.memUsed)} GiB` : '—'}</div>
          {latest && <div className="muted">of {gib(latest.memTotal)} GiB</div>}
        </div>
        <div className="tile">
          <div className="tile-label">CPU clock</div>
          <div className="tile-value">{latest?.freqMhz ? `${Math.round(latest.freqMhz)} MHz` : 'n/a'}</div>
          <Sparkline
            points={history.map((p) => p.freqMhz ?? 0)}
            max={Math.max(1, ...history.map((p) => p.freqMhz ?? 0))}
          />
        </div>
        <div className="tile">
          <div className="tile-label">Temperature</div>
          <div className="tile-value">{latest?.tempC ? `${latest.tempC.toFixed(0)} °C` : 'n/a'}</div>
        </div>
      </div>
    </section>
  );
}

type AnyMetric = Metric | { key: string; value: number; unit: string };

function isFull(metric: AnyMetric): metric is Metric {
  return 'summary' in metric;
}

export function MetricTable({ metrics }: { metrics: AnyMetric[] }) {
  return (
    <section className="panel" aria-labelledby="metrics-heading">
      <h2 id="metrics-heading">Raw measurements</h2>
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th scope="col">Metric</th>
              <th scope="col" className="num">Median</th>
              <th scope="col">Unit</th>
              <th scope="col" className="num">n</th>
              <th scope="col" className="num">CV</th>
              <th scope="col" className="num">95% CI</th>
            </tr>
          </thead>
          <tbody>
            {metrics.length === 0 && (
              <tr>
                <td colSpan={6} className="muted">
                  No measurements yet.
                </td>
              </tr>
            )}
            {metrics.map((metric) => {
              const full = isFull(metric) ? metric : null;
              const cv = full?.summary.cv ?? null;
              return (
                <tr key={metric.key}>
                  <td>
                    <code>{metric.key}</code>
                    {!full && <span className="pill">live</span>}
                  </td>
                  <td className="num">{metric.value.toFixed(2)}</td>
                  <td>{metric.unit}</td>
                  <td className="num">{full?.summary.n ?? '—'}</td>
                  <td className={`num ${cv !== null && cv > 0.15 ? 'alert' : ''}`}>
                    {cv === null ? '—' : `${(cv * 100).toFixed(1)}%`}
                  </td>
                  <td className="num">
                    {full?.summary.ci95
                      ? `${full.summary.ci95[0].toFixed(1)} – ${full.summary.ci95[1].toFixed(1)}`
                      : '—'}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

export function EventLog({ lines }: { lines: LogLine[] }) {
  return (
    <section className="panel" aria-labelledby="log-heading">
      <h2 id="log-heading">Event log</h2>
      <div className="log" role="log" aria-live="polite">
        {lines.length === 0 && <p className="muted">Nothing yet.</p>}
        {lines.map((line) => (
          <div key={line.seq} className={`log-line log-${line.severity}`}>
            <span className="log-seq">{line.seq}</span>
            <span className="log-kind">{line.kind}</span>
            <span>{line.text}</span>
          </div>
        ))}
      </div>
    </section>
  );
}
