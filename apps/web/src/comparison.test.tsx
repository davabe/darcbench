import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { api, ApiError } from './api';
import { RunComparisonPanel } from './comparison';
import type { MetricDelta, RunComparison, RunListEntry } from './types';

vi.mock('./api', async (importOriginal) => {
  const original = await importOriginal<typeof import('./api')>();
  return { ...original, api: { ...original.api, runs: vi.fn(), compare: vi.fn() } };
});

const runs = vi.mocked(api.runs);
const compare = vi.mocked(api.compare);

function run(id: string, finishedAt: string, overrides: Partial<RunListEntry> = {}): RunListEntry {
  return {
    run_id: id,
    profile: 'standard',
    state: 'completed',
    created_at: finishedAt,
    finished_at: finishedAt,
    modules: ['cpu.mixed'],
    progress: 1,
    total_score: 900,
    result_state: 'local',
    ...overrides,
  };
}

function delta(metric_key: string, ratio: number, baseline = 100, candidate = 100): MetricDelta {
  return { module: 'cpu.mixed', metric_key, unit: 'MB/s', baseline, candidate, ratio };
}

function comparison(overrides: Partial<RunComparison> = {}): RunComparison {
  return {
    baseline: { run_id: 'run_old', profile: 'standard', finished_at: '2026-10-01T00:00:00Z', total_score: 1000 },
    candidate: { run_id: 'run_new', profile: 'standard', finished_at: '2026-10-02T00:00:00Z', total_score: 900 },
    comparable: true,
    incomparable_reasons: [],
    metrics: [],
    unmatched: [],
    ...overrides,
  };
}

const HISTORY = [
  run('run_old', '2026-10-01T00:00:00Z'),
  run('run_new', '2026-10-02T00:00:00Z'),
  run('run_live', '2026-10-03T00:00:00Z', { state: 'running', finished_at: null }),
];

function select(label: string) {
  return screen.getByLabelText(label) as HTMLSelectElement;
}

async function renderWithHistory(history: RunListEntry[] = HISTORY) {
  runs.mockResolvedValue(history);
  render(<RunComparisonPanel completedRunId={null} />);
  await waitFor(() => expect(screen.queryByText(/Reading the run history/)).toBeNull());
  // The history arriving starts a chain: default selection, then the effect
  // that clears any comparison for the old pair. A browser runs that chain
  // before it dispatches the next click; a test has to let it finish.
  await act(async () => {});
}

beforeEach(() => {
  runs.mockReset();
  compare.mockReset();
});

describe('the run picker', () => {
  it('explains that a comparison needs two finished runs', async () => {
    await renderWithHistory([]);
    expect(screen.getByText(/No finished runs recorded yet/)).toBeTruthy();
  });

  it('names the one run there is', async () => {
    await renderWithHistory([run('run_only', '2026-10-01T00:00:00Z')]);
    expect(screen.getByText(/One finished run recorded/).textContent).toContain('run_only');
  });

  it('defaults to the newest run as candidate and the one before as baseline', async () => {
    await renderWithHistory();

    expect(select('Candidate').value).toBe('run_new');
    expect(select('Baseline').value).toBe('run_old');
  });

  it('offers only runs that have finished', async () => {
    await renderWithHistory();

    const offered = Array.from(select('Baseline').options).map((option) => option.value);
    expect(offered).toEqual(['run_new', 'run_old']);
  });

  it('will not compare a run with itself', async () => {
    await renderWithHistory();

    fireEvent.change(select('Baseline'), { target: { value: 'run_new' } });

    expect((screen.getByRole('button', { name: 'Compare' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/the same run/)).toBeTruthy();
  });

  it('shows the history failure and lets the operator retry', async () => {
    runs.mockRejectedValueOnce(new ApiError('boom', 'internal', 500)).mockResolvedValueOnce(HISTORY);
    render(<RunComparisonPanel completedRunId={null} />);

    await screen.findByText(/could not be read/);
    fireEvent.click(screen.getByRole('button', { name: 'Try again' }));

    await waitFor(() => expect(select('Candidate').value).toBe('run_new'));
    expect(runs).toHaveBeenCalledTimes(2);
  });

  it('refetches the history when a run completes, and not otherwise', async () => {
    runs.mockResolvedValue(HISTORY);
    const { rerender } = render(<RunComparisonPanel completedRunId={null} />);
    await waitFor(() => expect(runs).toHaveBeenCalledTimes(1));

    rerender(<RunComparisonPanel completedRunId={null} />);
    rerender(<RunComparisonPanel completedRunId="run_new" />);

    await waitFor(() => expect(runs).toHaveBeenCalledTimes(2));
  });
});

describe('a comparison', () => {
  async function compareWith(result: RunComparison) {
    await renderWithHistory();
    compare.mockResolvedValueOnce(result);
    fireEvent.click(screen.getByRole('button', { name: 'Compare' }));
    await screen.findByText(/compared$/);
  }

  it('asks for baseline then candidate', async () => {
    await compareWith(comparison());
    expect(compare).toHaveBeenCalledWith('run_old', 'run_new');
  });

  it('puts the largest regression first and counts each direction', async () => {
    await compareWith(
      comparison({
        metrics: [delta('a.better', 1.2), delta('b.worst', 0.5), delta('c.worse', 0.9), delta('d.same', 1)],
      }),
    );

    const rows = within(screen.getByRole('table')).getAllByRole('row').slice(1);
    expect(rows.map((row) => row.querySelector('code')?.textContent)).toEqual([
      'cpu.mixed/b.worst',
      'cpu.mixed/c.worse',
      'cpu.mixed/d.same',
      'cpu.mixed/a.better',
    ]);
    expect(rows[0]?.textContent).toContain('-50.0%');
    expect(rows[0]?.textContent).toContain('worse');
    expect(screen.getByText(/1 better/).textContent).toContain('4 metrics lined up — 1 better, 2 worse, 1 unchanged');
  });

  // A change that displays as 0.0% must not be captioned as a finding.
  it('calls float noise no change', async () => {
    await compareWith(comparison({ metrics: [delta('noise', 1.00004)] }));

    const row = within(screen.getByRole('table')).getAllByRole('row')[1]!;
    expect(row.textContent).toContain('0.0%');
    expect(row.textContent).toContain('no change');
    expect(row.textContent).not.toContain('better');
  });

  it('prints each measurement at a precision that suits it', async () => {
    await compareWith(
      comparison({ metrics: [delta('small', 1, 0.41, 0.41), delta('large', 1, 12400, 12400), delta('tiny', 1, 1.5e-5, 1.5e-5)] }),
    );

    const table = screen.getByRole('table');
    expect(within(table).getAllByText('0.4100')).toHaveLength(2);
    expect(within(table).getAllByText('12400')).toHaveLength(2);
    expect(within(table).getAllByText('1.50e-5')).toHaveLength(2);
  });

  it('states why two runs are not directly comparable, above the numbers', async () => {
    await compareWith(
      comparison({
        comparable: false,
        incomparable_reasons: ['different scoring model'],
        metrics: [delta('a', 1.1)],
      }),
    );

    const banner = screen.getByRole('note');
    expect(banner.textContent).toContain('Not directly comparable');
    expect(banner.textContent).toContain('different scoring model');
    expect(banner.compareDocumentPosition(screen.getByRole('table')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('names the metrics it could not line up instead of dropping them', async () => {
    await compareWith(comparison({ unmatched: ['storage.mixed/fsync.p99'] }));

    expect(screen.getByText(/Not compared · 1 metric/)).toBeTruthy();
    expect(screen.getByText('storage.mixed/fsync.p99')).toBeTruthy();
  });

  it('gives no total change when a run has no score', async () => {
    await compareWith(
      comparison({ baseline: { ...comparison().baseline, total_score: null } }),
    );

    expect(screen.getByText(/no positive total score/)).toBeTruthy();
  });

  it('explains a run that is not in the index', async () => {
    await renderWithHistory();
    compare.mockRejectedValueOnce(new ApiError('not found', 'unknown_run', 404));
    fireEvent.click(screen.getByRole('button', { name: 'Compare' }));

    expect((await screen.findByRole('alert')).textContent).toContain('added to the index when its result bundle is written');
  });

  // Two selects and a button can outrun a request on a busy machine.
  it('discards the answer for a pair that is no longer selected', async () => {
    await renderWithHistory([...HISTORY, run('run_third', '2026-09-30T00:00:00Z')]);
    let answer!: (value: RunComparison) => void;
    compare.mockReturnValueOnce(new Promise((resolve) => (answer = resolve)));

    fireEvent.click(screen.getByRole('button', { name: 'Compare' }));
    fireEvent.change(select('Baseline'), { target: { value: 'run_third' } });
    await act(async () => answer(comparison({ metrics: [delta('stale', 2)] })));

    expect(screen.queryByRole('table')).toBeNull();
    expect(screen.getByRole('status').textContent).toBe('not compared yet');
  });
});
