/**
 * Typed client for the agent API.
 *
 * Auth, mirroring `crates/darcbench-agent/src/server.rs`:
 *
 * 1. The agent prints a URL containing `?token=`.
 * 2. On load the UI POSTs it to `/api/v1/session`, which returns an HttpOnly,
 *    SameSite=Strict cookie, then strips the token from the address bar.
 * 3. The token is kept in memory and sent as `Authorization: Bearer` on every
 *    mutating call. The agent *refuses* cookie-authenticated mutation, so this
 *    is not optional - it is the CSRF defence, and a UI that "simplified" it
 *    away would simply stop being able to start runs.
 * 4. `EventSource` cannot set headers, so the SSE stream rides the cookie.
 */

import type { AgentMeta, Inventory, ProfileInfo, RunComparison, RunListEntry } from './types';

export class ApiError extends Error {
  constructor(
    message: string,
    readonly code: string,
    readonly status: number,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

let bearer: string | null = null;

export function bootstrapToken(): string | null {
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  if (token) bearer = token;
  return token;
}

/** Removes the token from the URL so it never reaches history or a Referer. */
export function scrubUrl(): void {
  window.history.replaceState({}, '', window.location.pathname);
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (bearer) headers.set('Authorization', `Bearer ${bearer}`);
  if (init.body) headers.set('Content-Type', 'application/json');

  const response = await fetch(path, { ...init, headers, credentials: 'same-origin' });
  if (!response.ok) {
    let code = 'http_error';
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { code?: string; message?: string; detail?: string };
      code = body.code ?? code;
      message = body.detail ? `${body.message} (${body.detail})` : (body.message ?? message);
    } catch {
      // A non-JSON error body is still an error; keep the status line.
    }
    throw new ApiError(message, code, response.status);
  }
  return (await response.json()) as T;
}

export const api = {
  async openSession(token: string): Promise<void> {
    await request(`/api/v1/session?token=${encodeURIComponent(token)}`, { method: 'POST' });
  },

  meta(): Promise<AgentMeta> {
    return request<AgentMeta>('/api/v1/meta');
  },

  async inventory(): Promise<{ inventory: Inventory; redacted: boolean; performance_digest: string }> {
    return request('/api/v1/inventory');
  },

  async profiles(): Promise<ProfileInfo[]> {
    const data = await request<{ profiles: ProfileInfo[] }>('/api/v1/profiles');
    return data.profiles;
  },

  /**
   * Every run this agent knows about: the one in flight, and the history.
   *
   * The endpoint merges the run manager with the on-disk index, so this is also
   * the only way to see runs executed by a previous agent process. Bounded
   * server-side at 200 historical rows; pagination arrives with the fleet views,
   * so a caller that needs "all of them" does not exist yet.
   */
  async runs(): Promise<RunListEntry[]> {
    const data = await request<{ runs: RunListEntry[] }>('/api/v1/runs');
    return data.runs;
  },

  /**
   * Two runs lined up metric by metric.
   *
   * Answered from the run index rather than by parsing two bundles, which is why
   * a run that has not been indexed yet - one still in flight, or whose bundle
   * was pruned - comes back `404 unknown_run` rather than as an empty
   * comparison. Both ids are path segments, so both are encoded: they are
   * validated as run ids server-side, and a caller that pasted something else
   * should get `invalid_run_id` rather than a mangled route.
   */
  compare(baselineRunId: string, candidateRunId: string): Promise<RunComparison> {
    return request<RunComparison>(
      `/api/v1/runs/${encodeURIComponent(baselineRunId)}/compare/${encodeURIComponent(candidateRunId)}`,
    );
  },

  startRun(profile: string, force: boolean): Promise<{ run_id: string; events_url: string }> {
    return request('/api/v1/runs', {
      method: 'POST',
      body: JSON.stringify({ profile, force }),
    });
  },

  cancelRun(runId: string): Promise<{ run_id: string; cancelling: boolean }> {
    return request(`/api/v1/runs/${encodeURIComponent(runId)}/cancel`, { method: 'POST' });
  },

  reportUrl(runId: string): string {
    return `/api/v1/runs/${encodeURIComponent(runId)}/report`;
  },

  bundleUrl(runId: string): string {
    return `/api/v1/runs/${encodeURIComponent(runId)}/bundle`;
  },

  eventsUrl(runId: string): string {
    return `/api/v1/runs/${encodeURIComponent(runId)}/events`;
  },
};
