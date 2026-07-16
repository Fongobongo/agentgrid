import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Dev server proxies API calls to the control plane (Stage 4.3). The control
// plane also serves the built `dist/` directly in production.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/v1': 'http://127.0.0.1:7800',
      '/metrics': 'http://127.0.0.1:7800',
      '/health': 'http://127.0.0.1:7800',
    },
  },
  build: {
    outDir: 'dist',
  },
});
