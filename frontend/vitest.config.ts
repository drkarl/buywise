import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// package.json is `type: "module"`, so `__dirname` is undefined in ESM. Derive
// the directory from the module URL instead.
const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Dedicated Vitest config: we use the bare Svelte plugin (not the full
// SvelteKit/PWA stack from vite.config.ts) so component tests run fast and the
// service-worker plugin never touches the test build. The `$lib` alias is wired
// manually since we bypass SvelteKit's resolver here.
export default defineConfig({
	plugins: [svelte(), svelteTesting()],
	resolve: {
		alias: {
			$lib: path.resolve(__dirname, './src/lib'),
			// SvelteKit's `$app/*` virtual modules don't exist outside a running
			// kit server; point `$app/state` at a test stub so Vite can resolve the
			// import. Individual tests still `vi.mock('$app/state')` to set params.
			'$app/state': path.resolve(__dirname, './src/test/app-state-stub.ts')
		},
		// Use the browser entry points (e.g. Svelte client runtime) under jsdom.
		conditions: ['browser']
	},
	test: {
		environment: 'jsdom',
		globals: true,
		setupFiles: ['./vitest-setup.ts'],
		include: ['src/**/*.{test,spec}.{ts,svelte.ts}']
	}
});
