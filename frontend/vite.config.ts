import { sveltekit } from '@sveltejs/kit/vite';
import { SvelteKitPWA } from '@vite-pwa/sveltekit';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit(),
		SvelteKitPWA({
			// Service worker silently updates in the background, then activates on
			// the next navigation — appropriate for an offline-first shopping list.
			registerType: 'autoUpdate',
			// This is a pure SPA (adapter-static with `fallback: '200.html'`), so
			// there is no prerendered `index.html` for the SW to fall back to. Tell
			// the plugin about the SPA shell so it (1) precaches `200.html` with a
			// real revision and (2) points the navigation route at it. Without this,
			// the SW registers `createHandlerBoundToURL('/')`, targeting a URL that
			// was never written or precached — so offline reloads and deep-link
			// navigations fail once the SW takes control, breaking offline-first.
			kit: {
				spa: true,
				adapterFallback: '200.html'
			},
			manifest: {
				name: 'BuyWise',
				short_name: 'BuyWise',
				description: 'Shared shopping lists that work offline.',
				theme_color: '#0f766e',
				background_color: '#ffffff',
				display: 'standalone',
				icons: [
					{
						src: 'pwa-192x192.png',
						sizes: '192x192',
						type: 'image/png'
					},
					{
						src: 'pwa-512x512.png',
						sizes: '512x512',
						type: 'image/png'
					},
					{
						src: 'pwa-512x512.png',
						sizes: '512x512',
						type: 'image/png',
						purpose: 'maskable'
					}
				]
			},
			workbox: {
				// Precaching: leave `globPatterns` at the generateSW default
				// (`**/*.{js,css,html,...}` under the build's `globDirectory`) so
				// every JS/CSS chunk and the SPA shell are precached. The
				// `@vite-pwa/sveltekit` plugin additionally appends the SvelteKit
				// `client/**` and `prerendered/**` patterns when `globPatterns` is
				// set; we deliberately do NOT set it so the broader default stands.
				//
				// `navigateFallback` is intentionally omitted: the plugin sets it to
				// `adapterFallback` ('200.html') for us (see kit.adapterFallback
				// above). Overriding it here would break offline deep-link routing.
				runtimeCaching: [
					{
						// CacheFirst for static assets NOT covered by the precache
						// manifest — chiefly images and fonts fetched at runtime
						// (e.g. user/category icons). JS/CSS chunks are already
						// precached, so they never reach this rule.
						urlPattern: ({ request, sameOrigin }) =>
							sameOrigin &&
							(request.destination === 'image' || request.destination === 'font'),
						handler: 'CacheFirst',
						options: {
							cacheName: 'buywise-static-assets',
							expiration: {
								maxEntries: 60,
								maxAgeSeconds: 60 * 60 * 24 * 30 // 30 days
							},
							cacheableResponse: {
								// Opaque (status 0) responses are cached too, so
								// cross-origin/no-cors fonts still work offline.
								statuses: [0, 200]
							}
						}
					},
					{
						// NetworkFirst with cache fallback for the two cacheable API
						// reads ONLY: `GET /api/list/:id` and `GET /api/items`.
						//
						// We match on `url.pathname` (not the whole href) with a
						// matcher function so we can also pin the method to GET. This
						// guarantees mutations (`POST /api/mutations`, `POST/PATCH`
						// on lists) and the SSE stream (`GET /api/sse/:id`) are never
						// cached. The singular `/api/list/<id>` pattern requires a
						// non-empty id segment, so it never matches plural
						// `/api/lists` or the bare `/api/list`.
						urlPattern: ({ url, request, sameOrigin }) =>
							sameOrigin &&
							request.method === 'GET' &&
							(/^\/api\/list\/[^/]+$/.test(url.pathname) ||
								/^\/api\/items\/?$/.test(url.pathname)),
						handler: 'NetworkFirst',
						options: {
							cacheName: 'buywise-api-reads',
							// Fall back to the cache quickly when offline / on a flaky
							// network rather than hanging on the failed request.
							networkTimeoutSeconds: 5,
							expiration: {
								maxEntries: 50,
								maxAgeSeconds: 60 * 60 * 24 // 1 day
							},
							cacheableResponse: {
								statuses: [200]
							}
						}
					}
				]
			}
		})
	]
});
