import { defineConfig, devices } from '@playwright/test';

// Playwright end-to-end suite for the P2-T4 client sync loop (issue #63).
//
// The whole app — JSON API and the built SvelteKit SPA — is served from ONE
// origin by the Axum backend (`crates/api/src/lib.rs::app`): API under `/api/*`,
// everything else falls back to the SPA shell `200.html`. So the suite drives a
// single real backend on `BASE_URL`; there is no Vite dev server, no proxy, and
// no CORS. `e2e/run-server.sh` is the `webServer` command: it boots the release
// `buywise-api`, lets it migrate a fresh SQLite file, and seeds a user +
// household + list (the backend has no register endpoint — see e2e/seed.sh).
//
// Browsers must be installed first (`npx playwright install chromium`); the CI
// job adds `--with-deps`. The suite is intentionally Chromium-only: it relies on
// Chromium's `context.setOffline()` and on localhost being treated as a secure
// context (so the `Secure` session/csrf cookies work over http).

const PORT = Number(process.env.E2E_PORT ?? 3000);
const BASE_URL = process.env.E2E_BASE_URL ?? `http://localhost:${PORT}`;

export default defineConfig({
  testDir: './e2e',
  // Match only the spec files; helpers (fixtures.ts) and shell scripts are
  // excluded so they are never picked up as test files.
  testMatch: /.*\.spec\.ts$/,
  // The single shared backend is stateful (one list, two contexts converging),
  // so specs must not run concurrently against it. One worker, serial.
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  // Sync convergence (drain + SSE + snapshot reconcile) is the unit under test;
  // give each test room for the ~1s SSE propagation plus reconnect drains.
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI
    ? [['html', { open: 'never' }], ['list']]
    : [['list']],
  use: {
    baseURL: BASE_URL,
    // Capture a trace on the first retry so a CI failure is debuggable without
    // re-running locally; screenshots/videos only on failure to keep artifacts small.
    trace: 'on-first-retry',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure'
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] }
    }
  ],
  webServer: {
    command: 'bash e2e/run-server.sh',
    url: `${BASE_URL}/api/auth/me`,
    // /api/auth/me returns 401 when unauthenticated — that is a healthy "up"
    // signal, so accept any status rather than only 2xx.
    ignoreHTTPSErrors: true,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    env: {
      PORT: String(PORT)
    }
  }
});
