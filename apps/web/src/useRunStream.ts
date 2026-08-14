/**
 * Subscribes to a run's SSE stream and folds it into render state.
 *
 * # Why a reducer over a pile of `useState`
 *
 * The event stream is the source of truth and it is ordered. Folding it in one
 * place means a reconnect that replays events produces exactly the same state
 * as the original live delivery - there is no ordering-dependent side effect
 * hiding in a component.
 *
 * # Observer overhead
 *
 * A deep run emits thousands of `module.sample` events. Re-rendering the whole
 * dashboard for each one would make the browser a measurable load on the
 * machine being benchmarked, which would corrupt the very numbers on screen.
 * So: samples are coalesced into a per-metric latest value, telemetry is kept
 * to a bounded ring buffer, and the log is capped.
 */

import { useCallback, useEffect, useMemo, useReducer, useRef } from 'react';
import { api } from './api';
import { EVENT_KINDS } from './types';
import type {
  CategoryScore,
  DarcEvent,
  Metric,
  ModuleResult,
  PreflightFinding,
  ResultState,
  RiskClass,
  RunState,
} from './types';

/** Telemetry points retained for the sparklines. At 1 Hz this is ~3 minutes. */
const TELEMETRY_WINDOW = 180;
const LOG_LIMIT = 300;

/**
 * How long incoming events are pooled before one render.
 *
 * Caps re-renders at ~10/s however fast events arrive. Chosen rather than
 * `requestAnimationFrame` because rAF does not fire in a background tab: an
 * operator who switches away mid-run would accumulate an unbounded queue and
 * then pay for all of it at once on return. A timer keeps running, throttled,
 * and `MAX_PENDING` bounds the queue either way.
 */
const FLUSH_MS = 100;

/** Flush immediately past this many pooled events, whatever the timer is doing. */
const MAX_PENDING = 250;

export interface TelemetryPoint {
  monoMs: number;
  cpuBusy: number;
  /** Busy CPU the agent process did not consume: competition for the measurement. */
  cpuExternal: number;
  cpuSteal: number;
  load1: number;
  memUsed: number;
  memTotal: number;
  freqMhz: number | null;
  tempC: number | null;
}

export interface LogLine {
  seq: number;
  kind: string;
  text: string;
  severity: 'info' | 'warning' | 'error';
}

export interface RunView {
  runId: string | null;
  state: RunState | 'idle';
  connected: boolean;
  profile: string | null;
  scoringModel: string | null;
  uncalibrated: boolean;
  risk: RiskClass | null;
  preflightPassed: boolean | null;
  findings: PreflightFinding[];
  estimatedSeconds: number | null;
  currentModule: string | null;
  currentPhase: string | null;
  moduleIndex: number;
  moduleTotal: number;
  progress: number;
  liveMetrics: Map<string, { key: string; value: number; unit: string; rep: number }>;
  results: ModuleResult[];
  telemetry: TelemetryPoint[];
  latest: TelemetryPoint | null;
  total: number | null;
  categories: CategoryScore[];
  scoresFinal: boolean;
  verdict: ResultState | null;
  bundleDigest: string | null;
  log: LogLine[];
  lastSeq: number;
}

const initial: RunView = {
  runId: null,
  state: 'idle',
  connected: false,
  profile: null,
  scoringModel: null,
  uncalibrated: true,
  risk: null,
  preflightPassed: null,
  findings: [],
  estimatedSeconds: null,
  currentModule: null,
  currentPhase: null,
  moduleIndex: 0,
  moduleTotal: 0,
  progress: 0,
  liveMetrics: new Map(),
  results: [],
  telemetry: [],
  latest: null,
  total: null,
  categories: [],
  scoresFinal: false,
  verdict: null,
  bundleDigest: null,
  log: [],
  lastSeq: -1,
};

type Action =
  | { type: 'reset'; runId: string }
  | { type: 'connection'; connected: boolean }
  | { type: 'events'; events: DarcEvent[] };

function log(state: RunView, kind: string, text: string, severity: LogLine['severity'], seq: number): LogLine[] {
  const next = [...state.log, { seq, kind, text, severity }];
  return next.length > LOG_LIMIT ? next.slice(next.length - LOG_LIMIT) : next;
}

function reduce(state: RunView, action: Action): RunView {
  if (action.type === 'reset') {
    return { ...initial, runId: action.runId, liveMetrics: new Map(), state: 'created' };
  }
  if (action.type === 'connection') {
    return { ...state, connected: action.connected };
  }

  // A batch is folded into one new state, which is the whole point of
  // `FLUSH_MS`: React re-renders once per batch rather than once per event.
  let next = state;
  for (const event of action.events) next = foldEvent(next, event);
  return next;
}

function foldEvent(state: RunView, event: DarcEvent): RunView {
  // Idempotent replay: an event already folded in is ignored, so a reconnect
  // that re-delivers the tail cannot double-count anything.
  if (event.seq <= state.lastSeq) return state;
  const base = { ...state, lastSeq: event.seq };

  switch (event.type) {
    case 'run.created':
      return {
        ...base,
        profile: event.profile,
        scoringModel: event.scoring_model,
        moduleTotal: event.modules.length,
        state: 'preflight',
        log: log(state, event.type, `${event.profile} · ${event.modules.length} module(s) · agent ${event.agent_version}`, 'info', event.seq),
      };

    case 'run.preflight.completed':
      return {
        ...base,
        risk: event.risk,
        preflightPassed: event.passed,
        findings: event.findings,
        estimatedSeconds: event.estimated_duration_s,
        state: event.passed ? 'running' : 'failed',
        log: log(
          state,
          event.type,
          `${event.risk} · ${event.passed ? 'passed' : 'BLOCKED'} · ~${event.estimated_duration_s}s · ${event.estimated_bytes_written} bytes written`,
          event.passed ? 'info' : 'error',
          event.seq,
        ),
      };

    case 'module.queued':
    case 'module.preparing':
    case 'module.warmup':
    case 'module.started':
      return {
        ...base,
        currentModule: event.module.id,
        currentPhase: event.phase,
        moduleIndex: event.index,
        moduleTotal: event.total,
        state: 'running',
      };

    case 'module.sample': {
      // Warm-up samples are streamed so the UI shows activity, but they are
      // never charted as results.
      if (event.warmup) {
        return { ...base, progress: event.module_progress };
      }
      const liveMetrics = new Map(state.liveMetrics);
      liveMetrics.set(event.metric_key, {
        key: event.metric_key,
        value: event.value,
        unit: event.unit,
        rep: event.rep,
      });
      return { ...base, liveMetrics, progress: event.module_progress };
    }

    case 'module.telemetry': {
      const point: TelemetryPoint = {
        monoMs: event.mono_ms,
        cpuBusy: event.cpu_busy_pct,
        cpuExternal: event.cpu_external_busy_pct,
        cpuSteal: event.cpu_steal_pct,
        load1: event.load1,
        memUsed: event.mem_used_bytes,
        memTotal: event.mem_total_bytes,
        freqMhz: event.cpu_freq_mhz,
        tempC: event.cpu_temp_c,
      };
      const telemetry = [...state.telemetry, point];
      return {
        ...base,
        latest: point,
        telemetry: telemetry.length > TELEMETRY_WINDOW ? telemetry.slice(telemetry.length - TELEMETRY_WINDOW) : telemetry,
      };
    }

    case 'module.warning':
      return {
        ...base,
        log: log(state, event.type, `${event.code}: ${event.message}`, 'warning', event.seq),
      };

    case 'module.completed':
      return {
        ...base,
        results: [...state.results, event.result],
        log: log(state, event.type, `${event.result.module.id} · ${event.result.status}`, 'info', event.seq),
      };

    case 'module.failed':
      return {
        ...base,
        log: log(state, event.type, `${event.module.id}: ${event.error}`, 'error', event.seq),
      };

    case 'score.provisional':
    case 'score.final':
      return {
        ...base,
        total: event.total,
        categories: event.categories,
        uncalibrated: event.uncalibrated,
        scoresFinal: event.type === 'score.final',
      };

    case 'report.generated':
      return {
        ...base,
        bundleDigest: event.bundle_sha256,
        log: log(state, event.type, `${event.formats.join(', ')} · ${event.bytes} bytes`, 'info', event.seq),
      };

    case 'run.completed':
      return {
        ...base,
        state: event.state,
        verdict: event.verdict.state,
        progress: 1,
        log: log(
          state,
          event.type,
          `${event.state} · verdict ${event.verdict.state} · ${event.modules_completed} completed, ${event.modules_failed} failed`,
          event.state === 'completed' ? 'info' : 'warning',
          event.seq,
        ),
      };

    case 'run.invalidated':
      return { ...base, state: 'failed', verdict: event.verdict.state };

    default:
      return base;
  }
}

export function useRunStream() {
  const [view, dispatch] = useReducer(reduce, initial);
  const sourceRef = useRef<EventSource | null>(null);
  const pendingRef = useRef<DarcEvent[]>([]);
  const flushRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(() => {
    if (flushRef.current !== null) {
      clearTimeout(flushRef.current);
      flushRef.current = null;
    }
    if (pendingRef.current.length === 0) return;
    const batch = pendingRef.current;
    pendingRef.current = [];
    dispatch({ type: 'events', events: batch });
  }, []);

  const disconnect = useCallback(() => {
    sourceRef.current?.close();
    sourceRef.current = null;
    // Whatever arrived in the last window is still real and still ordered;
    // dropping it would leave the final view a few events short of the run
    // that actually happened.
    flush();
    dispatch({ type: 'connection', connected: false });
  }, [flush]);

  const connect = useCallback(
    (runId: string) => {
      disconnect();
      pendingRef.current = [];
      dispatch({ type: 'reset', runId });

      const source = new EventSource(api.eventsUrl(runId));
      sourceRef.current = source;

      const handle = (raw: MessageEvent<string>) => {
        let event: DarcEvent;
        try {
          event = JSON.parse(raw.data) as DarcEvent;
        } catch {
          // A malformed frame must not take down the dashboard; the sequence
          // gap will be visible and a reconnect can recover it.
          return;
        }
        pendingRef.current.push(event);

        // A terminal event ends the stream, so there is no later flush to wait
        // for. Anything else is coalesced: the run emits samples as fast as the
        // machine produces them, and re-rendering per sample is what this
        // file's own docstring warns turns the observer into load on the
        // machine under test.
        if (event.type === 'run.completed' || event.type === 'run.invalidated') {
          flush();
          return;
        }
        if (pendingRef.current.length >= MAX_PENDING) {
          flush();
          return;
        }
        if (flushRef.current === null) {
          flushRef.current = setTimeout(flush, FLUSH_MS);
        }
      };

      source.onopen = () => dispatch({ type: 'connection', connected: true });
      source.onmessage = handle;
      // The agent names every event, so named listeners are what actually fire;
      // `onmessage` is the fallback for an unnamed frame.
      for (const kind of EVENT_KINDS) {
        source.addEventListener(kind, handle as EventListener);
      }
      source.onerror = () => {
        dispatch({ type: 'connection', connected: false });
        // EventSource reconnects on its own with Last-Event-ID, which the agent
        // honours. Closing here would throw away that recovery path.
      };
    },
    [disconnect, flush],
  );

  useEffect(() => () => disconnect(), [disconnect]);

  // Once the run reaches a terminal state there is nothing more to receive.
  useEffect(() => {
    if (view.state === 'completed' || view.state === 'failed' || view.state === 'cancelled') {
      disconnect();
    }
  }, [view.state, disconnect]);

  const metrics = useMemo(() => {
    const merged = new Map<string, Metric | { key: string; value: number; unit: string }>();
    for (const live of view.liveMetrics.values()) merged.set(live.key, live);
    for (const result of view.results) {
      for (const metric of result.metrics) merged.set(metric.key, metric);
    }
    return Array.from(merged.values()).sort((a, b) => a.key.localeCompare(b.key));
  }, [view.liveMetrics, view.results]);

  return { view, metrics, connect, disconnect };
}
