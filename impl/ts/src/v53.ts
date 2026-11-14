// PROTOTYPE: the v53 layout ahead of v53 (spec/design/manifest-crc32.md, deferred-interior.md §9). The
// Go and Rust cores write it behind three environment switches and no version bump; this core reads
// the same switches so the simulator holds all three to byte-identical images. At v53 each becomes
// unconditional and the version byte decides. Read once; a browser has no environment, so there every
// switch is off.

type Env = { process?: { env?: Record<string, string | undefined> } };

function switchOn(name: string): boolean {
  return (globalThis as Env).process?.env?.[name] === "on";
}

// Each switch is read on first use and kept, so a process sees one layout throughout.
const cache = new Map<string, boolean>();

function cached(name: string): boolean {
  let on = cache.get(name);
  if (on === undefined) {
    on = switchOn(name);
    cache.set(name, on);
  }
  return on;
}

// manifestCrc32 is JED_MANIFEST_CRC32=on: this process writes narrow manifests (manifest-crc32.md §2).
export function manifestCrc32(): boolean {
  return cached("JED_MANIFEST_CRC32");
}

// pageTxidStamp is JED_PAGE_TXID_STAMP=on: pages carry the writing commit's stamp (§2.1).
export function pageTxidStamp(): boolean {
  return cached("JED_PAGE_TXID_STAMP");
}

// deferInterior is JED_DEFER_INTERIOR=on: the deferred-interior layout and commit (§9).
export function deferInterior(): boolean {
  return cached("JED_DEFER_INTERIOR");
}
