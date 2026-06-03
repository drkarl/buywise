// Test stub for SvelteKit's `$app/state` virtual module (which only exists
// inside a running kit server). The vitest config aliases `$app/state` here so
// component imports resolve; tests override `page.params` via `vi.mock`.
export const page = { params: {} as Record<string, string> };
