# BuyWise frontend

The BuyWise client: a SvelteKit PWA for shared, offline-first shopping lists.

It runs as a pure client-side SPA (`adapter-static` with `ssr = false`,
`prerender = false`). Local data is persisted in IndexedDB via Dexie, and a
service worker (vite-pwa) provides offline caching and installability.

## Development

```sh
npm install
npm run dev
```

## Build & serve

```sh
npm run build      # type-check / lint
npm run check
```

`npm run build` emits the static SPA to `frontend/build/`, including the
fallback shell `200.html`. In production the Axum binary serves this directory
via `ServeDir`, falling back to `200.html` for any unknown deep link
(see `crates/api/src/lib.rs`). The PWA manifest link and service-worker
registration are wired up client-side in `src/routes/+layout.svelte` because
the SPA shell is not server-rendered.

## Tests

Unit/component tests run under Vitest and are unaffected by the e2e suite:

```sh
npm test           # Vitest (src/**)
```

End-to-end tests for the P2-T4 sync loop (issue #63) run under Playwright in
`e2e/`. They drive **two browser contexts** against a real backend to exercise
live SSE sync, offline → reconnect queue drain, rejected-mutation reconcile, the
drain-vs-SSE race, and full Dexie ⇄ `GET /api/list/:id` convergence:

```sh
# One-time: install the browser (CI adds --with-deps)
npx playwright install chromium

# Build the prerequisites the e2e webServer needs, then run:
cargo build --release -p buywise-api -p buywise-cli   # from the repo root
npm run build                                         # SPA the backend serves
npm run test:e2e
```

The whole app (API + SPA) is served from one origin by `buywise-api`, so the
suite needs no proxy. `e2e/run-server.sh` is Playwright's `webServer`: it boots
the backend on a fresh SQLite file, lets it migrate, and seeds a user +
household + list via `e2e/seed.sh` (the backend has no register endpoint — auth
is password login only). See `playwright.config.ts` and `.github/workflows/e2e.yml`.
