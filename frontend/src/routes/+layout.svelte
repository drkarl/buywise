<script lang="ts">
	import { onMount } from 'svelte';
	import { pwaInfo } from 'virtual:pwa-info';
	import favicon from '$lib/assets/favicon.svg';

	let { children } = $props();

	// With SSR/prerender disabled (see +layout.ts), `<svelte:head>` content is
	// injected client-side into the static `200.html` shell at runtime, so the
	// manifest link and service-worker registration must happen here rather than
	// during prerender (the build emits no prerendered HTML to inject into).
	const webManifestLink = $derived(pwaInfo ? pwaInfo.webManifest.linkTag : '');

	onMount(async () => {
		const { registerSW } = await import('virtual:pwa-register');
		registerSW({ immediate: true });
	});
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<!-- webManifestLink is a plugin-generated <link> tag, not user input. -->
	{@html webManifestLink}
</svelte:head>

{@render children()}
