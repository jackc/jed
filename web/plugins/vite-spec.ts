// Exposes the spec's canonical data tables (CLAUDE.md §5) to the website as the virtual module
// `virtual:jed-spec`, parsed at build time from spec/types/scalars.toml, spec/errors/registry.toml,
// and spec/functions/catalog.toml. The reference pages (types / errors / functions) render from
// THIS, so they are generated from the same canonical source the engine cores consume — they cannot
// drift from the spec. Parsing happens in Node at build/dev time; no parser ships to the client.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { parseSpecToml, type TomlValue } from './toml-lite.ts';

const VIRTUAL_ID = 'virtual:jed-spec';
const RESOLVED_ID = '\0' + VIRTUAL_ID;

const SOURCES = ['types/scalars.toml', 'errors/registry.toml', 'functions/catalog.toml'];

export function jedSpec(specDir: string) {
  function build(): string {
    const read = (rel: string) => parseSpecToml(readFileSync(join(specDir, rel), 'utf8'));
    const scalars = read('types/scalars.toml');
    const errors = read('errors/registry.toml');
    const catalog = read('functions/catalog.toml');
    const data: Record<string, Record<string, TomlValue>[]> = {
      types: scalars.tables.type ?? [],
      errors: errors.tables.error ?? [],
      operators: catalog.tables.operator ?? [],
      aggregates: catalog.tables.aggregate ?? [],
      setReturning: catalog.tables.set_returning ?? []
    };
    return `export default ${JSON.stringify(data)};`;
  }

  const watch = SOURCES.map((f) => join(specDir, f));

  return {
    name: 'jed-spec',
    resolveId(id: string) {
      if (id === VIRTUAL_ID) return RESOLVED_ID;
      return null;
    },
    load(this: { addWatchFile?: (f: string) => void }, id: string) {
      if (id !== RESOLVED_ID) return null;
      for (const f of watch) this.addWatchFile?.(f);
      return build();
    }
  };
}
