import { base } from '$app/paths';
import { DOCS_NAV } from '$lib/content/docs-nav.ts';

// A prerendered sitemap for the static site. Routes are derived from the docs nav (so it tracks new
// pages) plus the home and tool. Absolute URLs use SITE_URL when set (the deploy workflow sets it to
// the Pages URL); otherwise base-relative paths.
export const prerender = true;

const ROUTES: string[] = [
	'/',
	'/tool/',
	...DOCS_NAV.flatMap((section) => section.links.map((link) => link.href))
];

export function GET(): Response {
	const origin = (process.env.SITE_URL ?? '').replace(/\/$/, '');
	const urls = ROUTES.map((route) => `\n  <url><loc>${origin}${base}${route}</loc></url>`).join('');
	const body = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${urls}\n</urlset>\n`;
	return new Response(body, { headers: { 'content-type': 'application/xml' } });
}
