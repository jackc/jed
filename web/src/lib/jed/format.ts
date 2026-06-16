// Output formatters for the database tool's result grid — the same five forms the jed CLI offers
// (cli/src/render.rs §5). The worker already renders every Value to its canonical display string, so
// these operate on the rendered rows. `table` is rendered by ResultGrid; this module produces the
// text forms (csv / json / markdown) shown in a copyable <pre>.

import type { RunResult } from './protocol.ts';

export type OutputFormat = 'table' | 'csv' | 'json' | 'markdown';

type Query = Extract<RunResult, { kind: 'query' }>;

// RFC 4180: quote a field iff it contains a comma, quote, or newline; double embedded quotes.
function csvField(s: string): string {
	return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

export function toCsv(q: Query): string {
	const head = q.columnNames.map(csvField).join(',');
	const body = q.rows.map((r) => r.map(csvField).join(',')).join('\n');
	return body ? `${head}\n${body}` : head;
}

// JSON: an array of objects. Values are the rendered strings (a typed wire form is a later
// enhancement — the worker currently renders to strings, mirroring the CLI display).
export function toJson(q: Query): string {
	const objs = q.rows.map((r) => {
		const o: Record<string, string> = {};
		q.columnNames.forEach((name, i) => (o[name] = r[i] ?? ''));
		return o;
	});
	return JSON.stringify(objs, null, 2);
}

// Markdown: a GitHub-style pipe table. Pipes are escaped and newlines become <br>.
function mdCell(s: string): string {
	return s.replace(/\|/g, '\\|').replace(/\n/g, '<br>');
}

export function toMarkdown(q: Query): string {
	const head = `| ${q.columnNames.map(mdCell).join(' | ')} |`;
	const rule = `| ${q.columnNames.map(() => '---').join(' | ')} |`;
	const body = q.rows.map((r) => `| ${r.map(mdCell).join(' | ')} |`).join('\n');
	return body ? `${head}\n${rule}\n${body}` : `${head}\n${rule}`;
}

export function formatQuery(q: Query, format: OutputFormat): string {
	switch (format) {
		case 'csv':
			return toCsv(q);
		case 'json':
			return toJson(q);
		case 'markdown':
			return toMarkdown(q);
		default:
			return '';
	}
}
