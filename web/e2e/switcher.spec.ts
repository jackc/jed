import { expect, test } from '@playwright/test';

// Language switcher tests (Phase 3/5): the nav selector switches every CodeTabs site-wide, the
// choice persists across reload and client-side navigation, and a build-time-highlighted block is
// shown per language. Targets the API docs page, where CodeTabs lives (the SQL pages have no
// selector — that's the two-axis split).

const API = '/docs/api/opening-a-database/';

test('default language (Rust) example is shown', async ({ page }) => {
  await page.goto(API);
  await expect(page.getByTestId('code-rust')).toBeVisible();
  await expect(page.getByTestId('code-rust')).toContainText('fn main');
  await expect(page.getByTestId('code-go')).toBeHidden();
  await expect(page.getByTestId('code-ts')).toBeHidden();
});

test('selecting a language reveals its variant and hides the others', async ({ page }) => {
  await page.goto(API);
  await page.getByTestId('lang-go').click();
  await expect(page.getByTestId('code-go')).toBeVisible();
  await expect(page.getByTestId('code-go')).toContainText('package main');
  await expect(page.getByTestId('code-rust')).toBeHidden();

  await page.getByTestId('lang-ts').click();
  await expect(page.getByTestId('code-ts')).toBeVisible();
  await expect(page.getByTestId('code-ts')).toContainText('jed-ts');
  await expect(page.getByTestId('code-go')).toBeHidden();
});

test('the choice persists across reload', async ({ page }) => {
  await page.goto(API);
  await page.getByTestId('lang-go').click();
  await expect(page.getByTestId('code-go')).toBeVisible();
  await page.reload();
  await expect(page.getByTestId('code-go')).toBeVisible();
  await expect(page.getByTestId('code-rust')).toBeHidden();
});

test('the choice carries across client-side navigation', async ({ page }) => {
  await page.goto(API);
  await page.getByTestId('lang-ts').click();
  await expect(page.getByTestId('code-ts')).toBeVisible();
  // Navigate away (Home, no CodeTabs) and back via the sidebar — the selection survives.
  await page.getByRole('link', { name: 'Home' }).click();
  await page.getByRole('link', { name: 'Docs', exact: true }).click();
  await page.getByRole('link', { name: 'Opening a database', exact: true }).click();
  await expect(page.getByTestId('code-ts')).toBeVisible();
  await expect(page.getByTestId('code-rust')).toBeHidden();
});
