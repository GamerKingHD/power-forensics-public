import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// Tauri expects a fixed dev port; the Rust shell embeds ../dist for builds.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: 'es2022',
    sourcemap: false,
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./vitest.setup.ts'],
    css: false,
    // Components debounce bridge calls; give loaded CI runners room well above
    // the testing-library async-utility timeout so a slow poll cannot trip the
    // per-test timeout first.
    testTimeout: 30000,
    hookTimeout: 30000,
  },
})
