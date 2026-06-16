import { expect, test } from '@playwright/test';

// Engine bridge smoke tests (Phase 2 / Phase 3): the in-memory jed engine runs in a real browser via
// the shared Web Worker, results render in the grid, structured SQLSTATE errors surface, and the cost
// ceiling stops a runaway query instead of hanging the tab.

test('home hero autoruns a seeded query and renders rows', async ({ page }) => {
	await page.goto('/');
	const rows = page.getByTestId('result-rows');
	await expect(rows).toContainText('Grace');
	await expect(rows).toContainText('99.25');
	// Ada (91.5) is > 90 and present; Linus (88) is filtered out by WHERE score > 90.
	await expect(rows).toContainText('Ada');
	await expect(rows).not.toContainText('Linus');
});

test('editing and running executes against the same in-memory database', async ({ page }) => {
	await page.goto('/');
	await expect(page.getByTestId('run-button')).toBeEnabled();
	await page.getByTestId('sql-input').fill('SELECT count(*) AS n FROM person');
	await page.getByTestId('run-button').click();
	await expect(page.getByTestId('result-rows')).toContainText('3');
});

test('a SQL error surfaces its SQLSTATE code', async ({ page }) => {
	await page.goto('/');
	await expect(page.getByTestId('run-button')).toBeEnabled();
	await page.getByTestId('sql-input').fill('SELECT * FROM nonexistent_table');
	await page.getByTestId('run-button').click();
	await expect(page.getByTestId('result-error')).toBeVisible();
	// Undefined table is 42P01.
	await expect(page.getByTestId('error-code')).toHaveText('42P01');
});

test('a runaway query trips the cost ceiling (54P01), not a hang', async ({ page }) => {
	await page.goto('/');
	await expect(page.getByTestId('run-button')).toBeEnabled();
	await page.getByTestId('sql-input').fill('SELECT * FROM generate_series(1, 100000000)');
	await page.getByTestId('run-button').click();
	await expect(page.getByTestId('error-code')).toHaveText('54P01');
});

test('reset restores the widget to its seed + initial query', async ({ page }) => {
	await page.goto('/');
	await expect(page.getByTestId('run-button')).toBeEnabled();
	await page.getByTestId('sql-input').fill('DROP TABLE person');
	await page.getByTestId('run-button').click();
	await page.getByTestId('reset-button').click();
	// After reset the seed re-creates person and the initial query runs again.
	await expect(page.getByTestId('result-rows')).toContainText('Grace');
});
