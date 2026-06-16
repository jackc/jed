// The set of host languages the docs' API examples can be shown in (CLAUDE.md §2 — the cores +
// wrappers). Data-driven so adding a future core/wrapper (Java/C#/Swift) is a one-line change here
// plus a source file per example (see plugins/vite-examples.ts). The language selector switches
// which variant every <CodeTabs> shows; it governs the host/EMBEDDING API axis only — the SQL that
// live panels run is identical across languages and has no selector.

export type LangId = 'rust' | 'go' | 'ts';

export type Language = { id: LangId; label: string };

// Order = display order in the selector. Rust first (the priority core, CLAUDE.md §2).
export const LANGUAGES: readonly Language[] = [
	{ id: 'rust', label: 'Rust' },
	{ id: 'go', label: 'Go' },
	{ id: 'ts', label: 'TypeScript' }
];

export const DEFAULT_LANG: LangId = 'rust';

export function isLangId(v: unknown): v is LangId {
	return typeof v === 'string' && LANGUAGES.some((l) => l.id === v);
}
