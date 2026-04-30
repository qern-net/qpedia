import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

const API = process.env.QPEDIA_API ?? 'http://127.0.0.1:18080';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    proxy: {
      '/api':     { target: API, changeOrigin: true },
      '/healthz': { target: API, changeOrigin: true }
    }
  }
});
