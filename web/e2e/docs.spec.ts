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

test('the types page autoruns the boolean ⇄ i32 cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // Third panel = the boolean⇄i32 cast demo: true→1, false→0, 0→false, -5→true.
  const boolCastPanel = page.getByTestId('live-sql').nth(2);
  const rows = boolCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('1');
  await expect(rows).toContainText('false');
  await expect(rows).toContainText('true');
});

test('the types page autoruns the runtime text → number/boolean cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // Fourth panel = the runtime text→scalar cast demo: '42'→42, '  -7 '→-7, '3.14159'→3.14, 'yes'→true.
  const textNumPanel = page.getByTestId('live-sql').nth(3);
  const rows = textNumPanel.getByTestId('result-rows');
  await expect(rows).toContainText('42');
  await expect(rows).toContainText('-7');
  await expect(rows).toContainText('3.14');
  await expect(rows).toContainText('true');
});

test('the types page autoruns the varchar(n) truncation demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // Fifth panel = the varchar(n) demo: 'hello world'::varchar(5) truncates to 'hello', 'café'
  // fits varchar(4) by code point, 'ok' is within varchar(8).
  const varcharPanel = page.getByTestId('live-sql').nth(4);
  const rows = varcharPanel.getByTestId('result-rows');
  await expect(rows).toContainText('hello');
  await expect(rows).toContainText('café');
  await expect(rows).toContainText('ok');
});

test('the types page autoruns the uuid ⇄ text/bytea cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // Sixth panel = the uuid cast demo: text→uuid (canonical lowercase), uuid→text, uuid→bytea
  // (\x + 16 hex bytes), bytea→uuid (back to the canonical uuid).
  const uuidCastPanel = page.getByTestId('live-sql').nth(5);
  const rows = uuidCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('550e8400-e29b-41d4-a716-446655440000');
  await expect(rows).toContainText('\\x550e8400e29b41d4a716446655440000');
});

test('the types page autoruns the array cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // Seventh panel = the array cast demo: array→text ({1,2,3}), text→i32[], i32[]→i64[] widening, and
  // numeric[]→i32[] element rounding (1.7→2, 2.2→2, -2.5→-3, half away from zero).
  const arrayCastPanel = page.getByTestId('live-sql').nth(6);
  const rows = arrayCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('{1,2,3}');
  await expect(rows).toContainText('{2,2,-3}');
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
  page
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
      "INSERT INTO account VALUES (1, 'Ada', 100.00) ON CONFLICT (id) DO UPDATE SET balance = account.balance + excluded.balance RETURNING id, balance"
    );
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('200.00');

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the select page runs the data-modifying CTE demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Fourth LiveSql panel = the data-modifying CTE demo (spec/design/writable-cte.md): an
  // UPDATE ... RETURNING fed to a SELECT — kitchen prices cut 10%: Coffee 9.99 → 8.99, Mug 12.50 → 11.25.
  const panel = page.getByTestId('live-sql').nth(3);
  await expect(panel.getByTestId('result-rows')).toContainText('8.99');
  await expect(panel.getByTestId('result-rows')).toContainText('11.25');
});

test('the select page runs the WITH RECURSIVE series demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Fifth LiveSql panel = the WITH RECURSIVE demo: the recursive series 1..5 (spec/design/recursive-cte.md).
  const panel = page.getByTestId('live-sql').nth(4);
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('5');
});

test('the select page runs the nested WITH demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Sixth LiveSql panel = the nested-WITH demo (spec/design/cte.md §7): a derived table whose body
  // is `WITH cheap AS (… min(price) per category …) SELECT … WHERE cheapest <= 10` — office (1.50)
  // and kitchen (9.99).
  const panel = page.getByTestId('live-sql').nth(5);
  await expect(panel.getByTestId('result-rows')).toContainText('office');
  await expect(panel.getByTestId('result-rows')).toContainText('kitchen');
  await expect(panel.getByTestId('result-rows')).toContainText('1.50');
  await expect(panel.getByTestId('result-rows')).toContainText('9.99');
});

test('the select page runs the LATERAL top-N-per-group demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Ninth LiveSql panel = the CROSS JOIN LATERAL demo: the priciest product of each category —
  // kitchen → Mug (12.50), office → Notebook (4.00); the dependent subquery re-runs per category.
  const panel = page.getByTestId('live-sql').nth(8);
  await expect(panel.getByTestId('result-rows')).toContainText('Mug');
  await expect(panel.getByTestId('result-rows')).toContainText('Notebook');
});

test('the select page runs the CASE/COALESCE conditional demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Tenth LiveSql panel = the conditional-expressions demo: CASE tiers each product by price
  // (Coffee/Mug → premium, Notebook/Pen → basic) and coalesce substitutes Coffee's nickname.
  const panel = page.getByTestId('live-sql').nth(9);
  await expect(panel.getByTestId('result-rows')).toContainText('premium');
  await expect(panel.getByTestId('result-rows')).toContainText('basic');
  await expect(panel.getByTestId('result-rows')).toContainText('Joe');
});

test('the select page runs the array containment operators live (@> / <@ / &&)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // Twelfth LiveSql panel = the @>/<@/&& demo; every column is true (the array sets contain/overlap).
  const panel = page.getByTestId('live-sql').nth(11);
  await expect(panel.getByTestId('result-rows')).toContainText('true');
});

test('the select page runs the ANY/ALL quantified comparisons live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Thirteenth LiveSql panel = the = ANY / > ALL demo: any_match true, all_greater true, no_match false.
  const panel = page.getByTestId('live-sql').nth(12);
  await expect(panel.getByTestId('result-rows')).toContainText('true');
  await expect(panel.getByTestId('result-rows')).toContainText('false');
});

test('the select page runs the VARIADIC num_nulls demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Fifteenth LiveSql panel = the num_nulls demo: spread 1, variadic 1, non_nulls 2 (both forms agree).
  const panel = page.getByTestId('live-sql').nth(14);
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('2');
});

test('the select page runs the array-of-composite demo live (field access into array elements)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // Sixteenth LiveSql panel = the addr[] demo: row 1's first element renders (Main,90210), its street
  // is Main and zip 90210 — array-of-composite construction, subscript, and field access (§12 AC1).
  const panel = page.getByTestId('live-sql').nth(15);
  await expect(panel.getByTestId('result-rows')).toContainText('(Main,90210)');
  await expect(panel.getByTestId('result-rows')).toContainText('Main');
  await expect(panel.getByTestId('result-rows')).toContainText('90210');
});

test('the select page runs the composite-with-array-field demo live (field access + subscript)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // Nineteenth LiveSql panel = the poly(name, pts i32[]) demo: row 1 renders its array field
  // {10,20,30}, (p).pts reads the whole array, and (p).pts[1] reads the first element 10 —
  // a composite type with an array-typed field (array.md §12, the mirror of array-of-composite).
  const panel = page.getByTestId('live-sql').nth(18);
  await expect(panel.getByTestId('result-rows')).toContainText('{10,20,30}');
  await expect(panel.getByTestId('result-rows')).toContainText('10');
});

test('the select page runs the date demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // Twentieth LiveSql panel = the date demo: WHERE on_day < '2024-03-01' ORDER BY on_day keeps
  // only the two early dates, chronologically — review (2023-11-02) then launch (2024-01-15); the
  // 2024-06-15 and infinity rows are filtered out (spec/design/date.md).
  const panel = page.getByTestId('live-sql').nth(19);
  await expect(panel.getByTestId('result-rows')).toContainText('review');
  await expect(panel.getByTestId('result-rows')).toContainText('launch');
  await expect(panel.getByTestId('result-rows')).not.toContainText('kickoff');
});

test('the indexes page runs an ordered-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // First panel = the ordered city_region index: WHERE region = 1 ORDER BY name →
  // Kyoto, Osaka, Tokyo (the index narrows the scan; the result set is unchanged).
  const panel = page.getByTestId('live-sql').first();
  await expect(panel.getByTestId('result-rows')).toContainText('Kyoto');
  await expect(panel.getByTestId('result-rows')).not.toContainText('Paris');
});

test('the indexes page runs the expression-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Second panel = the UNIQUE index on lower(email): WHERE lower(email) = 'ada@example.com'
  // seeks the expression index to Ada's row (id 1); grace's row (id 2) is excluded.
  const panel = page.getByTestId('live-sql').nth(1);
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).not.toContainText('2');
});

test('the indexes page runs the partial-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Third panel = the partial orders_active index: WHERE status = 'active' AND customer = 10 →
  // orders 1 and 4; the shipped order 2 and the cancelled order 5 are excluded.
  const panel = page.getByTestId('live-sql').nth(2);
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('4');
  await expect(panel.getByTestId('result-rows')).not.toContainText('2');
});

test('the indexes page runs the GIN @> contains scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Fourth panel = the GIN containment scan: tags @> ARRAY[10,20] → intro and gin (both hold
  // {10,20}); arrays/storage/empty do not — the posting-list intersection of the index.
  const panel = page.getByTestId('live-sql').nth(3);
  await expect(panel.getByTestId('result-rows')).toContainText('intro');
  await expect(panel.getByTestId('result-rows')).toContainText('gin');
  await expect(panel.getByTestId('result-rows')).not.toContainText('arrays');
});

test('the indexes page runs the GIN && overlaps scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Fifth panel = the GIN overlap scan: tags && ARRAY[30,40] → intro (30), arrays (40),
  // storage (40) — the posting-list union; empty shares nothing and is excluded.
  const panel = page.getByTestId('live-sql').nth(4);
  await expect(panel.getByTestId('result-rows')).toContainText('storage');
  await expect(panel.getByTestId('result-rows')).not.toContainText('empty');
});

test('the indexes page runs the GIN = ANY membership scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Sixth panel = the GIN membership scan: 20 = ANY(tags) → intro, arrays, gin (all hold 20)
  // — the single term's posting list; storage ({40,50}) and empty do not and are excluded.
  const panel = page.getByTestId('live-sql').nth(5);
  await expect(panel.getByTestId('result-rows')).toContainText('intro');
  await expect(panel.getByTestId('result-rows')).toContainText('arrays');
  await expect(panel.getByTestId('result-rows')).not.toContainText('storage');
});

test('the indexes page runs the GIN = array-equality scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // Seventh panel = the GIN equality scan: tags = ARRAY[10,20] → gin ONLY (its tags ARE {10,20});
  // intro ({10,20,30}) merely contains them, so the residual = excludes it — stricter than @>.
  const panel = page.getByTestId('live-sql').nth(6);
  await expect(panel.getByTestId('result-rows')).toContainText('gin');
  await expect(panel.getByTestId('result-rows')).not.toContainText('intro');
});

test('the queries API page wires up the idiomatic example for each language', async ({ page }) => {
  await page.goto('/docs/api/queries/');
  // Default (Rust): the rusqlite-style run/query_row methods.
  await expect(page.getByTestId('code-rust')).toBeVisible();
  await expect(page.getByTestId('code-rust')).toContainText('db.run');
  await expect(page.getByTestId('code-rust')).toContainText('query_row');

  // Go: the database/sql-style Exec/QueryRow + struct mapping.
  await page.getByTestId('lang-go').click();
  await expect(page.getByTestId('code-go')).toContainText('db.Exec');
  await expect(page.getByTestId('code-go')).toContainText('RowToStructByName');

  // TypeScript: the better-sqlite3-style prepare/get/iterate.
  await page.getByTestId('lang-ts').click();
  await expect(page.getByTestId('code-ts')).toContainText('db.prepare');
  await expect(page.getByTestId('code-ts')).toContainText('jed-ts');
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

test('the explain page renders a live plan with the PK-bound access path', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // Second panel = EXPLAIN SELECT ... WHERE id = 3: a PK point lookup under the residual Filter.
  const panel = page.getByTestId('live-sql').nth(1);
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 3');
  await expect(panel.getByTestId('result-rows')).toContainText('Filter');
  await expect(panel).toContainText('est_rows');
  await expect(panel).toContainText('est_cost');
});

test('the explain page shows bounded top-k on a blocking sort', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  const panel = page.getByTestId('live-sql').nth(0);
  await expect(panel.getByTestId('result-rows')).toContainText('Sort');
  await expect(panel.getByTestId('result-rows')).toContainText('keys=1, top-k=2');
});

test('the explain page shows the OR / IN-list interval-set access path', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // Fifth panel = EXPLAIN SELECT ... WHERE id IN (1, 3, 5): a union of point probes, labelled
  // "PK interval set" (cost.md §3 "OR / IN-list").
  const panel = page.getByTestId('live-sql').nth(4);
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText('PK interval set: id; intervals=3');
});

test('the explain page shows the index-nested-loop access path on a join', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // Sixth panel = EXPLAIN SELECT ... FROM trip JOIN city ON city.id = trip.city_id: the inner city
  // scan is a per-outer-row PK seek, labelled Index-nested-loop.
  const panel = page.getByTestId('live-sql').nth(5);
  await expect(panel.getByTestId('result-rows')).toContainText('Nested Loop');
  await expect(panel.getByTestId('result-rows')).toContainText(
    'Index-nested-loop PK bound: id = join'
  );
});

test('the explain page shows the deterministic hash-join operator', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // Seventh panel = an equality join whose inner trip.city_id has no usable index.
  const panel = page.getByTestId('live-sql').nth(6);
  await expect(panel.getByTestId('result-rows')).toContainText('Hash Join');
  await expect(panel.getByTestId('result-rows')).toContainText('inner; keys=1; on:conjuncts=1');
});

test('the explain page runs EXPLAIN ANALYZE with a deterministic cost', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // Ninth panel = EXPLAIN ANALYZE: the Analyze root reports the real accrued cost + row count.
  const panel = page.getByTestId('live-sql').nth(8);
  await expect(panel.getByTestId('result-rows')).toContainText('Analyze');
  await expect(panel.getByTestId('result-rows')).toContainText('cost=');
});
