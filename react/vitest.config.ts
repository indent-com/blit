import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'jsdom',
    globals: true,
    coverage: {
      provider: 'v8',
      include: ['src/**/*.ts', 'src/**/*.tsx'],
      exclude: ['src/__tests__/**'],
      reporter: ['text', 'html'],
      reportsDirectory: 'coverage',
    },
  },
});
