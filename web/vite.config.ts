import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

// `pnpm dev` proxies the API to a running `cinelog serve` (self-signed TLS). The server
// requires Origin to match its own origin on mutating requests, so the proxy rewrites it.
const target = process.env.CINELOG_DEV_SERVER ?? 'https://localhost:11000';
const proxy = {
  target,
  changeOrigin: true,
  secure: false,
  headers: { origin: target },
};

export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    // The embedded binary serves `assets/*` as immutable (content-hashed names).
    assetsDir: 'assets',
  },
  server: {
    proxy: { '/api': proxy, '/healthz': proxy, '/link': proxy },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts'],
  },
});
