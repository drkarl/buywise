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
			}
		})
	]
});
