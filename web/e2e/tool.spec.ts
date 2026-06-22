import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

// Full database-tool flow (Phase 6): the CodeMirror editor + results grid + schema sidebar + output
// formats, and the OPFS file manager (create / insert / reopen-across-reload / export / import /
// drop, plus malformed-file handling). OPFS storage is partitioned per Playwright context, so each
// test starts with an empty origin-private file system.

// Type SQL into the CodeMirror editor: select all, delete, then insert (insertText avoids CM's
// per-keystroke auto-indent, so multi-line SQL lands verbatim).
async function setEditor(page: Page, sql: string): Promise<void> {
  const content = page.getByTestId('sql-editor').locator('.cm-content');
  await content.click();
  await page.keyboard.press('ControlOrMeta+a');
  await page.keyboard.press('Delete');
  await page.keyboard.insertText(sql);
}

test('autoruns the default SQL and populates the schema sidebar', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('result-rows')).toContainText('hello');
  await expect(page.getByTestId('schema-sidebar')).toContainText('note');
});

test('output-format selector renders csv / json / markdown', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('result-rows')).toContainText('hello');

  await page.getByTestId('format-select').selectOption('csv');
  await expect(page.getByTestId('formatted-output')).toContainText('id,body');

  await page.getByTestId('format-select').selectOption('json');
  await expect(page.getByTestId('formatted-output')).toContainText('"body"');

  await page.getByTestId('format-select').selectOption('markdown');
  await expect(page.getByTestId('formatted-output')).toContainText('| id |');
});

test('editing and running updates the grid and shows cost', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('run-button')).toBeEnabled();
  await setEditor(page, 'SELECT count(*) AS n FROM note');
  await page.getByTestId('run-button').click();
  await expect(page.getByTestId('result-rows')).toContainText('2');
  await expect(page.getByTestId('total-cost')).toContainText('cost');
});

test('OPFS is supported and the file manager is available', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('create-db')).toBeVisible();
  await expect(page.getByTestId('opfs-unsupported')).toHaveCount(0);
});

test('OPFS: create, insert, then reopen across a reload persists data', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('run-button')).toBeEnabled();

  await page.getByTestId('new-db-name').fill('persist');
  await page.getByTestId('create-db').click();
  await expect(page.getByTestId('db-label')).toContainText('persist.jed');

  await setEditor(
    page,
    "CREATE TABLE t (id i32 PRIMARY KEY, v text); INSERT INTO t VALUES (1, 'kept'); SELECT * FROM t;"
  );
  await page.getByTestId('run-button').click();
  await expect(page.getByTestId('result-rows')).toContainText('kept');

  // Reload (same context → OPFS survives) and reopen the file from the list.
  await page.reload();
  await expect(page.getByTestId('run-button')).toBeEnabled();
  await page.getByRole('button', { name: 'persist.jed' }).click();
  await setEditor(page, 'SELECT v FROM t');
  await page.getByTestId('run-button').click();
  await expect(page.getByTestId('result-rows')).toContainText('kept');
});

test('OPFS: export then re-import round-trips a database', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('run-button')).toBeEnabled();

  await page.getByTestId('new-db-name').fill('rt');
  await page.getByTestId('create-db').click();
  await setEditor(
    page,
    "CREATE TABLE t (id i32 PRIMARY KEY, v text); INSERT INTO t VALUES (1, 'roundtrip');"
  );
  await page.getByTestId('run-button').click();
  await expect(page.getByTestId('db-label')).toContainText('rt.jed');

  // Export downloads the file (the tool closes the db first to release the exclusive handle).
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    page.getByTitle('export').first().click()
  ]);
  const buffer = readFileSync(await download.path());

  // Delete it, then re-import the downloaded bytes and open — the data survives the round-trip.
  await page.getByTitle('delete').first().click();
  await expect(page.getByTestId('file-list')).not.toContainText('rt.jed');

  await page
    .getByTestId('import-input')
    .setInputFiles({ name: 'rt.jed', mimeType: 'application/octet-stream', buffer });
  await expect(page.getByTestId('file-list')).toContainText('rt.jed');

  await page.getByRole('button', { name: 'rt.jed' }).click();
  await setEditor(page, 'SELECT v FROM t');
  await page.getByTestId('run-button').click();
  await expect(page.getByTestId('result-rows')).toContainText('roundtrip');
});

test('OPFS: importing a non-jed file surfaces XX001 on open', async ({ page }) => {
  await page.goto('/tool/');
  await expect(page.getByTestId('run-button')).toBeEnabled();

  await page.getByTestId('import-input').setInputFiles({
    name: 'bad.jed',
    mimeType: 'application/octet-stream',
    buffer: Buffer.from('not a jed database')
  });
  await expect(page.getByTestId('file-list')).toContainText('bad.jed');

  await page.getByRole('button', { name: 'bad.jed' }).click();
  await expect(page.getByTestId('file-msg')).toContainText('XX001');
});
