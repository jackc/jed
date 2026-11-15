import { expect, test } from '@playwright/test';
import { DOCS_NAV } from '../src/lib/content/docs-nav.ts';

// Check the built HTML without opening every page's live database workers. The suite's default
// /jed/ prefix is deliberately the same shape as GitHub Pages, so root-relative links fail here.
test('all website links and local fragments resolve within the deployment prefix', async ({
  request,
  baseURL
}) => {
  const root = new URL(baseURL!);
  const routes = [
    '/',
    '/tool/',
    ...DOCS_NAV.flatMap((section) => section.links.map((link) => link.href))
  ];
  const pages = new Map<string, string>();
  for (const route of routes) {
    const url = new URL('.' + route, root);
    const response = await request.get(url.href);
    expect(response.status(), url.href).toBe(200);
    pages.set(url.href, await response.text());
  }
  for (const [source, html] of pages) {
    for (const match of html.matchAll(/<a\b[^>]*\bhref="([^"]+)"/g)) {
      const target = new URL(match[1]!.replaceAll('&amp;', '&'), source);
      if (target.origin !== root.origin) continue;
      expect(
        target.pathname.startsWith(root.pathname),
        `${source} links outside the base: ${target}`
      ).toBe(true);
      const fragment = decodeURIComponent(target.hash.slice(1));
      target.hash = '';
      let body = pages.get(target.href);
      if (body === undefined) {
        const response = await request.get(target.href);
        expect(response.status(), `${source} -> ${target}`).toBe(200);
        body = await response.text();
        pages.set(target.href, body);
      }
      if (fragment) {
        const ids = [...body.matchAll(/\bid="([^"]+)"/g)].map((id) => id[1]);
        expect(ids, `${source} -> ${target}#${fragment}`).toContain(fragment);
      }
    }
  }
});
