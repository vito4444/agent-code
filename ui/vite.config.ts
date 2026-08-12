import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// The daemon serves the built assets and owns the API, so dev mode proxies to it rather
// than mocking. Developing against the real event stream is the only way the ordering
// between a prompt response and the notifications that precede it gets exercised.
export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist', emptyOutDir: true },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:8787', ws: true, changeOrigin: true },
    },
  }
});
