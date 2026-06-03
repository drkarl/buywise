// BuyWise service-worker add-on — imported into the Workbox-generated service
// worker via `workbox.importScripts` (see ../vite.config.ts).
//
// SECURITY: the `buywise-api-reads` runtime cache stores session-authenticated,
// household-scoped responses (`GET /api/list/:id`, `GET /api/items`) keyed by
// request URL only — not by session. Left unmanaged, a logout or an account
// switch on a shared device would let the next user read the previous session's
// cached lists/catalogue while offline.
//
// Workbox's `generateSW` mode cannot serialise a custom plugin/handler callback,
// so we own the auth-boundary routes here instead. We intercept same-origin
// `POST /api/auth/login` and `POST /api/auth/logout`, pass them straight to the
// network (these are never cached), and on a *successful* response delete the
// entire auth-scoped cache. This covers an explicit logout AND an account switch
// performed via a fresh login without an intervening logout. The app-shell
// precache holds no user data, so it is deliberately left intact.
const AUTH_BOUNDARY_PATHS = new Set(['/api/auth/login', '/api/auth/logout']);

self.addEventListener('fetch', (event) => {
	const { request } = event;
	if (request.method !== 'POST') return;

	const url = new URL(request.url);
	if (url.origin !== self.location.origin) return;
	if (!AUTH_BOUNDARY_PATHS.has(url.pathname)) return;

	// Take ownership of these routes (NetworkOnly): no Workbox runtime route
	// matches `/api/auth/*`, so this does not conflict with Workbox's router.
	// On network failure (e.g. offline logout) `fetch` rejects and the page sees
	// a normal network error — the same outcome as without a service worker —
	// and the cache is left untouched until a logout actually completes.
	event.respondWith(
		(async () => {
			const response = await fetch(request);
			if (response.ok) {
				// Literal cache name (kept in sync with vite.config.ts's
				// `buywise-api-reads`) so the purge is greppable in the shipped SW.
				await caches.delete('buywise-api-reads');
			}
			return response;
		})()
	);
});
