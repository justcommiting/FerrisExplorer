/// <reference types="vitest/config" />
import path from 'node:path'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Tauri expects a fixed port and must see Rust errors in the terminal.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    target: 'es2022',
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
  test: {
    include: ['src/**/*.test.ts'],
  },
})
