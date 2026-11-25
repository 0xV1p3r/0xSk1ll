/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { fileURLToPath, URL } from 'node:url';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  publicDir: '../../../assets/branding',
  resolve: { alias: { '@': fileURLToPath(new URL('./src', import.meta.url)), '@branding': fileURLToPath(new URL('../../../assets/branding', import.meta.url)) } },
  server: { fs: { allow: [fileURLToPath(new URL('.', import.meta.url)), fileURLToPath(new URL('../../../assets/branding', import.meta.url))] } },
  clearScreen: false,
  // Flows drive the whole App and run in parallel, so they get more than the default 5s.
  test: { environment: 'jsdom', setupFiles: ['src/flows/setup.ts'], testTimeout: 15000 },
});
