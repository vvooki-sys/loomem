import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 3031,
    proxy: {
      // Proxy API calls to loomem-server in dev; in production the SPA is
      // embedded in the server binary and served same-origin, so no proxy.
      '/v1': {
        target: 'http://localhost:3030',
        changeOrigin: true,
      },
      '/admin': {
        target: 'http://localhost:3030',
        changeOrigin: true,
      },
      '/api': {
        target: 'http://localhost:3030',
        changeOrigin: true,
      },
      '/health': {
        target: 'http://localhost:3030',
        changeOrigin: true,
      },
      '/mcp': {
        target: 'http://localhost:3030',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    // Embedded into the loomem-server binary via rust-embed.
    assetsDir: 'assets',
  },
});
