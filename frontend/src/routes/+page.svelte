<script lang="ts">
	// Home / entry screen.
	//
	// Until now this route was a placeholder ("App UI lands in a later task").
	// The app has no dedicated login route, so this page doubles as the auth gate
	// and the list index:
	//   * logged out (GET /api/auth/me → 401) → a login form that POSTs to
	//     /api/auth/login (which sets the session + csrf cookies);
	//   * logged in → the household's active lists (GET /api/lists) as links into
	//     /list/<id>, a "new list" action, a link to the savings ledger, and
	//     logout.
	//
	// SSR/prerender are disabled globally (src/routes/+layout.ts), so every fetch
	// here runs in the browser with the session cookie attached automatically.

	import { jsonMutationHeaders } from '$lib/api/csrf';

	interface User {
		id: string;
		email: string;
		display_name: string;
	}

	interface ListSummary {
		id: string;
		household_id: string;
		name: string;
		status: string;
		current_revision: number;
		created_at: string;
	}

	type View = 'loading' | 'anon' | 'ready' | 'error';

	let view = $state<View>('loading');
	let user = $state<User | null>(null);
	let lists = $state<ListSummary[]>([]);

	// Login form.
	let email = $state('');
	let password = $state('');
	let loginError = $state<string | null>(null);
	let loggingIn = $state(false);

	// New-list form.
	let newListName = $state('');
	let creating = $state(false);
	let createError = $state<string | null>(null);

	// Sign-out.
	let loggingOut = $state(false);
	let logoutError = $state<string | null>(null);

	/** Reset to the logged-out view, clearing any authenticated state. Called on a
	 * fresh load with no session and whenever a request 401s mid-session (expiry,
	 * or logout in another tab) so stale lists never linger in client state. */
	function goAnon() {
		user = null;
		lists = [];
		loginError = null;
		logoutError = null;
		view = 'anon';
	}

	/** Resolve the current session, then load lists, or fall back to the login form. */
	async function bootstrap() {
		view = 'loading';
		try {
			const meRes = await fetch('/api/auth/me');
			if (meRes.status === 401) {
				goAnon();
				return;
			}
			if (!meRes.ok) throw new Error(`me: ${meRes.status}`);
			user = (await meRes.json()) as User;
			if (!(await loadLists())) return; // 401 → already routed to the login form
			view = 'ready';
		} catch {
			view = 'error';
		}
	}

	/** Fetch the household's lists. Returns false (and routes to the login form) if
	 * the session has expired; throws on any other failure for the caller to handle. */
	async function loadLists(): Promise<boolean> {
		const res = await fetch('/api/lists');
		if (res.status === 401) {
			goAnon();
			return false;
		}
		if (!res.ok) throw new Error(`lists: ${res.status}`);
		lists = (await res.json()) as ListSummary[];
		return true;
	}

	async function login(event: SubmitEvent) {
		event.preventDefault();
		loginError = null;
		loggingIn = true;
		try {
			const res = await fetch('/api/auth/login', {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ email, password })
			});
			if (res.status === 401) {
				password = '';
				loginError = 'Incorrect email or password.';
				return;
			}
			if (!res.ok) {
				loginError = 'Sign-in failed. Please try again.';
				return;
			}
			password = '';
			await bootstrap();
		} catch {
			loginError = 'Could not reach the server.';
		} finally {
			loggingIn = false;
		}
	}

	async function logout() {
		// The session cookie is HttpOnly: only the server can clear it, and only on
		// a CSRF-valid logout (204). So we must NOT drop to the logged-out view
		// unless the server actually ended the session — otherwise we'd claim
		// "signed out" while a reload or another tab stays authenticated.
		logoutError = null;
		loggingOut = true;
		try {
			const res = await fetch('/api/auth/logout', {
				method: 'POST',
				headers: jsonMutationHeaders()
			});
			// 204: server deleted the session + cleared cookies. 401: the session was
			// already gone. Either way we are genuinely logged out.
			if (res.status === 204 || res.status === 401) {
				goAnon();
				return;
			}
			// 403 (missing/stale CSRF) or 5xx: the session is still live server-side,
			// so keep the authenticated view and surface the failure.
			logoutError = 'Could not sign out. Please try again.';
		} catch {
			logoutError = 'Could not sign out. Please try again.';
		} finally {
			loggingOut = false;
		}
	}

	async function createList(event: SubmitEvent) {
		event.preventDefault();
		const name = newListName.trim();
		// Reuse an existing list's household: the API has no "my households"
		// endpoint, and a brand-new household has no list to borrow one from (seed
		// the first list as documented in the README).
		const householdId = lists[0]?.household_id;
		if (!name || !householdId) return;
		createError = null;
		creating = true;
		try {
			const res = await fetch('/api/lists', {
				method: 'POST',
				headers: jsonMutationHeaders(),
				body: JSON.stringify({ name, household_id: householdId })
			});
			if (res.status === 401 || res.status === 403) {
				goAnon(); // session expired (or CSRF lost with it) → back to the login form
				return;
			}
			if (!res.ok) {
				createError = 'Could not create the list.';
				return;
			}
			newListName = '';
			await loadLists();
		} catch {
			createError = 'Could not reach the server.';
		} finally {
			creating = false;
		}
	}

	$effect(() => {
		bootstrap();
	});
</script>

<main class="home">
	<header class="masthead">
		<h1>BuyWise</h1>
		<p class="tagline">Shared shopping lists that work offline.</p>
	</header>

	{#if view === 'loading'}
		<p class="status" aria-live="polite">Loading…</p>
	{:else if view === 'error'}
		<div class="panel">
			<p class="status">Couldn't reach the server.</p>
			<button class="btn" onclick={bootstrap}>Retry</button>
		</div>
	{:else if view === 'anon'}
		<form class="panel login" onsubmit={login}>
			<h2>Sign in</h2>
			<label>
				<span>Email</span>
				<input type="email" bind:value={email} autocomplete="username" required />
			</label>
			<label>
				<span>Password</span>
				<input
					type="password"
					bind:value={password}
					autocomplete="current-password"
					required
				/>
			</label>
			{#if loginError}
				<p class="error" role="alert">{loginError}</p>
			{/if}
			<button class="btn btn-primary" type="submit" disabled={loggingIn}>
				{loggingIn ? 'Signing in…' : 'Sign in'}
			</button>
		</form>
	{:else}
		<div class="panel">
			<div class="who">
				<span>Signed in as <strong>{user?.display_name}</strong></span>
				<button class="btn btn-quiet" onclick={logout} disabled={loggingOut}>
					{loggingOut ? 'Signing out…' : 'Sign out'}
				</button>
			</div>
			{#if logoutError}
				<p class="error" role="alert">{logoutError}</p>
			{/if}

			<h2>Your lists</h2>
			{#if lists.length === 0}
				<p class="status">
					No lists yet. Seed one as described in the README, then it will appear here.
				</p>
			{:else}
				<ul class="lists">
					{#each lists as list (list.id)}
						<li>
							<a class="list-link" href={`/list/${list.id}`}>
								<span class="list-name">{list.name}</span>
								<span class="chev" aria-hidden="true">→</span>
							</a>
						</li>
					{/each}
				</ul>
			{/if}

			{#if lists.length > 0}
				<form class="new-list" onsubmit={createList}>
					<input
						type="text"
						placeholder="New list name"
						bind:value={newListName}
						aria-label="New list name"
					/>
					<button
						class="btn btn-primary"
						type="submit"
						disabled={creating || !newListName.trim()}
					>
						{creating ? 'Creating…' : 'Add list'}
					</button>
				</form>
				{#if createError}
					<p class="error" role="alert">{createError}</p>
				{/if}
			{/if}

			<p class="ledger-link"><a href="/savings">View your savings ledger →</a></p>
		</div>
	{/if}
</main>

<style>
	.home {
		max-width: 40rem;
		margin: 0 auto;
		padding: 2rem 1rem 3rem;
		display: flex;
		flex-direction: column;
		gap: 1.5rem;
	}

	.masthead {
		text-align: center;
	}
	.masthead h1 {
		font-size: 2rem;
		color: var(--color-accent);
		margin-bottom: 0.25rem;
	}
	.tagline {
		color: var(--color-text-subtle);
		margin: 0;
	}

	.panel {
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		box-shadow: var(--shadow-card);
		padding: 1.25rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.panel h2 {
		font-size: 1.15rem;
		margin: 0;
	}

	.login label {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		font-size: 0.9rem;
		color: var(--color-text-muted);
	}

	input {
		width: 100%;
		padding: 0.55rem 0.7rem;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-sm);
		background: var(--color-surface);
	}
	input:focus {
		outline: none;
		border-color: var(--color-accent);
		box-shadow: 0 0 0 2px var(--color-accent-soft);
	}

	.btn {
		padding: 0.55rem 0.9rem;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-sm);
		background: var(--color-surface-alt);
		font-weight: 600;
		color: var(--color-text);
	}
	.btn-primary {
		background: var(--color-accent);
		border-color: var(--color-accent);
		color: #fff;
	}
	.btn-primary:hover:not(:disabled) {
		background: var(--color-accent-strong);
	}
	.btn-quiet {
		background: transparent;
		border-color: transparent;
		color: var(--color-accent);
		padding: 0.25rem 0.4rem;
		font-size: 0.9rem;
	}

	.who {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
		font-size: 0.9rem;
		color: var(--color-text-subtle);
	}

	.lists {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.list-link {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.75rem 0.9rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
		background: var(--color-surface-alt);
		color: var(--color-text);
		text-decoration: none;
	}
	.list-link:hover {
		border-color: var(--color-accent);
		background: var(--color-accent-soft);
		text-decoration: none;
	}
	.list-name {
		font-weight: 600;
	}
	.chev {
		color: var(--color-accent);
	}

	.new-list {
		display: flex;
		gap: 0.5rem;
	}
	.new-list input {
		flex: 1;
	}

	.status {
		color: var(--color-text-subtle);
		margin: 0;
	}
	.error {
		color: var(--color-danger);
		background: var(--color-danger-soft);
		border-radius: var(--radius-sm);
		padding: 0.5rem 0.65rem;
		margin: 0;
		font-size: 0.9rem;
	}
	.ledger-link {
		margin: 0;
		font-size: 0.9rem;
	}
</style>
