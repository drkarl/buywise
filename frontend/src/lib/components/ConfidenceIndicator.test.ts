import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import ConfidenceIndicator from './ConfidenceIndicator.svelte';

describe('ConfidenceIndicator', () => {
	it('renders a visible status badge for `unknown` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'unknown' });
		const badge = screen.getByRole('status');
		expect(badge.getAttribute('aria-label')).toMatch(/unknown/i);
	});

	it('renders a visible status badge for `low` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'low' });
		const badge = screen.getByRole('status');
		expect(badge.getAttribute('aria-label')).toMatch(/low/i);
	});

	it('renders nothing for `high` confidence', () => {
		render(ConfidenceIndicator, { confidence: 'high' });
		// High confidence is the quiet common case: no badge at all.
		expect(screen.queryByRole('status')).toBeNull();
	});
});
