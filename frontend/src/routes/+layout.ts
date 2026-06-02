// SPA mode for adapter-static.
//
// `ssr = false` disables server-side rendering so the app runs entirely in the
// browser — required because IndexedDB (Dexie) and the service worker are
// client-only. `prerender = false` ensures no routes are statically rendered;
// adapter-static instead emits the `fallback` shell (`200.html`) that the Axum
// server serves for every deep link. The result is a pure client SPA.
export const ssr = false;
export const prerender = false;
