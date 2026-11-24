import { defineConfig } from "vitest/config";

// Runs under the Bun runtime (`bun --bun vitest`): the code and the tests use Bun APIs.
// The pool is `threads`, not vitest's default `forks`: the forks pool's watchdog calls
// `child.channel.unref()`, and Bun's child_process has no `channel.unref`, so every worker fails
// to start under Bun ("child.channel?.unref is not a function").
export default defineConfig({
  test: {
    pool: "threads",
    environment: "node",
    include: ["test/**/*.test.ts"],
    globalSetup: ["../../scripts/test-tmp-scope.ts"],
  },
});
