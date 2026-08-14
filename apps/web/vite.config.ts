import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

/**
 * The dashboard is compiled into the agent binary, so the build must produce a
 * fully self-contained bundle:
 *
 * - No code splitting. A dynamic import would 404 if a chunk were missed by the
 *   embedding step in `crates/darcbench-agent/build.rs`.
 * - No inlined assets above a small threshold, and no external URLs, so the
 *   agent's `script-src 'self'` CSP holds without exemptions.
 * - Relative-free absolute base (`/`), because the agent serves the SPA from
 *   the site root and rewrites unknown paths to `index.html`.
 */
export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    target: 'es2022',
    outDir: 'dist',
    emptyOutDir: true,
    assetsInlineLimit: 4096,
    sourcemap: false,
    // Deterministic, hash-free names keep the embedded asset table stable
    // across rebuilds, which makes agent binary diffs reviewable.
    rollupOptions: {
      output: {
        manualChunks: undefined,
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name][extname]',
      },
    },
  },
  server: {
    port: 5173,
    // In development the UI runs on Vite and talks to a locally running agent.
    proxy: {
      '/api': { target: 'http://127.0.0.1:7842', changeOrigin: false },
      '/healthz': { target: 'http://127.0.0.1:7842', changeOrigin: false },
    },
  },
});
