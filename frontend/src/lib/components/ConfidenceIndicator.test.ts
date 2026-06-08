import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ConfidenceIndicator from './ConfidenceIndicator.svelte';

describe('ConfidenceIndicator', () => {
	it('renders a visible img badge for `unknown` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'unknown' });
		// role="img" (not "status"): a persistent annotation, not a live region.
		const badge = screen.getByRole('img');
		expect(badge.getAttribute('aria-label')).toMatch(/unknown/i);
	});

	it('renders a visible img badge for `low` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'low' });
		const badge = screen.getByRole('img');
		expect(badge.getAttribute('aria-label')).toMatch(/low/i);
	});

	it('renders nothing for `high` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'high' });
		// High confidence is the quiet common case: no badge at all.
		expect(screen.queryByRole('img')).toBeNull();
	});
});
