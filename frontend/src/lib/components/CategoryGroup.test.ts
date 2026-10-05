import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import CategoryGroup from './CategoryGroup.svelte';
import type { LocalListEntry } from '$lib/db/dexie';
import type { CategoryGroup as CategoryGroupData } from '$lib/shopping/sortByStoreOrder';

const item: LocalListEntry = {
	id: 'entry-1',
	list_id: 'list-1',
	resolution_status: 'resolved',
	raw_text: 'Milk',
	display_name: 'Milk',
	quantity: 1,
	status: 'active',
	last_updated_revision: 0,
	updated_at: '2026-01-01T00:00:00.000Z'
};

describe('CategoryGroup', () => {
	it('marks a shopping-mode item while its mutation is pending', () => {
		const group: CategoryGroupData = {
			category_id: '__uncategorised__',
			items: [item],
			remaining: 1
		};
		render(CategoryGroup, {
			props: { group, pendingIds: new Set([item.id]), onbought: vi.fn() }
		});

		expect(screen.getByRole('img', { name: 'Not synced yet' })).toBeVisible();
	});
});
