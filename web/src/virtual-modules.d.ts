// Ambient declarations for Vite virtual modules (a pure .d.ts so the declaration is global and
// visible to svelte-check, unlike one tucked inside app.d.ts's module scope).

// The build-time-highlighted code examples (plugins/vite-examples.ts): topic id -> language id ->
// Shiki-highlighted HTML string.
declare module 'virtual:jed-examples' {
	const examples: Record<string, Record<string, string>>;
	export default examples;
}

// The spec's canonical data tables (plugins/vite-spec.ts), parsed from spec/*.toml at build time.
declare module 'virtual:jed-spec' {
	type SpecCell = string | number | boolean | (string | number | boolean)[] | Record<string, unknown>;
	type SpecRow = Record<string, SpecCell>;
	const spec: { types: SpecRow[]; errors: SpecRow[]; operators: SpecRow[]; aggregates: SpecRow[] };
	export default spec;
}
