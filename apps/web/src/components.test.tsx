import { render, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { EventLog, MetricTable, RiskPanel, ScoreBoard, Telemetry } from './components';
import { category, metric } from './test/events';
import type { TelemetryPoint } from './useRunStream';

describe('MetricTable', () => {
  it('says when there is nothing to show', () => {
    render(<MetricTable metrics={[]} />);
    expect(screen.getByText('No measurements yet.')).toBeTruthy();
  });

  it('marks live values and flags a coefficient of variation above 15%', () => {
    render(
      <MetricTable
        metrics={[metric('steady', 100, 0.05), metric('noisy', 100, 0.2), { key: 'live.one', value: 3, unit: 'ms' }]}
      />,
    );

    const row = (key: string) => screen.getByText(key).closest('tr')!;
    expect(within(row('live.one')).getByText('live')).toBeTruthy();
    expect(within(row('steady')).queryByText('live')).toBeNull();
    expect(within(row('noisy')).getByText('20.0%').className).toContain('alert');
    expect(within(row('steady')).getByText('5.0%').className).not.toContain('alert');
  });
});

describe('ScoreBoard', () => {
  it('exposes the final number to assistive technology, not the animation', () => {
    render(
      <ScoreBoard total={1234.4} categories={[category('compute', 812.6)]} provisional verdict="self_reported" />,
    );

    expect(screen.getByText('Total score 1234')).toBeTruthy();
    expect(screen.getByText('Compute 813')).toBeTruthy();
    expect(screen.getByText(/DARCBench Total/).textContent).toContain('(provisional)');
    expect(screen.getByText('Result state: self reported')).toBeTruthy();
  });

  it('says there is no score yet rather than showing zero', () => {
    render(<ScoreBoard total={null} categories={[]} provisional={false} verdict={null} />);

    expect(screen.getByText('No total score yet')).toBeTruthy();
    expect(screen.getByText('No scores yet.')).toBeTruthy();
  });

  it('shows a category weight only when it carries one', () => {
    render(
      <ScoreBoard
        total={1}
        categories={[category('compute', 1, 0.3), category('single_core', 1, 0)]}
        provisional={false}
        verdict={null}
      />,
    );

    expect(screen.getAllByText(/^weight/)).toHaveLength(1);
    expect(screen.getByText('weight 30%')).toBeTruthy();
  });
});

describe('RiskPanel', () => {
  it('renders nothing before preflight', () => {
    const { container } = render(<RiskPanel risk={null} findings={[]} estimatedSeconds={null} />);
    expect(container.innerHTML).toBe('');
  });

  it('names the risk in words and marks blocking findings', () => {
    render(
      <RiskPanel
        risk="production_risk"
        findings={[{ check: 'web.listeners', severity: 'error', message: 'nginx is serving', blocking: true }]}
        estimatedSeconds={90}
      />,
    );

    expect(screen.getByText('Production risk')).toBeTruthy();
    expect(screen.getByText(/about 90s/)).toBeTruthy();
    const finding = screen.getByText('nginx is serving').closest('li')!;
    expect(within(finding).getByText('ERROR')).toBeTruthy();
    expect(within(finding).getByText('blocking')).toBeTruthy();
  });
});

describe('Telemetry', () => {
  const point = (overrides: Partial<TelemetryPoint> = {}): TelemetryPoint => ({
    monoMs: 0,
    cpuBusy: 99,
    cpuExternal: 0,
    cpuSteal: 0,
    load1: 1,
    memUsed: 2 * 1024 ** 3,
    memTotal: 8 * 1024 ** 3,
    freqMhz: null,
    tempC: null,
    ...overrides,
  });

  it('warns, in words, when something else is competing for the CPU', () => {
    render(<Telemetry latest={point({ cpuExternal: 12 })} history={[point()]} />);
    expect(screen.getByText(/Something other than this benchmark/)).toBeTruthy();
  });

  it('stays quiet when the run has the machine to itself', () => {
    render(<Telemetry latest={point({ cpuExternal: 2 })} history={[point()]} />);
    expect(screen.queryByText(/Something other than this benchmark/)).toBeNull();
    expect(screen.queryByText(/Hypervisor/)).toBeNull();
  });

  it('flags steal above 1%', () => {
    render(<Telemetry latest={point({ cpuSteal: 3 })} history={[point()]} />);
    expect(screen.getByText(/Hypervisor is taking CPU/)).toBeTruthy();
  });

  it('says n/a for what the host does not expose', () => {
    render(<Telemetry latest={point()} history={[]} />);
    expect(screen.getAllByText('n/a')).toHaveLength(2);
    expect(screen.getByText('2.0 GiB')).toBeTruthy();
  });
});

describe('EventLog', () => {
  it('shows each line with its sequence number and severity', () => {
    render(<EventLog lines={[{ seq: 7, kind: 'module.warning', text: 'wide', severity: 'warning' }]} />);

    const line = screen.getByText('wide').closest('.log-line')!;
    expect(line.className).toContain('log-warning');
    expect(line.textContent).toContain('7');
  });
});
