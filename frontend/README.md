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
