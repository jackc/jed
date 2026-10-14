import { expect, test, type Locator, type Page } from '@playwright/test';

// SQL docs live-panel tests (Phase 5): the language-neutral SQL pages embed real in-memory jed
// databases that autorun, enforce constraints live, surface SQLSTATE codes, and reset to seed.

// A LiveSql panel located by its `demo` name (rendered as `data-demo`), not its position on the page:
// adding a panel above it then cannot silently retarget the test at a neighbouring demo.
function livePanel(page: Page, demo: string): Locator {
  return page.locator(`[data-testid="live-sql"][data-demo="${demo}"]`);
}

test('the types page autoruns a decimal demo (0.1 + 0.2 = 0.3, exactly)', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The decimals demo; exact decimal so the sum is 0.3, not 0.30000000004.
  await expect(livePanel(page, 'decimalDemo').getByTestId('result-rows')).toContainText('0.3');
});

test('the types page shows the integer overflow trap (22003)', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The i16 overflow demo; it autoruns to a 22003 error.
  const overflowPanel = livePanel(page, 'overflowDemo');
  await expect(overflowPanel.getByTestId('error-code')).toHaveText('22003');
});

test('the types page autoruns the boolean ⇄ i32 cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The boolean⇄i32 cast demo: true→1, false→0, 0→false, -5→true.
  const boolCastPanel = livePanel(page, 'boolCastDemo');
  const rows = boolCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('1');
  await expect(rows).toContainText('false');
  await expect(rows).toContainText('true');
});

test('the types page autoruns the runtime text → number/boolean cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The runtime text→scalar cast demo: '42'→42, '  -7 '→-7, '3.14159'→3.14, 'yes'→true.
  const textNumPanel = livePanel(page, 'textNumCastDemo');
  const rows = textNumPanel.getByTestId('result-rows');
  await expect(rows).toContainText('42');
  await expect(rows).toContainText('-7');
  await expect(rows).toContainText('3.14');
  await expect(rows).toContainText('true');
});

test('the types page autoruns the varchar(n) truncation demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The varchar(n) demo: 'hello world'::varchar(5) truncates to 'hello', 'café'
  // fits varchar(4) by code point, 'ok' is within varchar(8), and the two-word
  // 'jedi'::character varying(3) truncates to 'jed'.
  const varcharPanel = livePanel(page, 'varcharDemo');
  const rows = varcharPanel.getByTestId('result-rows');
  await expect(rows).toContainText('hello');
  await expect(rows).not.toContainText('hello world');
  await expect(rows).toContainText('café');
  await expect(rows).toContainText('ok');
  await expect(rows).toContainText('jed');
  await expect(rows).not.toContainText('jedi');
});

test('the types page autoruns the uuid ⇄ text/bytea cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The uuid cast demo: text→uuid (canonical lowercase), uuid→text, uuid→bytea
  // (\x + 16 hex bytes), bytea→uuid (back to the canonical uuid).
  const uuidCastPanel = livePanel(page, 'uuidCastDemo');
  const rows = uuidCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('550e8400-e29b-41d4-a716-446655440000');
  await expect(rows).toContainText('\\x550e8400e29b41d4a716446655440000');
});

test('the types page autoruns the text concatenation demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // 'jed' || '-' || 'db', 'row ' || 42, 1.50 || ' USD' (the numeric keeps its scale).
  const rows = livePanel(page, 'concatDemo').getByTestId('result-rows');
  await expect(rows).toContainText('jed-db');
  await expect(rows).toContainText('row 42');
  await expect(rows).toContainText('1.50 USD');
});

test('the types page autoruns the bytea input demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // hex input '\xdead beef' (whitespace between pairs) = \xdeadbeef, escape-format 'abc' =
  // \x616263, 'a\\b\101' = \x615c6241.
  const panel = livePanel(page, 'byteaInputDemo');
  const rows = panel.getByTestId('result-rows');
  await expect(rows).toContainText('\\xdeadbeef');
  await expect(rows).toContainText('\\x616263');
  await expect(rows).toContainText('\\x615c6241');
});

test('the types page autoruns the bytea functions demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The bytea demo: length 3, \x0102 || \xff = \x0102ff, SUBSTRING(… FROM 2 FOR 3) = \x020304,
  // get_byte(…, 2) = 255.
  const byteaPanel = livePanel(page, 'byteaDemo');
  const rows = byteaPanel.getByTestId('result-rows');
  await expect(rows).toContainText('\\x0102ff');
  await expect(rows).toContainText('\\x020304');
  await expect(rows).toContainText('255');
});

test('the types page autoruns the interval input forms demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // SQL-standard '1-2' = 1 year 2 mons and '1 1:00' = 1 day 01:00:00; both ISO 8601 spellings
  // = 1 year 2 mons 3 days 04:05:06.
  const panel = livePanel(page, 'intervalInputDemo');
  const rows = panel.getByTestId('result-rows');
  await expect(rows).toContainText('1 day 01:00:00');
  await expect(rows).toContainText('1 year 2 mons 3 days 04:05:06');
});

test('the types page autoruns the interval justify demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // justify_hours('50 hours') = 2 days 02:00:00, justify_days('45 days') = 1 mon 15 days,
  // justify_interval('1 mon -1 hour') = 29 days 23:00:00.
  const justifyPanel = livePanel(page, 'justifyDemo');
  const rows = justifyPanel.getByTestId('result-rows');
  await expect(rows).toContainText('2 days 02:00:00');
  await expect(rows).toContainText('1 mon 15 days');
  await expect(rows).toContainText('29 days 23:00:00');
});

test('the types page autoruns the age demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // age keeps years and months (43 years 9 mons 27 days; Jan 31 → Mar 1 = 1 mon 1 day) where plain
  // subtraction counts days (30 days).
  const panel = livePanel(page, 'ageDemo');
  const rows = panel.getByTestId('result-rows');
  await expect(rows).toContainText('43 years 9 mons 27 days');
  await expect(rows).toContainText('1 mon 1 day');
  await expect(rows).toContainText('30 days');
});

test('the types page autoruns the bytea position / overlay demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // position = 3, overlay(… PLACING \xaabb FROM 2) = \x01aabb0405, set_byte(…, 0, 255) = \xff0203,
  // get_bit(\x0102, 9) = 1, btrim(…, \x0001) = \x02.
  const panel = livePanel(page, 'byteaEditDemo');
  const rows = panel.getByTestId('result-rows');
  await expect(rows).toContainText('\\x01aabb0405');
  await expect(rows).toContainText('\\xff0203');
  await expect(rows).toContainText('\\x02');
});

test('the types page autoruns the bytea digest demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // md5 of text and of the same bytes agree; sha256('abc') is the FIPS 180-4 vector; sha512 is 64 bytes.
  const panel = livePanel(page, 'byteaHashDemo');
  const rows = panel.getByTestId('result-rows');
  await expect(rows).toContainText('900150983cd24fb0d6963f7d28e17f72');
  await expect(rows).toContainText(
    'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'
  );
  await expect(rows).toContainText('64');
});

test('the types page autoruns the array cast demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The array cast demo: array→text ({1,2,3}), text→i32[], i32[]→i64[] widening, and
  // numeric[]→i32[] element rounding (1.7→2, 2.2→2, -2.5→-3, half away from zero).
  const arrayCastPanel = livePanel(page, 'arrayCastDemo');
  const rows = arrayCastPanel.getByTestId('result-rows');
  await expect(rows).toContainText('{1,2,3}');
  await expect(rows).toContainText('{2,2,-3}');
});

test('the types page updates json/jsonb through the shared assignment rule', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // The jsonb expression assignment plus jsonb → json assignment from the OLD row.
  const jsonUpdatePanel = livePanel(page, 'jsonUpdateDemo');
  const rows = jsonUpdatePanel.getByTestId('result-rows');
  await expect(rows).toContainText('{"a": 1, "active": true}');
  await expect(rows).toContainText('{"a": 1}');
});

test('the types page autoruns the jsonb_set_lax demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // jsonb_set replaces a; jsonb_set_lax stores a JSON null by default, or deletes the key by name.
  const rows = livePanel(page, 'jsonSetLaxDemo').getByTestId('result-rows');
  await expect(rows).toContainText('{"a": 99, "b": 2}');
  await expect(rows).toContainText('{"a": null, "b": 2}');
  await expect(rows).toContainText('{"b": 2}');
});

test('the types page autoruns the verbatim json accessor demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // json -> keeps the last duplicate's exact text; jsonb -> is canonical; #>> renders 1.50 verbatim.
  const rows = livePanel(page, 'jsonReadDemo').getByTestId('result-rows');
  await expect(rows).toContainText('{"b" :  1.50}');
  await expect(rows).toContainText('{"b": 1.50}');
});

test('the types page autoruns the row_to_json demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // A named composite keys by field name; an anonymous ROW by f1, f2, …; a timestamp is ISO 8601.
  const rows = livePanel(page, 'rowToJsonDemo').getByTestId('result-rows');
  await expect(rows).toContainText('{"street":"Main","zip":90210}');
  await expect(rows).toContainText('{"f1":1,"f2":null,"f3":[1,2],"f4":{"b": 2}}');
  await expect(rows).toContainText('{"f1":"2024-01-15T10:30:00"}');
});

test('the types page autoruns the to_json demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // A jsonb object sorts its keys; arrays nest; a date key and value take the ISO 8601 form.
  const rows = livePanel(page, 'toJsonDemo').getByTestId('result-rows');
  await expect(rows).toContainText('{"f1": "b", "f2": 1}');
  await expect(rows).toContainText('[[1,2],[3,4]]');
  await expect(rows).toContainText('{"2024-01-15" : "1 day"}');
  await expect(rows).toContainText('["2024-01-15", "2024-02-15"]');
});

test('the types page autoruns the jsonpath item method demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // .type() per element; .integer() unwraps the array and rounds 3.7 → 4; .keyvalue().key lists keys.
  const rows = livePanel(page, 'jsonpathMethodDemo').getByTestId('result-rows');
  await expect(rows).toContainText('["number", "string", "number"]');
  await expect(rows).toContainText('[12, 4, -4]');
  await expect(rows).toContainText('["x", "y"]');
});

test('the types page autoruns the jsonpath arithmetic demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // 2.50 * 3 keeps the scale (7.50); the filter keeps even numbers; last - 1 is the second-to-last.
  const rows = livePanel(page, 'jsonpathArithDemo').getByTestId('result-rows');
  await expect(rows).toContainText('7.50');
  await expect(rows).toContainText('[2, 4]');
  await expect(rows).toContainText('"number"');
});

test('the types page autoruns the jsonpath variables demo', async ({ page }) => {
  await page.goto('/docs/sql/types/');
  // vars bound $min/$max; silent stopped at "x" keeping [1]; PASSING bound $qty (2.50 * 4).
  const rows = livePanel(page, 'jsonpathVarsDemo').getByTestId('result-rows');
  await expect(rows).toContainText('[8, 12]');
  await expect(rows).toContainText('[1]');
  await expect(rows).toContainText('10.00');
});

test('the tables page decodes Unicode-escape literals and names live', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  // U&'caf\00e9' is café in a column named näme; the UESCAPE '!' form decodes to data.
  const panel = livePanel(page, 'unicodeEscapeDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('café');
  await expect(panel.getByTestId('result-rows')).toContainText('data');
  await expect(panel).toContainText('näme');
});

test('the tables page enforces a CHECK constraint live and resets to seed', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');

  await panel.getByTestId('sql-input').fill("INSERT INTO account VALUES (3, 'Bob', -5)");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('23514');

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the tables page enforces a FOREIGN KEY and cascades parent deletes', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');

  // A child row referencing a non-existent account traps 23503.
  await panel.getByTestId('sql-input').fill('INSERT INTO txn VALUES (3, 99, 5)');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('23503');

  // The parent side follows the declared ON DELETE CASCADE and removes txn 1 with account 1.
  await panel
    .getByTestId('sql-input')
    .fill('DELETE FROM account WHERE id = 1; SELECT account_id FROM txn ORDER BY id');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveCount(0);
  await expect(panel.getByTestId('result-rows').locator('td')).toHaveText(['2']);

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the tables page creates a table from a query with CREATE TABLE AS live', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');

  // The new table takes the query's rows, and its balance column keeps numeric(12,2): 1.234 is
  // stored as 1.23 (create-table-as.md §2).
  await panel
    .getByTestId('sql-input')
    .fill(
      "CREATE TABLE big_spender AS SELECT owner, balance FROM account WHERE balance > 60; INSERT INTO big_spender VALUES ('Eve', 1.234); SELECT owner, balance FROM big_spender ORDER BY owner"
    );
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveCount(0);
  await expect(panel.getByTestId('result-rows').locator('td')).toHaveText([
    'Ada',
    '100.00',
    'Eve',
    '1.23'
  ]);

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the tables page upserts with ON CONFLICT DO UPDATE (excluded) live', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
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

test('the tables page upserts with DO UPDATE SET col = DEFAULT live', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');

  // The conflicting job keeps its key, takes the proposed state, and resets attempts to its
  // declared default 0 (upsert.md §5).
  await panel
    .getByTestId('sql-input')
    .fill(
      "CREATE TABLE job (id i32 PRIMARY KEY, state text NOT NULL DEFAULT 'queued', attempts i32 DEFAULT 0); INSERT INTO job VALUES (1, 'running', 3); INSERT INTO job (id, state) VALUES (1, 'retry') ON CONFLICT (id) DO UPDATE SET state = excluded.state, attempts = DEFAULT RETURNING state, attempts"
    );
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveCount(0);
  await expect(panel.getByTestId('result-rows').locator('td')).toHaveText(['retry', '0']);

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the tables page upserts through an INSERT target alias live', async ({ page }) => {
  await page.goto('/docs/sql/tables/');
  const panel = livePanel(page, 'accountDemo');
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');

  // The alias replaces the table's name (upsert.md §5): balance 100 + 5 = 105 through `a`.
  await panel
    .getByTestId('sql-input')
    .fill(
      "INSERT INTO account AS a VALUES (1, 'Ada', 5.00) ON CONFLICT (id) DO UPDATE SET balance = a.balance + excluded.balance RETURNING a.balance"
    );
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('105.00');

  // ... and the table's own name is no longer visible.
  await panel
    .getByTestId('sql-input')
    .fill("INSERT INTO account AS a VALUES (9, 'Zed', 1.00) RETURNING account.id");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('42P01');

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Ada');
});

test('the select page names a column with a bare label (no AS)', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The grouped demo: `count(*) items` labels its column without AS
  // (spec/design/grammar.md §8) — office 2 / kitchen 2.
  const panel = livePanel(page, 'grouped');
  await expect(panel.locator('thead')).toContainText('items');
  await expect(panel.getByTestId('result-rows')).toContainText('kitchen');
});

test('the select page orders an aggregate input live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // An in-aggregate ORDER BY (spec/design/aggregates.md §22): each category's names, priciest first.
  const panel = livePanel(page, 'aggOrder');
  await expect(panel.getByTestId('result-rows')).toContainText('["Mug", "Coffee"]');
  await expect(panel.getByTestId('result-rows')).toContainText('["Notebook", "Pen"]');
  await panel.getByTestId('sql-input').fill('SELECT jsonb_agg(name ORDER BY price) FROM product');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText(
    '["Pen", "Notebook", "Coffee", "Mug"]'
  );
});

test('the select page inserts the rows of a set operation live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // A set operation as an INSERT source (spec/design/grammar.md §24): the two office products are
  // copied as refills and a literal kitchen row is appended, all through one UNION ALL.
  const panel = livePanel(page, 'setOps');
  await expect(panel.getByTestId('result-rows')).toContainText('Mug');
  await panel
    .getByTestId('sql-input')
    .fill(
      "INSERT INTO product SELECT id + 10, name || ' refill', category, price FROM product WHERE category = 'office' UNION ALL SELECT 20, 'Kettle', 'kitchen', 30.00 RETURNING id, name"
    );
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Pen refill');
  await expect(panel.getByTestId('result-rows')).toContainText('Notebook refill');
  await expect(panel.getByTestId('result-rows')).toContainText('Kettle');

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('Mug');
});

test('the select page runs a VALUES query as a set-operation operand live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // A VALUES list as a query (spec/design/grammar.md §25): two literal rows appended to the office
  // products with UNION ALL, ordered by price.
  const panel = livePanel(page, 'valuesQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('Stapler');
  await expect(panel.getByTestId('result-rows')).toContainText('Tape');
  await panel
    .getByTestId('sql-input')
    .fill("VALUES (1, 'one'), (2, 'two'), (3, 'three') ORDER BY column2 LIMIT 1");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('one');
  await expect(panel.getByTestId('result-rows')).not.toContainText('three');
});

test('the select page runs the data-modifying CTE demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The data-modifying CTE demo (spec/design/writable-cte.md): an
  // UPDATE ... RETURNING fed to a SELECT — kitchen prices cut 10%: Coffee 9.99 → 8.99, Mug 12.50 → 11.25.
  const panel = livePanel(page, 'dataModifying');
  await expect(panel.getByTestId('result-rows')).toContainText('8.99');
  await expect(panel.getByTestId('result-rows')).toContainText('11.25');
});

test('the select page runs the WITH RECURSIVE series demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The WITH RECURSIVE demo: the recursive series 1..5 (spec/design/recursive-cte.md).
  const panel = livePanel(page, 'recursive');
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('5');
});

test('the select page runs the SEARCH / CYCLE demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // SEARCH DEPTH FIRST + CYCLE (spec/design/recursive-cte.md §9): the walk 1 → 2 → 3 revisits 1,
  // which is flagged as a cycle and not expanded; 3 → 4 is the other branch.
  const panel = livePanel(page, 'searchCycle');
  await expect(panel.getByTestId('result-rows')).toContainText('{(1),(2),(3),(1)}');
  await expect(panel.getByTestId('result-rows')).toContainText('{(1),(2),(3),(4)}');
});

test('the select page runs the nested WITH demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The nested-WITH demo (spec/design/cte.md §7): a derived table whose body
  // is `WITH cheap AS (… min(price) per category …) SELECT … WHERE cheapest <= 10` — office (1.50)
  // and kitchen (9.99).
  const panel = livePanel(page, 'nestedWith');
  await expect(panel.getByTestId('result-rows')).toContainText('office');
  await expect(panel.getByTestId('result-rows')).toContainText('kitchen');
  await expect(panel.getByTestId('result-rows')).toContainText('1.50');
  await expect(panel.getByTestId('result-rows')).toContainText('9.99');
});

test('the select page runs the LATERAL top-N-per-group demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The CROSS JOIN LATERAL demo: the priciest product of each category —
  // kitchen → Mug (12.50), office → Notebook (4.00); the dependent subquery re-runs per category.
  const panel = livePanel(page, 'lateral');
  await expect(panel.getByTestId('result-rows')).toContainText('Mug');
  await expect(panel.getByTestId('result-rows')).toContainText('Notebook');
});

test('the select page runs the parenthesized-join demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The parenthesized-join demo (spec/design/grammar.md §15): customer LEFT JOIN
  // (orders JOIN product) keeps Linus, who has no orders, with a NULL product.
  const panel = livePanel(page, 'parenthesized');
  await expect(panel.getByTestId('result-rows')).toContainText('Mug');
  await expect(panel.getByTestId('result-rows')).toContainText('Linus');
});

test('the select page runs the CASE/COALESCE conditional demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The conditional-expressions demo: CASE tiers each product by price
  // (Coffee/Mug → premium, Notebook/Pen → basic) and coalesce substitutes Coffee's nickname.
  const panel = livePanel(page, 'conditional');
  await expect(panel.getByTestId('result-rows')).toContainText('premium');
  await expect(panel.getByTestId('result-rows')).toContainText('basic');
  await expect(panel.getByTestId('result-rows')).toContainText('Joe');
});

test('the select page runs the array containment operators live (@> / <@ / &&)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // The @>/<@/&& demo; every column is true (the array sets contain/overlap).
  const panel = livePanel(page, 'containmentExample');
  await expect(panel.getByTestId('result-rows')).toContainText('true');
});

test('the select page runs the ANY/ALL quantified comparisons live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The = ANY / > ALL demo: any_match true, all_greater true, no_match false.
  const panel = livePanel(page, 'quantifiedExample');
  await expect(panel.getByTestId('result-rows')).toContainText('true');
  await expect(panel.getByTestId('result-rows')).toContainText('false');
});

test('the select page runs the VARIADIC num_nulls demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The num_nulls demo: spread 1, variadic 1, non_nulls 2 (both forms agree).
  const panel = livePanel(page, 'variadicExample');
  await expect(panel.locator('thead')).toContainText('non_nulls');
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('2');
});

test('the select page runs the array-of-composite demo live (field access into array elements)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // The addr[] demo: row 1's first element renders (Main,90210), its street
  // is Main and zip 90210 — array-of-composite construction, subscript, and field access (§12 AC1).
  const panel = livePanel(page, 'arrayCompositeExample');
  await expect(panel.getByTestId('result-rows')).toContainText('(Main,90210)');
  await expect(panel.getByTestId('result-rows')).toContainText('Main');
  await expect(panel.getByTestId('result-rows')).toContainText('90210');
});

test('the select page runs the composite-with-array-field demo live (field access + subscript)', async ({
  page
}) => {
  await page.goto('/docs/sql/select/');
  // The poly(name, pts i32[]) demo: row 1 renders its array field
  // {10,20,30}, (p).pts reads the whole array, and (p).pts[1] reads the first element 10 —
  // a composite type with an array-typed field (array.md §12, the mirror of array-of-composite).
  const panel = livePanel(page, 'compositeArrayFieldExample');
  await expect(panel.getByTestId('result-rows')).toContainText('{10,20,30}');
  await expect(panel.getByTestId('result-rows')).toContainText('10');
});

test('the select page runs the date demo live', async ({ page }) => {
  await page.goto('/docs/sql/select/');
  // The date demo: WHERE on_day < '2024-03-01' ORDER BY on_day keeps
  // only the two early dates, chronologically — review (2023-11-02) then launch (2024-01-15); the
  // 2024-06-15 and infinity rows are filtered out (spec/design/date.md).
  const panel = livePanel(page, 'dateExample');
  await expect(panel.getByTestId('result-rows')).toContainText('review');
  await expect(panel.getByTestId('result-rows')).toContainText('launch');
  await expect(panel.getByTestId('result-rows')).not.toContainText('kickoff');
  await expect(panel.getByTestId('result-rows')).not.toContainText('someday');
});

test('the indexes page runs an ordered-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The ordered city_region index: WHERE region = 1 ORDER BY name →
  // Kyoto, Osaka, Tokyo (the index narrows the scan; the result set is unchanged).
  const panel = livePanel(page, 'orderedQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('Kyoto');
  await expect(panel.getByTestId('result-rows')).not.toContainText('Paris');
});

test('the indexes page runs the expression-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The UNIQUE index on lower(email): WHERE lower(email) = 'ada@example.com'
  // seeks the expression index to Ada's row (id 1); grace's row (id 2) is excluded.
  const panel = livePanel(page, 'exprQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).not.toContainText('2');
});

test('the indexes page runs the partial-index lookup live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The partial orders_active index: WHERE status = 'active' AND customer = 10 →
  // orders 1 and 4; the shipped order 2 and the cancelled order 5 are excluded.
  const panel = livePanel(page, 'partialQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('1');
  await expect(panel.getByTestId('result-rows')).toContainText('4');
  await expect(panel.getByTestId('result-rows')).not.toContainText('2');
});

test('the indexes page runs the GIN @> contains scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The GIN containment scan: tags @> ARRAY[10,20] → intro and gin (both hold
  // {10,20}); arrays/storage/empty do not — the posting-list intersection of the index.
  const panel = livePanel(page, 'containsQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('intro');
  await expect(panel.getByTestId('result-rows')).toContainText('gin');
  await expect(panel.getByTestId('result-rows')).not.toContainText('arrays');
});

test('the indexes page runs the GIN && overlaps scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The GIN overlap scan: tags && ARRAY[30,40] → intro (30), arrays (40),
  // storage (40) — the posting-list union; empty shares nothing and is excluded.
  const panel = livePanel(page, 'overlapsQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('storage');
  await expect(panel.getByTestId('result-rows')).not.toContainText('empty');
});

test('the indexes page runs the GIN = ANY membership scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The GIN membership scan: 20 = ANY(tags) → intro, arrays, gin (all hold 20)
  // — the single term's posting list; storage ({40,50}) and empty do not and are excluded.
  const panel = livePanel(page, 'memberQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('intro');
  await expect(panel.getByTestId('result-rows')).toContainText('arrays');
  await expect(panel.getByTestId('result-rows')).not.toContainText('storage');
});

test('the indexes page runs the GIN = array-equality scan live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // The GIN equality scan: tags = ARRAY[10,20] → gin ONLY (its tags ARE {10,20});
  // intro ({10,20,30}) merely contains them, so the residual = excludes it — stricter than @>.
  const panel = livePanel(page, 'equalQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('gin');
  await expect(panel.getByTestId('result-rows')).not.toContainText('intro');
});

test('the indexes page runs the jsonb GIN @> and ? scans live', async ({ page }) => {
  await page.goto('/docs/sql/indexes/');
  // jsonb_ops containment: only event 3 is a login from ios (event 4's device is android).
  const contains = livePanel(page, 'jsonbContainsQuery');
  await expect(contains.getByTestId('result-rows')).toContainText('3');
  await expect(contains.getByTestId('result-rows')).not.toContainText('4');
  // jsonb_ops key existence: events 1 and 4 carry a top-level "tags" key.
  const key = livePanel(page, 'jsonbKeyQuery');
  await expect(key.getByTestId('result-rows')).toContainText('1');
  await expect(key.getByTestId('result-rows')).toContainText('4');
  await expect(key.getByTestId('result-rows')).not.toContainText('2');
});

test('the views page reads views live and rejects bad writes and breaking drops', async ({
  page
}) => {
  await page.goto('/docs/sql/views/');
  // active_staff JOIN dept_payroll (spec/design/views.md §4): Ada in eng with the eng
  // payroll 19400.00; Ken is inactive, so the view hides him.
  const panel = livePanel(page, 'query');
  await expect(panel.getByTestId('result-rows')).toContainText('19400.00');
  await expect(panel.getByTestId('result-rows')).not.toContainText('Ken');

  // A grouped view is not updatable (55000), and a table a view reads cannot be dropped (2BP01).
  await panel.getByTestId('sql-input').fill('DELETE FROM dept_payroll');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('55000');
  await panel.getByTestId('sql-input').fill('DROP TABLE employee');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('2BP01');

  await panel.getByTestId('reset-button').click();
  await expect(panel.getByTestId('result-rows')).toContainText('19400.00');
});

test('the views page plans a view like its derived table', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // EXPLAIN over the view: a Subquery node whose outer id = 2 became a PK lookup.
  const panel = livePanel(page, 'explain');
  await expect(panel.getByTestId('result-rows')).toContainText('Subquery active_staff');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 2');
});

test('the views page freezes * at CREATE VIEW', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // SELECT * FROM all_notes after note gained a pinned column: the view still has
  // only id and body (views.md §5).
  const panel = livePanel(page, 'starQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('hello');
  await expect(panel.locator('thead')).toContainText('body');
  await expect(panel.locator('thead')).not.toContainText('pinned');
});

test('the views page rewrites view definitions on RENAME', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // RENAME COLUMN + RENAME TO, then jed_views: the stored text now reads the renamed
  // table under its old label with a column alias list keeping the old column name (views.md §7).
  const panel = livePanel(page, 'rename');
  await expect(panel.getByTestId('result-rows')).toContainText('"staff" as "employee"');
  await expect(panel.getByTestId('error-code')).toHaveCount(0);
});

test('the views page replaces a view and refuses dropping its columns', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // CREATE OR REPLACE VIEW appending salary and narrowing to eng (views.md §1.2).
  const panel = livePanel(page, 'replace');
  await expect(panel.getByTestId('result-rows')).toContainText('9900.00');
  await expect(panel.getByTestId('result-rows')).not.toContainText('Linus');

  // Dropping an existing column is 42P16.
  await panel
    .getByTestId('sql-input')
    .fill('CREATE OR REPLACE VIEW active_staff AS SELECT id, name FROM employee');
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('42P16');
});

test('the views page renames a view and rewrites its dependents', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // ALTER VIEW RENAME TO + RENAME COLUMN, then jed_views: the dependent view reads the
  // renamed view under its old label, with a column alias list keeping the old column name (§7.1).
  const panel = livePanel(page, 'renameView');
  await expect(panel.getByTestId('result-rows')).toContainText('"on_duty" as "active_staff"');
  await expect(panel.getByTestId('error-code')).toHaveCount(0);
});

test('the views page writes through an updatable view', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // UPDATE / INSERT / DELETE through open_tasks, then the table (views.md §15): task 1
  // renamed, task 4 inserted with done defaulted to false, Ken's open task 3 deleted, and the done
  // task 2 untouched.
  const panel = livePanel(page, 'writeQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('write more docs');
  await expect(panel.getByTestId('result-rows')).toContainText('plan');
  await expect(panel.getByTestId('result-rows')).toContainText('ship it');
  await expect(panel.getByTestId('result-rows')).not.toContainText('review');
  await expect(panel.getByTestId('error-code')).toHaveCount(0);

  // A computed view column is read-only (0A000).
  await panel.getByTestId('sql-input').fill("UPDATE open_tasks SET owner_tag = 'X'");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('0A000');
});

test('the views page upserts through an updatable view', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // INSERT … ON CONFLICT through open_tasks (views.md §15.7): the done task 2, which the view hides,
  // is updated anyway, and task 5 is inserted.
  const panel = livePanel(page, 'upsertQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('ship it again');
  await expect(panel.getByTestId('result-rows')).toContainText('triage');
  await expect(panel.getByTestId('error-code')).toHaveCount(0);

  // A computed view column matches no unique constraint (42P10).
  await panel
    .getByTestId('sql-input')
    .fill("INSERT INTO open_tasks (id, title) VALUES (1, 'x') ON CONFLICT (owner_tag) DO NOTHING");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('42P10');
});

test('the views page enforces WITH CHECK OPTION', async ({ page }) => {
  await page.goto('/docs/sql/views/');
  // INSERT through ada_tasks of a row the view shows succeeds (views.md §15.6).
  const panel = livePanel(page, 'checkQuery');
  await expect(panel.getByTestId('result-rows')).toContainText('blog post');
  await expect(panel.getByTestId('error-code')).toHaveCount(0);

  // A row the view would hide, inserted or moved there by an UPDATE, is 44000.
  await panel.getByTestId('sql-input').fill("INSERT INTO ada_tasks VALUES (4, 'triage', 'ken')");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('44000');
  await panel.getByTestId('sql-input').fill("UPDATE ada_tasks SET owner = 'ken' WHERE id = 1");
  await panel.getByTestId('run-button').click();
  await expect(panel.getByTestId('error-code')).toHaveText('44000');
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

test('the host-functions API page wires up the idiomatic example for each language', async ({
  page
}) => {
  await page.goto('/docs/api/host-functions/');
  // Default (Rust): build an ExtensionRegistry and register a HostFunction.
  await expect(page.getByTestId('code-rust')).toBeVisible();
  await expect(page.getByTestId('code-rust')).toContainText('ExtensionRegistry');
  await expect(page.getByTestId('code-rust')).toContainText('register_function');

  // Go: NewHostFunction + RegisterFunction on the registry.
  await page.getByTestId('lang-go').click();
  await expect(page.getByTestId('code-go')).toContainText('NewHostFunction');
  await expect(page.getByTestId('code-go')).toContainText('RegisterFunction');

  // TypeScript: the options-object registerFunction against the jed-ts package.
  await page.getByTestId('lang-ts').click();
  await expect(page.getByTestId('code-ts')).toContainText('registerFunction');
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
  // EXPLAIN SELECT ... WHERE id = 3: a PK point lookup under the residual Filter.
  const panel = livePanel(page, 'pkBound');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 3');
  await expect(panel.getByTestId('result-rows')).toContainText('Filter');
  await expect(panel).toContainText('est_rows');
  await expect(panel).toContainText('est_cost');
});

test('the explain page shows bounded top-k on a blocking sort', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  const panel = livePanel(page, 'fullScan');
  await expect(panel.getByTestId('result-rows')).toContainText('Sort');
  await expect(panel.getByTestId('result-rows')).toContainText('keys=1, top-k=2');
});

test('the explain page shows the OR / IN-list interval-set access path', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // EXPLAIN SELECT ... WHERE id IN (1, 3, 5): a union of point probes, labelled
  // "PK interval set" (cost.md §3 "OR / IN-list").
  const panel = livePanel(page, 'pointSet');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText('PK interval set: id; intervals=3');
});

test('the explain page shows key = ANY(array) as an interval set', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // EXPLAIN SELECT ... WHERE id = ANY(ARRAY[1, 3, 5]): one array disjunct whose elements are the
  // point probes (cost.md §3 "OR / IN-list").
  const panel = livePanel(page, 'anyPointSet');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText(
    'PK interval set: id; intervals=1; arrays=1'
  );
});

test('the explain page shows the index-nested-loop access path on a join', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // The demo authors city first, but P7 physically drives trip and makes city the per-outer-row
  // PK seek, labelled Index-nested-loop.
  const panel = livePanel(page, 'indexNestedLoop');
  await expect(panel.getByTestId('result-rows')).toContainText('Nested Loop');
  await expect(panel.getByTestId('result-rows')).toContainText(
    'Index-nested-loop PK bound: id = join'
  );
});

test('the explain page shows the deterministic hash-join operator', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // An equality join between two columns with no usable index.
  const panel = livePanel(page, 'hashJoin');
  await expect(panel.getByTestId('result-rows')).toContainText('Hash Join');
  await expect(panel.getByTestId('result-rows')).toContainText('inner; keys=1; on:conjuncts=1');
});

test('the explain page shows a bounded N-way physical join tree', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  const panel = livePanel(page, 'nwayJoin');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan city');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan trip');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan region');
});

test('the explain page shows a WHERE conjunct pushed into its scan', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // a join whose single-table conjuncts run inside each table's scan (planner.md §3.2).
  const panel = livePanel(page, 'pushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('filter:conjuncts=1');
  await expect(panel.getByTestId('result-rows')).not.toContainText('Filter');
});

test('the explain page shows a proven WHERE contradiction', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // `zone > 5 AND zone < 3`: nothing is read (planner.md §3.1).
  const panel = livePanel(page, 'contradiction');
  await expect(panel.getByTestId('result-rows')).toContainText('conjuncts=2; contradiction');
});

test('the explain page shows a WHERE conjunct moved into a FROM subquery', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // The subquery's body plans the moved `id = 3` as a PK lookup (planner.md §3.2).
  const panel = livePanel(page, 'derivedPushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('Subquery v');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 3');
});

test('the explain page shows a WHERE conjunct moved below a grouped subquery', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // The grouping-column `region = 2` bounds city_region under the Aggregate (§3.2).
  const panel = livePanel(page, 'groupedPushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('Aggregate');
  await expect(panel.getByTestId('result-rows')).toContainText('Index bound: using city_region');
});

test('the explain page shows a WHERE conjunct moved below a window', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // `region = 2` is every window's PARTITION BY key, so it bounds city_region below the Window (§3.2).
  const panel = livePanel(page, 'windowPushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('Window');
  await expect(panel.getByTestId('result-rows')).toContainText('Index bound: using city_region');
});

test('the explain page shows a WHERE conjunct moved into an inlined CTE', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // The single-reference CTE is planned at its CTE Scan, where `id = 3` is a PK lookup (§3.4).
  const panel = livePanel(page, 'ctePushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('CTE Scan c');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 3');
});

test('the explain page shows a WHERE conjunct moved into every set-operation branch', async ({
  page
}) => {
  await page.goto('/docs/sql/explain/');
  // `x.id = 3` seeks city's PK in one UNION branch and filters trip in the other.
  const panel = livePanel(page, 'setopPushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('Union');
  await expect(panel.getByTestId('result-rows')).toContainText('PK bound: id = 3');
  await expect(panel.getByTestId('result-rows')).toContainText('Scan trip');
});

test('the explain page shows an ON conjunct pushed to the NULL-extended side', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // `t.id > 5` runs in trip's scan; the LEFT join keeps one ON conjunct (§3.3).
  const panel = livePanel(page, 'onPushdown');
  await expect(panel.getByTestId('result-rows')).toContainText('left; on:conjuncts=1');
  await expect(panel.getByTestId('result-rows')).toContainText('filter:conjuncts=1');
});

test('the explain page runs EXPLAIN ANALYZE with a deterministic cost', async ({ page }) => {
  await page.goto('/docs/sql/explain/');
  // EXPLAIN ANALYZE: the Analyze root reports the real accrued cost + row count.
  const panel = livePanel(page, 'analyze');
  await expect(panel.getByTestId('result-rows')).toContainText('Analyze');
  await expect(panel.getByTestId('result-rows')).toContainText('cost=');
});

test('the pattern-matching page autoruns the regexp_matches and split demos', async ({ page }) => {
  await page.goto('/docs/sql/pattern-matching/');
  // regexp_matches with `g`: one row per key=value pair.
  const matchesRows = livePanel(page, 'regexpMatches').getByTestId('result-rows');
  await expect(matchesRows).toContainText('port');
  await expect(matchesRows).toContainText('5432');
  // regexp_split_to_table: the runs of whitespace split the words.
  const tableRows = livePanel(page, 'regexpSplitTable').getByTestId('result-rows');
  await expect(tableRows).toContainText('quick');
  await expect(tableRows).toContainText('brown');
  // regexp_split_to_array over a seeded column.
  const arrayRows = livePanel(page, 'regexpSplitArray').getByTestId('result-rows');
  await expect(arrayRows).toContainText('{bob,work,example,org}');
});
