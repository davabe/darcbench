import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * `api.ts` keeps the bearer token in module state, which is the point - it
 * never touches storage - so each test imports a fresh copy of the module.
 */
async function freshApi() {
  vi.resetModules();
  return import('./api');
}

function json(body: unknown, init: ResponseInit = {}) {
  return new Response(JSON.stringify(body), { status: 200, ...init, headers: { 'Content-Type': 'application/json' } });
}

let fetchMock: ReturnType<typeof vi.fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>>;

function lastCall() {
  const [path, init] = fetchMock.mock.calls.at(-1)!;
  return { path: String(path), init: init!, headers: new Headers(init!.headers) };
}

beforeEach(() => {
  fetchMock = vi.fn(async () => json({}));
  vi.stubGlobal('fetch', fetchMock);
  window.history.replaceState({}, '', '/');
});

afterEach(() => {
  window.history.replaceState({}, '', '/');
});

describe('authentication', () => {
  it('sends the printed token as a bearer on every request', async () => {
    window.history.replaceState({}, '', '/?token=s3cret');
    const { api, bootstrapToken } = await freshApi();

    expect(bootstrapToken()).toBe('s3cret');
    await api.meta();

    expect(lastCall().headers.get('Authorization')).toBe('Bearer s3cret');
    expect(lastCall().init.credentials).toBe('same-origin');
  });

  it('sends no Authorization header when there is no token', async () => {
    const { api, bootstrapToken } = await freshApi();

    expect(bootstrapToken()).toBeNull();
    await api.meta();

    expect(lastCall().headers.has('Authorization')).toBe(false);
  });

  it('removes the token from the address bar', async () => {
    window.history.replaceState({}, '', '/dashboard?token=s3cret');
    const { scrubUrl } = await freshApi();

    scrubUrl();

    expect(window.location.search).toBe('');
    expect(window.location.pathname).toBe('/dashboard');
  });

  it('exchanges the token for a session with the token URL-encoded', async () => {
    const { api } = await freshApi();

    await api.openSession('a&b=c');

    expect(lastCall().path).toBe('/api/v1/session?token=a%26b%3Dc');
    expect(lastCall().init.method).toBe('POST');
  });
});

describe('requests', () => {
  it('sends a JSON body, declared as JSON, only when there is one', async () => {
    const { api } = await freshApi();

    await api.startRun('standard', true);
    expect(lastCall().init.method).toBe('POST');
    expect(lastCall().headers.get('Content-Type')).toBe('application/json');
    expect(JSON.parse(String(lastCall().init.body))).toEqual({ profile: 'standard', force: true });

    await api.meta();
    expect(lastCall().headers.has('Content-Type')).toBe(false);
  });

  it('encodes run ids as path segments', async () => {
    const { api } = await freshApi();

    await api.compare('run/../a', 'b?c');
    expect(lastCall().path).toBe('/api/v1/runs/run%2F..%2Fa/compare/b%3Fc');

    expect(api.eventsUrl('a/b')).toBe('/api/v1/runs/a%2Fb/events');
    expect(api.reportUrl('a/b')).toBe('/api/v1/runs/a%2Fb/report');
    expect(api.bundleUrl('a/b')).toBe('/api/v1/runs/a%2Fb/bundle');
  });

  it('unwraps list endpoints', async () => {
    const { api } = await freshApi();
    fetchMock.mockResolvedValueOnce(json({ runs: [{ run_id: 'run_a' }] }));

    await expect(api.runs()).resolves.toEqual([{ run_id: 'run_a' }]);
  });
});

describe('errors', () => {
  it('carries the agent’s code and status, with its detail in the message', async () => {
    const { api, ApiError } = await freshApi();
    fetchMock.mockResolvedValueOnce(
      json({ code: 'run_in_progress', message: 'A run is already in progress', detail: 'run_x' }, { status: 409 }),
    );

    const error = await api.startRun('quick', false).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({
      code: 'run_in_progress',
      status: 409,
      message: 'A run is already in progress (run_x)',
    });
  });

  it('keeps the status line when the error body is not JSON', async () => {
    const { api } = await freshApi();
    fetchMock.mockResolvedValueOnce(new Response('upstream died', { status: 502, statusText: 'Bad Gateway' }));

    await expect(api.meta()).rejects.toMatchObject({ code: 'http_error', status: 502, message: '502 Bad Gateway' });
  });
});
