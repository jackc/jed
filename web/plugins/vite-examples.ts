// Build-time syntax highlighting for the per-language code examples (CLAUDE.md §5/§6). A Vite plugin
// that exposes a virtual module `virtual:jed-examples`: it reads each example's real per-language
// source files and emits Shiki-highlighted HTML. Because the highlighting happens HERE (Node, at
// build/dev time), Shiki never enters the CLIENT bundle — the page ships only the rendered HTML
// strings (one per language), and the language selector reveals one at runtime (zero highlighter JS).
//
// Example layout (data-over-code): src/lib/content/examples/<id>/{rust.rs, go.go, ts.ts}. The folder
// name is the topic id a <CodeTabs topic="..."/> references; adding a language = adding a file.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { createHighlighter, type Highlighter } from 'shiki';

const VIRTUAL_ID = 'virtual:jed-examples';
const RESOLVED_ID = '\0' + VIRTUAL_ID;
const THEME = 'github-light';

// Map a source filename to (our language id, Shiki grammar). Future cores/wrappers (Java/C#/Swift)
// are one row each — keep in sync with src/lib/content/languages.ts.
const LANG_FILES: Record<string, { id: string; grammar: string }> = {
	'rust.rs': { id: 'rust', grammar: 'rust' },
	'go.go': { id: 'go', grammar: 'go' },
	'ts.ts': { id: 'ts', grammar: 'typescript' }
};

const GRAMMARS = ['rust', 'go', 'typescript'];

type RawVariant = { id: string; grammar: string; code: string; file: string };

function readExamples(dir: string): { examples: Record<string, RawVariant[]>; files: string[] } {
	const examples: Record<string, RawVariant[]> = {};
	const files: string[] = [];
	if (!existsSync(dir)) return { examples, files };
	for (const topic of readdirSync(dir)) {
		const topicDir = join(dir, topic);
		if (!statSync(topicDir).isDirectory()) continue;
		const variants: RawVariant[] = [];
		for (const file of readdirSync(topicDir)) {
			const meta = LANG_FILES[file];
			if (!meta) continue;
			const path = join(topicDir, file);
			files.push(path);
			variants.push({
				id: meta.id,
				grammar: meta.grammar,
				code: readFileSync(path, 'utf8').replace(/\s+$/, '') + '\n',
				file: path
			});
		}
		if (variants.length > 0) examples[topic] = variants;
	}
	return { examples, files };
}

export function jedExamples(examplesDir: string) {
	let highlighter: Highlighter | null = null;
	let watchFiles: string[] = [];

	async function buildModule(): Promise<string> {
		if (highlighter === null) {
			highlighter = await createHighlighter({ themes: [THEME], langs: GRAMMARS });
		}
		const { examples, files } = readExamples(examplesDir);
		watchFiles = files;
		const out: Record<string, Record<string, string>> = {};
		for (const [topic, variants] of Object.entries(examples)) {
			out[topic] = {};
			for (const v of variants) {
				out[topic][v.id] = highlighter.codeToHtml(v.code, { lang: v.grammar, theme: THEME });
			}
		}
		return `export default ${JSON.stringify(out)};`;
	}

	return {
		name: 'jed-examples',
		resolveId(id: string) {
			if (id === VIRTUAL_ID) return RESOLVED_ID;
			return null;
		},
		async load(this: { addWatchFile?: (f: string) => void }, id: string) {
			if (id !== RESOLVED_ID) return null;
			const code = await buildModule();
			// Editing an example source invalidates the virtual module in dev.
			for (const f of watchFiles) this.addWatchFile?.(f);
			return code;
		}
	};
}
