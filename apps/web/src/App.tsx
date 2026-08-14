import { useCallback, useEffect, useState } from 'react';
import { api, ApiError, bootstrapToken, scrubUrl } from './api';
import {
  Brand,
  CalibrationBanner,
  EventLog,
  MetricTable,
  RiskPanel,
  ScoreBoard,
  Telemetry,
} from './components';
import { RunComparisonPanel } from './comparison';
import { CategoryBalancePanel } from './radar';
import type { AgentMeta, Inventory, ProfileInfo } from './types';
import { useRunStream } from './useRunStream';

type Phase = 'connecting' | 'ready' | 'error';

export default function App() {
  const [phase, setPhase] = useState<Phase>('connecting');
  const [error, setError] = useState<string | null>(null);
  const [meta, setMeta] = useState<AgentMeta | null>(null);
  const [inventory, setInventory] = useState<Inventory | null>(null);
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [selected, setSelected] = useState<string>('quick');
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);

  const { view, metrics, connect } = useRunStream();

  useEffect(() => {
    let cancelled = false;

    (async () => {
      try {
        const token = bootstrapToken();
        if (token) {
          await api.openSession(token);
          // Only strip the URL once the session cookie is actually set;
          // otherwise a failed exchange would leave no way back in.
          scrubUrl();
        }
        const [agentMeta, profileList, inv] = await Promise.all([
          api.meta(),
          api.profiles(),
          api.inventory(),
        ]);
        if (cancelled) return;
        setMeta(agentMeta);
        setProfiles(profileList);
        setInventory(inv.inventory);
        const firstAvailable = profileList.find((p) => p.available);
        if (firstAvailable) setSelected(firstAvailable.key);
        setPhase('ready');
      } catch (caught) {
        if (cancelled) return;
        const message =
          caught instanceof ApiError && caught.status === 401
            ? 'Not authenticated. Open the URL the agent printed, including its ?token= parameter.'
            : caught instanceof Error
              ? caught.message
              : 'Could not reach the agent.';
        setError(message);
        setPhase('error');
      }
    })();

    return () => {
      cancelled = true;
    };
  }, []);

  const start = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const run = await api.startRun(selected, acknowledged);
      connect(run.run_id);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : 'Could not start the run.');
    } finally {
      setBusy(false);
    }
  }, [selected, acknowledged, connect]);

  const cancel = useCallback(async () => {
    if (!view.runId) return;
    setBusy(true);
    try {
      await api.cancelRun(view.runId);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : 'Could not cancel the run.');
    } finally {
      setBusy(false);
    }
  }, [view.runId]);

  const running = view.state === 'running' || view.state === 'preflight' || view.state === 'created';
  const finished = view.state === 'completed' || view.state === 'failed' || view.state === 'cancelled';

  if (phase === 'connecting') {
    return (
      <main className="shell">
        <Brand meta={null} />
        <p className="muted">Connecting to the agent…</p>
      </main>
    );
  }

  return (
    <main className="shell">
      <Brand meta={meta} />

      {meta && !meta.scoring_calibrated && <CalibrationBanner model={meta.scoring_model} />}

      {error && (
        <div className="banner banner-error" role="alert">
          {error}
        </div>
      )}

      {inventory && (
        <section className="panel" aria-labelledby="env-heading">
          <h2 id="env-heading">This machine</h2>
          <dl className="kv">
            <div>
              <dt>CPU</dt>
              <dd>{inventory.cpu.model ?? 'unknown'}</dd>
            </div>
            <div>
              <dt>Topology</dt>
              <dd>
                {inventory.cpu.logical_cpus} logical
                {inventory.cpu.physical_cores ? ` / ${inventory.cpu.physical_cores} physical` : ''}
              </dd>
            </div>
            <div>
              <dt>Memory</dt>
              <dd>{(inventory.memory.total_bytes / 1024 ** 3).toFixed(1)} GiB</dd>
            </div>
            <div>
              <dt>Scope</dt>
              <dd>
                {inventory.platform.scope}
                {inventory.platform.virtualization ? ` (${inventory.platform.virtualization})` : ''}
              </dd>
            </div>
            <div>
              <dt>Kernel</dt>
              <dd>{inventory.platform.kernel_release ?? 'unknown'}</dd>
            </div>
            <div>
              <dt>Governor</dt>
              <dd>{inventory.cpu.governor ?? 'not exposed'}</dd>
            </div>
          </dl>
          {inventory.platform.scope === 'Container' && (
            <p className="muted">
              Measurements will describe this container, not the host, and are labelled accordingly.
            </p>
          )}
          {inventory.software.production_signals.length > 0 && (
            <p className="muted">
              Production signals: {inventory.software.production_signals.join('; ')}
            </p>
          )}
        </section>
      )}

      <section className="panel" aria-labelledby="control-heading">
        <h2 id="control-heading">Run control</h2>
        <div className="controls">
          <label htmlFor="profile">Profile</label>
          <select
            id="profile"
            value={selected}
            disabled={running || busy}
            onChange={(event) => setSelected(event.target.value)}
          >
            {profiles.map((profile) => (
              <option key={profile.key} value={profile.key} disabled={!profile.available}>
                {profile.key} · {profile.nominal_minutes[0]}–{profile.nominal_minutes[1]} min
                {profile.available ? '' : ' · not implemented yet'}
              </option>
            ))}
          </select>

          <button className="primary" onClick={start} disabled={running || busy}>
            Start benchmark
          </button>
          <button className="danger" onClick={cancel} disabled={!running || busy}>
            Cancel
          </button>

          <span className={`status status-${view.state}`} role="status" aria-live="polite">
            {view.state}
            {running ? (view.connected ? ' · streaming' : ' · reconnecting') : ''}
          </span>
        </div>

        <label className="ack">
          <input
            type="checkbox"
            checked={acknowledged}
            disabled={running}
            onChange={(event) => setAcknowledged(event.target.checked)}
          />
          Proceed despite preflight warnings (this machine may be serving live traffic)
        </label>

        <progress value={view.progress} max={1} aria-label="Run progress" />
        {view.currentModule && (
          <p className="muted">
            {view.currentModule}
            {view.currentPhase ? ` · ${view.currentPhase}` : ''} · module {view.moduleIndex + 1} of{' '}
            {view.moduleTotal} · {Math.round(view.progress * 100)}%
          </p>
        )}

        {finished && view.runId && (
          <p className="artifacts">
            <a href={api.reportUrl(view.runId)} target="_blank" rel="noreferrer">
              Open HTML report
            </a>
            {' · '}
            <a href={api.bundleUrl(view.runId)} target="_blank" rel="noreferrer">
              Download JSON bundle
            </a>
            {view.bundleDigest && <span className="muted"> · {view.bundleDigest}</span>}
          </p>
        )}
      </section>

      <RiskPanel
        risk={view.risk}
        findings={view.findings}
        estimatedSeconds={view.estimatedSeconds}
      />

      <ScoreBoard
        total={view.total}
        categories={view.categories}
        provisional={!view.scoresFinal}
        verdict={view.verdict}
        // While this is true the scoreboard animates nothing: the machine
        // under the page is being measured, and the reveal is saved for the
        // moment it goes idle again.
        runActive={running}
      />

      {/* Immediately under the scores it decomposes, and above the live
          telemetry: the shape of the machine is a conclusion about the run,
          not something to watch while it happens. It renders nothing until a
          score event has arrived. */}
      <CategoryBalancePanel categories={view.categories} total={view.total} />

      <Telemetry latest={view.latest} history={view.telemetry} />

      <MetricTable metrics={metrics} />

      {/* Below this run's own measurements, because it is about a different
          question: not what this machine did, but what changed since last time.
          `finished ? view.runId : null` is deliberately a plain string - it
          changes once, when a run reaches a terminal state and the history
          gains a row - so the panel's `memo` boundary holds through the
          thousands of events before that and the comparison never re-renders on
          the telemetry stream. */}
      <RunComparisonPanel completedRunId={finished ? view.runId : null} />

      <EventLog lines={view.log} />

      <footer className="foot">
        DARCBench · Tombatossals Softworks LLC · raw measurements are reproducible from the
        result bundle; scores are derived and can be recomputed from it.
      </footer>
    </main>
  );
}
