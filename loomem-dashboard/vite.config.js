import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  // Tailwind v4 via its Vite plugin, not @tailwindcss/postcss: the PostCSS
  // plugin inlines the @fontsource @imports without rebasing their relative
  // url(./files/…), so no self-hosted font ships in the build.
  plugins: [react(), tailwindcss()],
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
