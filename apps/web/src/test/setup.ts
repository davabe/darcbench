import { cleanup } from '@testing-library/react';
import { afterEach, vi } from 'vitest';

// Testing Library only unmounts automatically when test globals are on, and
// they are deliberately off (see vitest.config.ts).
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

/**
 * jsdom has no `matchMedia`. The dashboard reads exactly one query,
 * `prefers-reduced-motion`, and answers it as *reduce* here: animated
 * count-ups are decoration driven by `requestAnimationFrame`, and a test that
 * reads a score should read the final number, not a frame of an easing curve.
 */
Object.defineProperty(window, 'matchMedia', {
  configurable: true,
  value: (query: string): MediaQueryList =>
    ({
      matches: query.includes('prefers-reduced-motion'),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as MediaQueryList,
});
