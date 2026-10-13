import { defineConfig, mergeConfig } from 'vitest/config';
import viteConfig from './vite.config';

/**
 * Tests run against the same Vite configuration the dashboard is built with,
 * merged rather than duplicated, so a transform that works in the tests and not
 * in the embedded bundle (or the reverse) cannot appear.
 *
 * jsdom rather than a real browser: the logic worth testing here is the event
 * fold, the API client and what each panel says - none of it needs layout, and
 * a browser runner would be a second toolchain in CI for no added coverage.
 */
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      environment: 'jsdom',
      include: ['src/**/*.test.{ts,tsx}'],
      setupFiles: ['src/test/setup.ts'],
      // No globals: every test imports `describe`/`it`/`expect` from `vitest`,
      // so `tsc --noEmit` type-checks the tests with the app's own tsconfig and
      // no ambient test types leak into application code.
      globals: false,
      restoreMocks: true,
      unstubGlobals: true,
    },
  }),
);
