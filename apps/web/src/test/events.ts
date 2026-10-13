/**
 * Builders for `darcbench.events/1` events, so a test states only the fields it
 * is about. Every builder produces a complete, correctly typed event: a fixture
 * that drifted from `types.ts` should fail `tsc`, not pass a test by accident.
 */

import type { CategoryScore, DarcEvent, Metric, ModuleResult } from '../types';

/**
 * The event variant whose `type` admits `K`. Not `Extract<DarcEvent, { type: K }>`:
 * several variants share one shape under a union of kinds (`module.started` is one
 * of five), and `Extract` matches none of them.
 */
type EventOf<K extends DarcEvent['type']> = DarcEvent extends infer E
  ? E extends { type: infer T }
    ? K extends T
      ? E
      : never
    : never
  : never;

const RUN_ID = 'run_0123456789abcdef0123456789abcdef';

function envelope(seq: number) {
  return { protocol: 'darcbench.events/1', run_id: RUN_ID, seq, ts: '2026-10-13T12:00:00Z', mono_ms: seq * 10 };
}

export function runCreated(seq: number, modules = ['cpu.mixed', 'memory.bandwidth']): EventOf<'run.created'> {
  return {
    ...envelope(seq),
    type: 'run.created',
    profile: 'quick',
    modules: modules.map((id) => ({ id, version: '1.0.0' })),
    agent_version: '0.2.0',
    scoring_model: 'dbs/0.2.0-dev',
    environment_digest: 'sha256:00',
  };
}

export function preflightCompleted(seq: number, passed = true): EventOf<'run.preflight.completed'> {
  return {
    ...envelope(seq),
    type: 'run.preflight.completed',
    risk: passed ? 'heavy_load' : 'production_risk',
    passed,
    findings: [],
    estimated_duration_s: 60,
    estimated_bytes_written: 0,
    estimated_network_bytes: 0,
    estimated_peak_memory_bytes: 0,
    estimated_write_volume_bytes: 0,
  };
}

export function moduleStarted(seq: number, id = 'cpu.mixed', index = 0, total = 2): EventOf<'module.started'> {
  return { ...envelope(seq), type: 'module.started', module: { id, version: '1.0.0' }, index, total, phase: 'measure' };
}

export function sample(
  seq: number,
  { key = 'hash.single', value = 100, warmup = false, progress = 0.5 } = {},
): EventOf<'module.sample'> {
  return {
    ...envelope(seq),
    type: 'module.sample',
    module: 'cpu.mixed',
    metric_key: key,
    rep: seq,
    warmup,
    value,
    unit: 'MB/s',
    duration_ms: 10,
    module_progress: progress,
  };
}

export function telemetry(seq: number, cpuBusy = 50): EventOf<'module.telemetry'> {
  return {
    ...envelope(seq),
    type: 'module.telemetry',
    module: 'cpu.mixed',
    cpu_busy_pct: cpuBusy,
    cpu_external_busy_pct: 0,
    cpu_steal_pct: 0,
    cpu_iowait_pct: 0,
    load1: 1,
    mem_used_bytes: 1,
    mem_total_bytes: 2,
    swap_used_bytes: 0,
    cpu_freq_mhz: null,
    cpu_temp_c: null,
    psi_cpu_some_avg10: null,
    disk_read_bytes_per_s: 0,
    disk_write_bytes_per_s: 0,
    net_rx_bytes_per_s: 0,
    net_tx_bytes_per_s: 0,
  };
}

export function moduleWarning(seq: number): EventOf<'module.warning'> {
  return { ...envelope(seq), type: 'module.warning', module: 'cpu.mixed', code: 'high_variance', message: 'wide' };
}

export function metric(key: string, value: number, cv: number | null = 0.01): Metric {
  return {
    key,
    label: key,
    unit: 'MB/s',
    direction: 'higher_is_better',
    value,
    summary: { n: 7, min: value, max: value, mean: value, median: value, stddev: 0, cv, ci95: [value, value] },
    outliers: [],
  };
}

export function moduleCompleted(seq: number, metrics: Metric[] = [metric('hash.single', 120)]): EventOf<'module.completed'> {
  const result: ModuleResult = {
    module: { id: 'cpu.mixed', version: '1.0.0' },
    status: 'completed',
    duration_ms: 1000,
    metrics,
    warnings: [],
    error: null,
    context: {},
  };
  return { ...envelope(seq), type: 'module.completed', result };
}

export function score(seq: number, final: boolean, categories: CategoryScore[], total: number | null) {
  const event: EventOf<'score.provisional' | 'score.final'> = {
    ...envelope(seq),
    type: final ? 'score.final' : 'score.provisional',
    scoring_model: 'dbs/0.2.0-dev',
    provisional: !final,
    total,
    categories,
    uncalibrated: true,
  };
  return event;
}

export function runCompleted(seq: number): EventOf<'run.completed'> {
  return {
    ...envelope(seq),
    type: 'run.completed',
    state: 'completed',
    verdict: { state: 'local', reasons: [], validator_version: 'dbv/0.1.0' },
    duration_ms: 60_000,
    modules_completed: 2,
    modules_failed: 0,
    final_seq: seq,
  };
}

export function category(key: string, score: number, weight = 0.2): CategoryScore {
  return { key, label: key[0]!.toUpperCase() + key.slice(1), score, weight };
}
