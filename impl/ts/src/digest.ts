// The digest functions md5 / sha224 / sha256 / sha384 / sha512 (spec/design/string-functions.md §4).
// The algorithms come from @noble/hashes, a vetted, dependency-free library that runs synchronously
// in Node and the browser alike (CLAUDE.md §14 clause 3: no hand-rolled crypto). MD5 and SHA-2 are
// standardized, so the digests match the Rust and Go cores byte-for-byte.

import { md5 } from "@noble/hashes/legacy.js";
import { sha224, sha256, sha384, sha512 } from "@noble/hashes/sha2.js";

const UTF8 = new TextEncoder();

// md5Hex is md5(text | bytea) → text: the RFC 1321 digest as 32 lowercase hex digits. A text
// argument (a JS string, UTF-16) is hashed as its UTF-8 bytes, as the other cores do.
export function md5Hex(input: string | Uint8Array): string {
  const sum = md5(typeof input === "string" ? UTF8.encode(input) : input);
  let out = "";
  for (const b of sum) out += b.toString(16).padStart(2, "0");
  return out;
}

// sha2Digest is sha224/sha256/sha384/sha512(bytea) → bytea: the raw FIPS 180-4 digest.
export function sha2Digest(
  func: "sha224" | "sha256" | "sha384" | "sha512",
  bytes: Uint8Array,
): Uint8Array {
  switch (func) {
    case "sha224":
      return sha224(bytes);
    case "sha256":
      return sha256(bytes);
    case "sha384":
      return sha384(bytes);
    case "sha512":
      return sha512(bytes);
  }
}
