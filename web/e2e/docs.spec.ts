import { expect, test } from '@playwright/test';

// SQL docs live-panel tests (Phase 5): the language-neutral SQL pages embed real in-memory jed
// databases that autorun, enforce constraints live, surface SQLSTATE codes, and reset to seed.

test('the types page autoruns a decimal demo (0.1 + 0.2 = 0.3, exactly)', async ({ page }) => {
	await page.goto('/docs/sql/types/');
	// First LiveSql panel = the decimals demo; exact decimal so the sum is 0.3, not 0.30000000004.
	await expect(page.getByTestId('result-rows').first()).toContainText('0.3');
});

test('the types page shows the integer overflow trap (22003)', async ({ page }) => {
	await page.goto('/docs/sql/types/');
	// Second panel = the int16 overflow demo; it autoruns to a 22003 error.
	const overflowPanel = page.getByTestId('live-sql').nth(1);
	await expect(overflowPanel.getByTestId('error-code')).toHaveText('22003');
});

test('the tables page enforces a CHECK constraint live and resets to seed', async ({ page }) => {
	await page.goto('/docs/sql/tables/');
	const panel = page.getByTestId('live-sql');
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');

	await panel.getByTestId('sql-input').fill("INSERT INTO account VALUES (3, 'Bob', -5)");
	await panel.getByTestId('run-button').click();
	await expect(panel.getByTestId('error-code')).toHaveText('23514');

	await panel.getByTestId('reset-button').click();
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the reference pages are generated from the spec', async ({ page }) => {
	await page.goto('/docs/reference/errors/');
	// 54P01 (cost limit) and 23514 (check violation) come straight from spec/errors/registry.toml.
	await expect(page.getByRole('cell', { name: '54P01' })).toBeVisible();
	await expect(page.getByRole('cell', { name: '23514', exact: true })).toBeVisible();
});

test('docs search (Pagefind) returns results from the built index', async ({ page }) => {
	await page.goto('/docs/');
	const input = page.getByTestId('search-input');
	await expect(input).toBeEnabled();
	await input.fill('decimal');
	// Pagefind loads index shards on demand; results appear asynchronously.
	await expect(page.getByTestId('search-results')).toBeVisible();
	await expect(page.getByTestId('search-results')).toContainText('Types');
});
