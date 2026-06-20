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
	// Second panel = the i16 overflow demo; it autoruns to a 22003 error.
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

test('the tables page enforces a FOREIGN KEY constraint live (23503) and resets to seed', async ({
	page,
}) => {
	await page.goto('/docs/sql/tables/');
	const panel = page.getByTestId('live-sql');
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');

	// A child row referencing a non-existent account traps 23503.
	await panel.getByTestId('sql-input').fill('INSERT INTO txn VALUES (3, 99, 5)');
	await panel.getByTestId('run-button').click();
	await expect(panel.getByTestId('error-code')).toHaveText('23503');

	// The parent side: deleting an account still referenced by a txn also traps 23503.
	await panel.getByTestId('sql-input').fill('DELETE FROM account WHERE id = 1');
	await panel.getByTestId('run-button').click();
	await expect(panel.getByTestId('error-code')).toHaveText('23503');

	await panel.getByTestId('reset-button').click();
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the tables page upserts with ON CONFLICT DO UPDATE (excluded) live', async ({ page }) => {
	await page.goto('/docs/sql/tables/');
	const panel = page.getByTestId('live-sql');
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');

	// Account 1 already exists, so instead of a 23505 the row is updated: balance 100 + 100 = 200.
	await panel
		.getByTestId('sql-input')
		.fill(
			"INSERT INTO account VALUES (1, 'Ada', 100.00) ON CONFLICT (id) DO UPDATE SET balance = account.balance + excluded.balance RETURNING id, balance",
		);
	await panel.getByTestId('run-button').click();
	await expect(panel.getByTestId('result-rows')).toContainText('200.00');

	await panel.getByTestId('reset-button').click();
	await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the select page runs the LATERAL top-N-per-group demo live', async ({ page }) => {
	await page.goto('/docs/sql/select/');
	// Sixth LiveSql panel = the CROSS JOIN LATERAL demo: the priciest product of each category —
	// kitchen → Mug (12.50), office → Notebook (4.00); the dependent subquery re-runs per category.
	const panel = page.getByTestId('live-sql').nth(5);
	await expect(panel.getByTestId('result-rows')).toContainText('Mug');
	await expect(panel.getByTestId('result-rows')).toContainText('Notebook');
});

test('the select page runs the array containment operators live (@> / <@ / &&)', async ({ page }) => {
	await page.goto('/docs/sql/select/');
	// Eighth LiveSql panel = the @>/<@/&& demo; every column is true (the array sets contain/overlap).
	const panel = page.getByTestId('live-sql').nth(7);
	await expect(panel.getByTestId('result-rows')).toContainText('true');
});

test('the select page runs the ANY/ALL quantified comparisons live', async ({ page }) => {
	await page.goto('/docs/sql/select/');
	// Ninth LiveSql panel = the = ANY / > ALL demo: any_match true, all_greater true, no_match false.
	const panel = page.getByTestId('live-sql').nth(8);
	await expect(panel.getByTestId('result-rows')).toContainText('true');
	await expect(panel.getByTestId('result-rows')).toContainText('false');
});

test('the select page runs the VARIADIC num_nulls demo live', async ({ page }) => {
	await page.goto('/docs/sql/select/');
	// Eleventh LiveSql panel = the num_nulls demo: spread 1, variadic 1, non_nulls 2 (both forms agree).
	const panel = page.getByTestId('live-sql').nth(10);
	await expect(panel.getByTestId('result-rows')).toContainText('1');
	await expect(panel.getByTestId('result-rows')).toContainText('2');
});

test('the select page runs the array-of-composite demo live (field access into array elements)', async ({
	page,
}) => {
	await page.goto('/docs/sql/select/');
	// Twelfth LiveSql panel = the addr[] demo: row 1's first element renders (Main,90210), its street
	// is Main and zip 90210 — array-of-composite construction, subscript, and field access (§12 AC1).
	const panel = page.getByTestId('live-sql').nth(11);
	await expect(panel.getByTestId('result-rows')).toContainText('(Main,90210)');
	await expect(panel.getByTestId('result-rows')).toContainText('Main');
	await expect(panel.getByTestId('result-rows')).toContainText('90210');
});

test('the select page runs the composite-with-array-field demo live (field access + subscript)', async ({
	page,
}) => {
	await page.goto('/docs/sql/select/');
	// Fifteenth LiveSql panel = the poly(name, pts i32[]) demo: row 1 renders its array field
	// {10,20,30}, (p).pts reads the whole array, and (p).pts[1] reads the first element 10 —
	// a composite type with an array-typed field (array.md §12, the mirror of array-of-composite).
	const panel = page.getByTestId('live-sql').nth(14);
	await expect(panel.getByTestId('result-rows')).toContainText('{10,20,30}');
	await expect(panel.getByTestId('result-rows')).toContainText('10');
});

test('the select page runs the date demo live', async ({ page }) => {
	await page.goto('/docs/sql/select/');
	// Sixteenth LiveSql panel = the date demo: WHERE on_day < '2024-03-01' ORDER BY on_day keeps
	// only the two early dates, chronologically — review (2023-11-02) then launch (2024-01-15); the
	// 2024-06-15 and infinity rows are filtered out (spec/design/date.md).
	const panel = page.getByTestId('live-sql').nth(15);
	await expect(panel.getByTestId('result-rows')).toContainText('review');
	await expect(panel.getByTestId('result-rows')).toContainText('launch');
	await expect(panel.getByTestId('result-rows')).not.toContainText('kickoff');
});

test('the indexes page runs an ordered-index lookup live', async ({ page }) => {
	await page.goto('/docs/sql/indexes/');
	// First panel = the ordered city_country index: WHERE country = 'JP' ORDER BY name →
	// Kyoto, Osaka, Tokyo (the index narrows the scan; the result set is unchanged).
	const panel = page.getByTestId('live-sql').first();
	await expect(panel.getByTestId('result-rows')).toContainText('Kyoto');
	await expect(panel.getByTestId('result-rows')).not.toContainText('Paris');
});

test('the indexes page runs the GIN @> contains scan live', async ({ page }) => {
	await page.goto('/docs/sql/indexes/');
	// Second panel = the GIN containment scan: tags @> ARRAY[10,20] → intro and gin (both hold
	// {10,20}); arrays/storage/empty do not — the posting-list intersection of the index.
	const panel = page.getByTestId('live-sql').nth(1);
	await expect(panel.getByTestId('result-rows')).toContainText('intro');
	await expect(panel.getByTestId('result-rows')).toContainText('gin');
	await expect(panel.getByTestId('result-rows')).not.toContainText('arrays');
});

test('the indexes page runs the GIN && overlaps scan live', async ({ page }) => {
	await page.goto('/docs/sql/indexes/');
	// Third panel = the GIN overlap scan: tags && ARRAY[30,40] → intro (30), arrays (40),
	// storage (40) — the posting-list union; empty shares nothing and is excluded.
	const panel = page.getByTestId('live-sql').nth(2);
	await expect(panel.getByTestId('result-rows')).toContainText('storage');
	await expect(panel.getByTestId('result-rows')).not.toContainText('empty');
});

test('the indexes page runs the GIN = ANY membership scan live', async ({ page }) => {
	await page.goto('/docs/sql/indexes/');
	// Fourth panel = the GIN membership scan: 20 = ANY(tags) → intro, arrays, gin (all hold 20)
	// — the single term's posting list; storage ({40,50}) and empty do not and are excluded.
	const panel = page.getByTestId('live-sql').nth(3);
	await expect(panel.getByTestId('result-rows')).toContainText('intro');
	await expect(panel.getByTestId('result-rows')).toContainText('arrays');
	await expect(panel.getByTestId('result-rows')).not.toContainText('storage');
});

test('the indexes page runs the GIN = array-equality scan live', async ({ page }) => {
	await page.goto('/docs/sql/indexes/');
	// Fifth panel = the GIN equality scan: tags = ARRAY[10,20] → gin ONLY (its tags ARE {10,20});
	// intro ({10,20,30}) merely contains them, so the residual = excludes it — stricter than @>.
	const panel = page.getByTestId('live-sql').nth(4);
	await expect(panel.getByTestId('result-rows')).toContainText('gin');
	await expect(panel.getByTestId('result-rows')).not.toContainText('intro');
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
