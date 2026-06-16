// The site-wide host-language selection (CLAUDE.md §6). A single writable store, read by every
// <CodeTabs> so one choice re-skins all API examples across the whole site, and persisted to
// localStorage so it survives reloads and navigation. SSR-safe: during prerender there is no
// localStorage, so it starts at DEFAULT_LANG; the client re-reads the persisted value on load.

import { browser } from '$app/environment';
import { writable } from 'svelte/store';
import { DEFAULT_LANG, isLangId, type LangId } from '$lib/content/languages.ts';

const STORAGE_KEY = 'jed:lang';

function initial(): LangId {
	if (!browser) return DEFAULT_LANG;
	const stored = localStorage.getItem(STORAGE_KEY);
	return isLangId(stored) ? stored : DEFAULT_LANG;
}

export const lang = writable<LangId>(initial());

if (browser) {
	lang.subscribe((value) => localStorage.setItem(STORAGE_KEY, value));
}
