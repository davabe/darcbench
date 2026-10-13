import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { AgentMeta, ProfileInfo } from './types';

const META: AgentMeta = {
  product: 'DARCBench',
  agent_version: '0.2.0',
  protocol: 'darcbench.events/1',
  bundle_schema: 'darcbench.bundle/1',
  scoring_model: 'dbs/0.2.0-dev',
  scoring_calibrated: false,
  authentication_required: true,
  loopback_only: true,
};

const PROFILES: ProfileInfo[] = [
  { key: 'web', standard: true, nominal_minutes: [8, 15], modules: [], available: false },
  { key: 'standard', standard: true, nominal_minutes: [10, 20], modules: ['cpu.mixed'], available: true },
];

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

/** The agent, as far as the dashboard's startup can see it. */
function agent({ sessionStatus = 200 } = {}) {
  return vi.fn(async (input: RequestInfo | URL) => {
    const path = String(input).split('?')[0];
    switch (path) {
      case '/api/v1/session':
        return sessionStatus === 200
          ? json({ ok: true })
          : json({ code: 'unauthorized', message: 'invalid token' }, sessionStatus);
      case '/api/v1/meta':
        return json(META);
      case '/api/v1/profiles':
        return json({ profiles: PROFILES });
      case '/api/v1/inventory':
        return json({ inventory: null, redacted: true, performance_digest: '' });
      case '/api/v1/runs':
        return json({ runs: [] });
      default:
        return json({ code: 'not_found', message: path }, 404);
    }
  });
}

async function renderApp() {
  // `api.ts` holds the bearer token in module state; a fresh module per test.
  vi.resetModules();
  const { default: App } = await import('./App');
  return render(<App />);
}

beforeEach(() => {
  window.history.replaceState({}, '', '/');
});

describe('startup', () => {
  it('exchanges the token for a session before anything else, then scrubs it from the URL', async () => {
    window.history.replaceState({}, '', '/?token=s3cret');
    const fetchMock = agent();
    vi.stubGlobal('fetch', fetchMock);

    await renderApp();
    await screen.findByText('Run control');

    expect(String(fetchMock.mock.calls[0]?.[0])).toBe('/api/v1/session?token=s3cret');
    expect(window.location.search).toBe('');
  });

  it('keeps the token in the URL when the exchange fails, so the operator can retry', async () => {
    window.history.replaceState({}, '', '/?token=wrong');
    vi.stubGlobal('fetch', agent({ sessionStatus: 401 }));

    await renderApp();

    expect((await screen.findByRole('alert')).textContent).toContain('Not authenticated');
    expect(window.location.search).toBe('?token=wrong');
  });

  it('selects the first profile this build can run, and says which ones it cannot', async () => {
    vi.stubGlobal('fetch', agent());

    await renderApp();
    const picker = (await screen.findByLabelText('Profile')) as HTMLSelectElement;

    expect(picker.value).toBe('standard');
    const web = Array.from(picker.options).find((option) => option.value === 'web')!;
    expect(web.disabled).toBe(true);
    expect(web.textContent).toContain('not implemented yet');
  });

  it('shows the calibration warning while the model is uncalibrated', async () => {
    vi.stubGlobal('fetch', agent());

    await renderApp();

    expect((await screen.findByText('Provisional scoring model.')).closest('[role="note"]')?.textContent).toContain(
      'dbs/0.2.0-dev',
    );
  });
});
