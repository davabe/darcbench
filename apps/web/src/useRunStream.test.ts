import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  FLUSH_MS,
  initialRunView,
  LOG_LIMIT,
  MAX_PENDING,
  reduceRunView,
  TELEMETRY_WINDOW,
  useRunStream,
} from './useRunStream';
import type { RunView } from './useRunStream';
import type { DarcEvent } from './types';
import { EVENT_KINDS } from './types';
import {
  category,
  moduleCompleted,
  moduleStarted,
  moduleWarning,
  preflightCompleted,
  runCompleted,
  runCreated,
  sample,
  score,
  telemetry,
} from './test/events';

function fold(events: DarcEvent[], from: RunView = reduceRunView(initialRunView, { type: 'reset', runId: 'run_x' })) {
  return reduceRunView(from, { type: 'events', events });
}

describe('the event fold', () => {
  it('follows a run from creation to its verdict', () => {
    const view = fold([
      runCreated(0),
      preflightCompleted(1),
      moduleStarted(2),
      sample(3, { value: 110 }),
      moduleCompleted(4),
      score(5, true, [category('compute', 900)], 900),
      runCompleted(6),
    ]);

    expect(view.state).toBe('completed');
    expect(view.profile).toBe('quick');
    expect(view.moduleTotal).toBe(2);
    expect(view.risk).toBe('heavy_load');
    expect(view.results).toHaveLength(1);
    expect(view.total).toBe(900);
    expect(view.scoresFinal).toBe(true);
    expect(view.verdict).toBe('local');
    expect(view.progress).toBe(1);
    expect(view.lastSeq).toBe(6);
  });

  // The reconnect guarantee: EventSource resends from Last-Event-ID, and a
  // tail delivered twice must not count twice.
  it('ignores events it has already folded', () => {
    const events = [runCreated(0), moduleCompleted(1), moduleWarning(2)];
    const once = fold(events);
    const twice = reduceRunView(once, { type: 'events', events });

    expect(twice).toBe(once);
    expect(twice.results).toHaveLength(1);
    expect(twice.log).toHaveLength(once.log.length);
  });

  it('shows warm-up activity without charting it as a result', () => {
    const view = fold([sample(0, { warmup: true, progress: 0.2 })]);

    expect(view.liveMetrics.size).toBe(0);
    expect(view.progress).toBe(0.2);
  });

  it('keeps only the latest live value per metric', () => {
    const view = fold([sample(0, { value: 1 }), sample(1, { value: 2 }), sample(2, { key: 'other', value: 3 })]);

    expect(view.liveMetrics.size).toBe(2);
    expect(view.liveMetrics.get('hash.single')?.value).toBe(2);
  });

  it('fails the run when preflight blocks it, and logs that as an error', () => {
    const view = fold([runCreated(0), preflightCompleted(1, false)]);

    expect(view.state).toBe('failed');
    expect(view.preflightPassed).toBe(false);
    expect(view.log.at(-1)?.severity).toBe('error');
    expect(view.log.at(-1)?.text).toContain('BLOCKED');
  });

  it('marks provisional scores as provisional', () => {
    const view = fold([score(0, false, [category('compute', 500)], 500)]);

    expect(view.scoresFinal).toBe(false);
    expect(view.uncalibrated).toBe(true);
  });

  it(`bounds telemetry to the last ${TELEMETRY_WINDOW} points`, () => {
    const events = Array.from({ length: TELEMETRY_WINDOW + 20 }, (_, seq) => telemetry(seq, seq));
    const view = fold(events);

    expect(view.telemetry).toHaveLength(TELEMETRY_WINDOW);
    expect(view.telemetry[0]?.cpuBusy).toBe(20);
    expect(view.latest?.cpuBusy).toBe(TELEMETRY_WINDOW + 19);
  });

  it(`bounds the log to the last ${LOG_LIMIT} lines`, () => {
    const events = Array.from({ length: LOG_LIMIT + 50 }, (_, seq) => moduleWarning(seq));
    const view = fold(events);

    expect(view.log).toHaveLength(LOG_LIMIT);
    expect(view.log[0]?.seq).toBe(50);
    expect(view.log.at(-1)?.seq).toBe(LOG_LIMIT + 49);
  });

  it('starts every run from a clean slate', () => {
    const finished = fold([runCreated(0), moduleCompleted(1), runCompleted(2)]);
    const next = reduceRunView(finished, { type: 'reset', runId: 'run_y' });

    expect(next.runId).toBe('run_y');
    expect(next.state).toBe('created');
    expect(next.results).toHaveLength(0);
    expect(next.lastSeq).toBe(-1);
    // A shared Map would let one run's live values leak into the next.
    expect(next.liveMetrics).not.toBe(initialRunView.liveMetrics);
  });
});

/** Enough of `EventSource` to drive the hook: named listeners, open, error, close. */
class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly listeners = new Map<string, ((event: MessageEvent<string>) => void)[]>();
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  closed = false;

  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }

  addEventListener(kind: string, listener: (event: MessageEvent<string>) => void) {
    this.listeners.set(kind, [...(this.listeners.get(kind) ?? []), listener]);
  }

  close() {
    this.closed = true;
  }

  emit(event: DarcEvent) {
    this.emitRaw(event.type, JSON.stringify(event));
  }

  emitRaw(kind: string, data: string) {
    for (const listener of this.listeners.get(kind) ?? []) {
      listener(new MessageEvent(kind, { data }));
    }
  }
}

describe('useRunStream', () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal('EventSource', FakeEventSource);
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function connected() {
    const hook = renderHook(() => useRunStream());
    act(() => hook.result.current.connect('run_abc'));
    const source = FakeEventSource.instances.at(-1)!;
    return { hook, source };
  }

  it('subscribes to the run’s event URL and to every named event kind', () => {
    const { source } = connected();

    expect(source.url).toBe('/api/v1/runs/run_abc/events');
    expect([...source.listeners.keys()].sort()).toEqual([...EVENT_KINDS].sort());
  });

  // The observer-overhead rule: one render per window, not one per sample.
  it('pools events and applies them once per flush window', () => {
    const { hook, source } = connected();

    act(() => {
      source.emit(runCreated(0));
      source.emit(sample(1));
      source.emit(sample(2));
    });
    expect(hook.result.current.view.lastSeq).toBe(-1);

    act(() => vi.advanceTimersByTime(FLUSH_MS));
    expect(hook.result.current.view.lastSeq).toBe(2);
  });

  it(`flushes without waiting once ${MAX_PENDING} events are pooled`, () => {
    const { hook, source } = connected();

    act(() => {
      for (let seq = 0; seq < MAX_PENDING; seq += 1) source.emit(sample(seq));
    });
    expect(hook.result.current.view.lastSeq).toBe(MAX_PENDING - 1);
  });

  it('applies a terminal event at once and closes the stream', () => {
    const { hook, source } = connected();

    act(() => {
      source.emit(runCreated(0));
      source.emit(runCompleted(1));
    });

    expect(hook.result.current.view.state).toBe('completed');
    expect(source.closed).toBe(true);
    expect(hook.result.current.view.connected).toBe(false);
  });

  it('drops a malformed frame and keeps going', () => {
    const { hook, source } = connected();

    act(() => {
      source.emitRaw('module.sample', '{not json');
      source.emit(runCreated(0));
      vi.advanceTimersByTime(FLUSH_MS);
    });

    expect(hook.result.current.view.lastSeq).toBe(0);
    expect(hook.result.current.view.state).toBe('preflight');
  });

  it('reports the connection, and leaves reconnection to EventSource', () => {
    const { hook, source } = connected();

    act(() => source.onopen?.());
    expect(hook.result.current.view.connected).toBe(true);

    act(() => source.onerror?.());
    expect(hook.result.current.view.connected).toBe(false);
    // Closing on error would discard the Last-Event-ID recovery path.
    expect(source.closed).toBe(false);
  });

  it('merges live values with final results, final winning', () => {
    const { hook, source } = connected();

    act(() => {
      source.emit(sample(0, { key: 'hash.single', value: 99 }));
      source.emit(sample(1, { key: 'live.only', value: 5 }));
      source.emit(moduleCompleted(2));
      vi.advanceTimersByTime(FLUSH_MS);
    });

    const metrics = hook.result.current.metrics;
    expect(metrics.map((m) => m.key)).toEqual(['hash.single', 'live.only']);
    expect(metrics[0]?.value).toBe(120);
    expect('summary' in metrics[0]!).toBe(true);
  });

  it('closes the stream on unmount', () => {
    const { hook, source } = connected();
    hook.unmount();
    expect(source.closed).toBe(true);
  });
});
