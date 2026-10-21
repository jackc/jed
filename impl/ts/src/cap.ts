// A database's page capacities (spec/fileformat/format.md *Page model*; encrypted.md §1–§2) — what
// every split, record-cap, fit, chain and pointer-width rule reads. Its own dependency-free module so
// the B+tree (pmap.ts), the stores, the format layer and the codec (crypto.ts) share one definition
// without an import cycle.
//
// c is the payload capacity C every size rule uses; tag is the bytes a bound pointer adds to a page
// index: 0 in a plaintext file (and every in-memory or temp store), 16 in an encrypted one, whose C
// also excludes the 40-byte reserve (nonce ‖ tag) at the end of every sealed page.

export type Cap = { readonly c: number; readonly tag: number };

// The page header every body page carries (format.md *Page header*).
const PAGE_HEADER_BYTES = 16;
// The bytes a bound pointer adds to a page index — the AEAD tag of the page it names (encrypted.md §2).
export const TAG_LEN = 16;
// Reserved bytes at the end of every sealed page: the 24-byte nonce, then the 16-byte tag.
export const RESERVE = 40;
// Pages 0–1 are the meta slots; an encrypted file's 2–3 are its key block, so its body starts at 4.
export const FIRST_BODY_PLAIN = 2;
export const FIRST_BODY_ENCRYPTED = 4;

// NO_TAG is the tag of a pointer to no page, and of every pointer of a plaintext file (which stores
// none). Shared and never written: callers copy a tag before changing bytes.
export const NO_TAG: Uint8Array = new Uint8Array(TAG_LEN);

// plainCap is a plaintext file's (or an in-memory/temp store's) capacities at pageSize.
export function plainCap(pageSize: number): Cap {
  return { c: pageSize - PAGE_HEADER_BYTES, tag: 0 };
}

// encryptedCap is an encrypted file's capacities at pageSize (encrypted.md §1).
export function encryptedCap(pageSize: number): Cap {
  return { c: pageSize - PAGE_HEADER_BYTES - RESERVE, tag: TAG_LEN };
}

export function capOf(pageSize: number, encrypted: boolean): Cap {
  return encrypted ? encryptedCap(pageSize) : plainCap(pageSize);
}

export function capEncrypted(cap: Cap): boolean {
  return cap.tag !== 0;
}

// capPtr is the bytes one page pointer takes: a u32 index, plus its tag when bound.
export function capPtr(cap: Cap): number {
  return 4 + cap.tag;
}

// capChain is the content one chain page (catalog, overflow, free-list) holds after its successor's
// tag.
export function capChain(cap: Cap): number {
  return cap.c - cap.tag;
}

// capFirstBody is the first body page: 2, or 4 past an encrypted file's key block.
export function capFirstBody(cap: Cap): number {
  return cap.tag !== 0 ? FIRST_BODY_ENCRYPTED : FIRST_BODY_PLAIN;
}

// tagsEqual compares two tags in constant time over their full width (a tag is public — it is stored
// next to the page — but there is no reason to branch on its bytes).
export function tagsEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

// isNoTag reports whether tag is all zeros (a pointer to no page carries one).
export function isNoTag(tag: Uint8Array): boolean {
  for (let i = 0; i < tag.length; i++) if (tag[i] !== 0) return false;
  return true;
}
