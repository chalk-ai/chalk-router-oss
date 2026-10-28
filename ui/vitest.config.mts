import path from "node:path";
import { defineConfig } from "vitest/config";

/**
 * Minimal vitest setup for the admin UI's `lib/` modules (pure helpers and
 * route-handler-adjacent parsing/validation code). Everything under test
 * runs server-side, so a plain Node environment is enough — no jsdom.
 *
 * The `@/*` alias mirrors tsconfig.json's `paths` so that `lib/*.ts` files
 * (which import generated proto types via `@/gen/...`) resolve the same way
 * under vitest as they do under `tsc`/Next.js.
 */
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    // Pin the timezone so the local-time formatters in lib/format.ts (as
    // opposed to their explicit-UTC counterparts) produce deterministic
    // assertions regardless of the machine/CI running the suite.
    env: { TZ: "UTC" },
  },
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
});
