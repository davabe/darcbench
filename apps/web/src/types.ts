/**
 * TypeScript mirror of the `darcbench.events/1` protocol.
 *
 * These types are hand-maintained against `crates/darcbench-protocol`. That is
 * a deliberate Phase 1 choice: generating them adds a build-time dependency for
 * five small files. `docs/adr/0004-realtime-transport.md` records the plan to
 * generate them from the Rust types once the protocol stops moving, and
 * `scripts/check-protocol-parity.sh` fails CI if an event kind exists in Rust
 * but not here.
 */

export const PROTOCOL_VERSION = 'darcbench.events/1';

export type RunState =
  | 'created'
  | 'preflight'
  | 'running'
  | 'finalizing'
  | 'completed'
  | 'failed'
  | 'cancelled';

export type ResultState =
  | 'local'
  | 'self_reported'
  | 'validated'
  | 'verified'
  | 'official'
  | 'invalid'
  | 'partial'
  | 'custom';

export type RiskClass =
  | 'safe'
  | 'moderate_load'
  | 'heavy_load'
  | 'production_risk'
  | 'unsupported';

export type Severity = 'info' | 'warning' | 'error';

export interface ModuleRef {
  id: string;
  version: string;
}

export interface Summary {
  n: number;
  min: number;
  max: number;
  mean: number;
  median: number;
  stddev: number;
  cv: number | null;
  ci95: [number, number] | null;
}

export interface Metric {
  key: string;
  label: string;
  unit: string;
  direction: 'higher_is_better' | 'lower_is_better';
  value: number;
  summary: Summary;
  outliers: number[];
}

export interface Warning {
  code: string;
  message: string;
  metric_key: string | null;
}

export interface ModuleResult {
  module: ModuleRef;
  status: 'completed' | 'degraded' | 'failed' | 'cancelled' | 'skipped';
  duration_ms: number;
  metrics: Metric[];
  warnings: Warning[];
  error: string | null;
  context: Record<string, unknown>;
}

export interface PreflightFinding {
  check: string;
  severity: Severity;
  message: string;
  blocking: boolean;
}

export interface CategoryScore {
  key: string;
  label: string;
  score: number;
  weight: number;
}

/** The envelope fields present on every event. */
interface Base {
  protocol: string;
  run_id: string;
  seq: number;
  ts: string;
  mono_ms: number;
}

export type DarcEvent =
  | (Base & {
      type: 'run.created';
      profile: string;
      modules: ModuleRef[];
      agent_version: string;
      scoring_model: string;
      environment_digest: string;
    })
  | (Base & { type: 'run.preflight.started'; checks: string[] })
  | (Base & {
      type: 'run.preflight.completed';
      risk: RiskClass;
      passed: boolean;
      findings: PreflightFinding[];
      estimated_duration_s: number;
      estimated_bytes_written: number;
      estimated_network_bytes: number;
      estimated_peak_memory_bytes: number;
      estimated_write_volume_bytes: number;
    })
  | (Base & {
      type:
        | 'module.queued'
        | 'module.preparing'
        | 'module.warmup'
        | 'module.started'
        | 'module.cancelled';
      module: ModuleRef;
      index: number;
      total: number;
      phase: string | null;
    })
  | (Base & {
      type: 'module.sample';
      module: string;
      metric_key: string;
      rep: number;
      warmup: boolean;
      value: number;
      unit: string;
      duration_ms: number;
      module_progress: number;
    })
  | (Base & {
      type: 'module.telemetry';
      module: string | null;
      cpu_busy_pct: number;
      cpu_external_busy_pct: number;
      cpu_steal_pct: number;
      cpu_iowait_pct: number;
      load1: number;
      mem_used_bytes: number;
      mem_total_bytes: number;
      swap_used_bytes: number;
      cpu_freq_mhz: number | null;
      cpu_temp_c: number | null;
      psi_cpu_some_avg10: number | null;
      disk_read_bytes_per_s: number;
      disk_write_bytes_per_s: number;
      net_rx_bytes_per_s: number;
      net_tx_bytes_per_s: number;
    })
  | (Base & { type: 'module.warning'; module: string; code: string; message: string })
  | (Base & { type: 'module.completed'; result: ModuleResult })
  | (Base & { type: 'module.failed'; module: ModuleRef; error: string; fatal: boolean })
  | (Base & {
      type: 'score.provisional' | 'score.final';
      scoring_model: string;
      provisional: boolean;
      total: number | null;
      categories: CategoryScore[];
      uncalibrated: boolean;
    })
  | (Base & {
      type: 'report.generated';
      formats: string[];
      bundle_sha256: string;
      bytes: number;
    })
  | (Base & {
      type: 'run.completed';
      state: RunState;
      verdict: { state: ResultState; reasons: unknown[]; validator_version: string };
      duration_ms: number;
      modules_completed: number;
      modules_failed: number;
      final_seq: number;
    })
  | (Base & {
      type: 'run.invalidated';
      verdict: { state: ResultState; reasons: unknown[]; validator_version: string };
    })
  | (Base & { type: 'stream.heartbeat'; state: RunState; last_seq: number });

/** Every event kind the UI knows how to receive. Kept in sync by CI. */
export const EVENT_KINDS = [
  'run.created',
  'run.preflight.started',
  'run.preflight.completed',
  'module.queued',
  'module.preparing',
  'module.warmup',
  'module.started',
  'module.sample',
  'module.telemetry',
  'module.warning',
  'module.completed',
  'module.failed',
  'module.cancelled',
  'score.provisional',
  'score.final',
  'report.generated',
  'run.completed',
  'run.invalidated',
  'stream.heartbeat',
] as const;

export interface AgentMeta {
  product: string;
  agent_version: string;
  protocol: string;
  bundle_schema: string;
  scoring_model: string;
  scoring_calibrated: boolean;
  authentication_required: boolean;
  loopback_only: boolean;
}

/**
 * One row of `GET /api/v1/runs`.
 *
 * The endpoint merges two sources into one list, and the merge is visible in
 * this type: a run *in flight* is reported by the run manager and has no
 * `finished_at`, no `total_score` and no `result_state` yet, while a run from a
 * previous agent process comes from the on-disk index and always has all three.
 * The optional fields are therefore genuinely optional on the wire - the Rust
 * side skips serialising `None` - and anything that reads them has to handle
 * their absence rather than assume a completed run.
 */
export interface RunListEntry {
  run_id: string;
  profile: string;
  state: RunState;
  created_at: string;
  finished_at?: string | null;
  modules: string[];
  /** Fraction in `[0, 1]`, derived from completed modules rather than elapsed time. */
  progress: number;
  total_score?: number | null;
  result_state?: ResultState | null;
}

/** The `baseline` / `candidate` side of a comparison, as the agent reports it. */
export interface ComparedRun {
  run_id: string;
  profile: string;
  finished_at: string;
  total_score: number | null;
}

/** One metric lined up across two runs. */
export interface MetricDelta {
  module: string;
  metric_key: string;
  unit: string;
  baseline: number;
  candidate: number;
  /**
   * Candidate relative to baseline, **already direction-adjusted by the agent**:
   * above 1.0 always means the candidate did better, whether the metric counts
   * throughput or latency. Mirrors `MetricDelta::ratio` in
   * `crates/darcbench-agent/src/index.rs`, which inverts the quotient itself for
   * lower-is-better metrics. Re-inverting it here would report a doubled fsync
   * latency as a 2x improvement, which is the single reading a comparison must
   * never allow.
   */
  ratio: number;
}

/** The body of `GET /api/v1/runs/{baseline}/compare/{candidate}`. */
export interface RunComparison {
  baseline: ComparedRun;
  candidate: ComparedRun;
  /** False when the two runs disagree about anything that makes their numbers
   *  non-comparable. The comparison is still produced; it is labelled. */
  comparable: boolean;
  incomparable_reasons: string[];
  metrics: MetricDelta[];
  /** Metrics present in one run only, or not a positive measurement in both. */
  unmatched: string[];
}

export interface ProfileInfo {
  key: string;
  standard: boolean;
  nominal_minutes: [number, number];
  modules: string[];
  available: boolean;
}

export interface Inventory {
  platform: {
    scope: string;
    virtualization: string | null;
    kernel_release: string | null;
    distribution: string | null;
    distribution_version: string | null;
    architecture: string;
    cgroup_cpu_limit: number | null;
    load1: number | null;
    running_as_root: boolean;
    cloud_hint: string | null;
  };
  cpu: {
    model: string | null;
    vendor: string | null;
    logical_cpus: number;
    physical_cores: number | null;
    sockets: number | null;
    governor: string | null;
    max_mhz: number | null;
    instruction_sets: string[];
  };
  memory: { total_bytes: number; available_bytes: number; swap_total_bytes: number };
  storage: { complex_stack: boolean; stack_indicators: string[] };
  software: { production_likelihood: string; production_signals: string[] };
  gaps: { field: string; reason: string }[];
}
