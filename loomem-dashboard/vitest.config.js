import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.js'],
    globals: true,
    css: false,
    coverage: {
      provider: 'v8',
      reporter: ['text', 'html'],
      // Skip scanning untested files — works around an @ampproject/remapping
      // crash in @vitest/coverage-v8 v1.x when Node >= 22.5 walks source maps
      // of unreferenced files. Tested files are still reported. Upgrade to
      // coverage-v8 v2.x will re-enable untested-file inclusion.
      all: false,
      exclude: [
        'node_modules/',
        'src/test-setup.js',
        '**/*.config.js',
        '**/__tests__/**',
      ],
    },
  },
});
