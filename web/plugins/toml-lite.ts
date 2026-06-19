// A tiny, dependency-free reader for the spec's CANONICAL data tables (CLAUDE.md §5): the flat
// array-of-tables TOML in spec/types/scalars.toml, spec/errors/registry.toml, spec/functions/
// catalog.toml. It is deliberately NOT a general TOML parser — it handles exactly the subset those
// files use (comments, `[[table]]` arrays-of-tables, `key = value` with strings / numbers / bools /
// flat arrays / one-level inline tables). "Boring, explicit code over a dependency" (§10/§14): the
// website generates its type/error/function reference pages from this canonical data, so it never
// drifts from the spec.

export type TomlValue = string | number | boolean | TomlValue[] | { [k: string]: TomlValue };

export type SpecToml = {
	top: Record<string, TomlValue>;
	tables: Record<string, Record<string, TomlValue>[]>;
};

// Strip a `#` line comment, but never one inside a "basic string" (these files use only double
// quotes; no message template contains a literal `#`).
function stripComment(line: string): string {
	let inString = false;
	for (let i = 0; i < line.length; i++) {
		const c = line[i];
		if (c === '"') inString = !inString;
		else if (c === '#' && !inString) return line.slice(0, i);
	}
	return line;
}

// Split on top-level commas (not inside quotes / brackets / braces) — for arrays and inline tables.
function splitTopLevel(s: string): string[] {
	const out: string[] = [];
	let depth = 0;
	let inString = false;
	let start = 0;
	for (let i = 0; i < s.length; i++) {
		const c = s[i];
		if (c === '"') inString = !inString;
		else if (!inString && (c === '[' || c === '{')) depth++;
		else if (!inString && (c === ']' || c === '}')) depth--;
		else if (!inString && c === ',' && depth === 0) {
			out.push(s.slice(start, i));
			start = i + 1;
		}
	}
	out.push(s.slice(start));
	return out.map((p) => p.trim()).filter((p) => p !== '');
}

function parseValue(s: string): TomlValue {
	if (s.startsWith('"')) return s.slice(1, s.lastIndexOf('"'));
	if (s === 'true') return true;
	if (s === 'false') return false;
	if (s.startsWith('[')) {
		const inner = s.slice(1, s.lastIndexOf(']')).trim();
		return inner === '' ? [] : splitTopLevel(inner).map(parseValue);
	}
	if (s.startsWith('{')) {
		const inner = s.slice(1, s.lastIndexOf('}')).trim();
		const obj: Record<string, TomlValue> = {};
		if (inner !== '') {
			for (const part of splitTopLevel(inner)) {
				const eq = part.indexOf('=');
				obj[part.slice(0, eq).trim()] = parseValue(part.slice(eq + 1).trim());
			}
		}
		return obj;
	}
	// Integers: keep big i64 bounds (beyond Number.MAX_SAFE_INTEGER) as their exact digit string.
	if (/^-?\d+$/.test(s)) {
		const n = Number(s);
		return Number.isSafeInteger(n) ? n : s;
	}
	if (/^-?\d+\.\d+$/.test(s)) return Number(s);
	return s;
}

export function parseSpecToml(text: string): SpecToml {
	const top: Record<string, TomlValue> = {};
	const tables: Record<string, Record<string, TomlValue>[]> = {};
	let current: Record<string, TomlValue> | null = null;

	for (const rawLine of text.split('\n')) {
		const line = stripComment(rawLine).trim();
		if (line === '') continue;

		const aot = /^\[\[(\w+)\]\]$/.exec(line);
		if (aot) {
			current = {};
			(tables[aot[1]!] ??= []).push(current);
			continue;
		}
		// A single-bracket [table] header has no use in these files — skip it.
		if (line.startsWith('[')) continue;

		const eq = line.indexOf('=');
		if (eq === -1) continue;
		const key = line.slice(0, eq).trim();
		const value = parseValue(line.slice(eq + 1).trim());
		if (current) current[key] = value;
		else top[key] = value;
	}

	return { top, tables };
}
