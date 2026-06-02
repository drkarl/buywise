import adapter from '@sveltejs/adapter-static';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	compilerOptions: {
		// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
		runes: ({ filename }) => filename.split(/[/\\]/).includes('node_modules') ? undefined : true
	},
	kit: {
		// SPA mode: `adapter-static` with a fallback page produces a pure client
		// SPA with no SvelteKit server routes in production. The fallback file is
		// `200.html`, which the Axum server serves for any unknown deep link
		// (see crates/api/src/lib.rs: ServeDir + ServeFile("200.html")).
		adapter: adapter({
			fallback: '200.html'
		})
	}
};

export default config;
