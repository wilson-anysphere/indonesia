import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The VS Code extension unit tests can exceed the 4GB per-process cap in CI/agent
    // environments when Vitest runs with its default parallelism. Constrain worker
    // parallelism by default to keep `npm test` reliable under memory limits.
    maxWorkers: 1,
    fileParallelism: false,
  },
});

