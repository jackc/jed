// The docs sidebar structure (CLAUDE.md §6). Two content axes, kept as separate sections (the user
// note): the EMBEDDING API (per-language, switched by the nav selector) and the SQL itself
// (language-neutral, with live runnable panels), plus a generated REFERENCE. hrefs are base-relative
// (the layout prepends $app/paths base) and end in '/' to match trailingSlash: 'always'.

export type DocLink = { title: string; href: string };
export type DocSection = { title: string; links: DocLink[] };

export const DOCS_NAV: readonly DocSection[] = [
	{
		title: 'Getting started',
		links: [{ title: 'Introduction', href: '/docs/' }]
	},
	{
		title: 'Embedding API',
		links: [
			{ title: 'Opening a database', href: '/docs/api/opening-a-database/' },
			{ title: 'Transactions', href: '/docs/api/transactions/' },
			{ title: 'Running scripts', href: '/docs/api/scripts/' },
			{ title: 'Authorization', href: '/docs/api/authorization/' },
			{ title: 'Resource limits', href: '/docs/api/resource-limits/' }
		]
	},
	{
		title: 'SQL',
		links: [
			{ title: 'Types', href: '/docs/sql/types/' },
			{ title: 'Tables & constraints', href: '/docs/sql/tables/' },
			{ title: 'Indexes', href: '/docs/sql/indexes/' },
			{ title: 'Querying', href: '/docs/sql/select/' }
		]
	},
	{
		title: 'Reference',
		links: [
			{ title: 'Types', href: '/docs/reference/types/' },
			{ title: 'Functions & operators', href: '/docs/reference/functions/' },
			{ title: 'Error codes', href: '/docs/reference/errors/' }
		]
	}
];
