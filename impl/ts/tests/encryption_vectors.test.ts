// The encrypted variant's shared vectors (spec/fileformat/encryption_vectors.toml, authored by the
// Ruby reference spec/fileformat/xaes.rb): the XAES-256-GCM derivation this core writes over the
// backend's raw AES block (the C2SP vectors plus a page-AD case), the HKDF subkeys and host-key KEK,
// the goldens' key block drawn from the seeded key stream, and Argon2id. Mirrors the vector tests in
// impl/rust/src/crypto.rs.

import "../src/crypto_node.ts";

import assert from "node:assert/strict";
import { test } from "node:test";
import { argon2id } from "@noble/hashes/argon2.js";

import {
  createCodec,
  DEFAULT_ARGON2_PARAMS,
  hkdf,
  newKeyBlock,
  NO_TAG,
  unlockCodec,
  XaesKey,
} from "../src/crypto.ts";
import { seededRandomSource } from "../src/seam.ts";
import { readTomlSection, readTomlTables, specPath } from "./tomlmini.ts";
import { bytesToHex, errCode } from "./util.ts";

const VECTORS = specPath("fileformat/encryption_vectors.toml");

function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function sealed(
  key: Uint8Array,
  nonce: Uint8Array,
  plaintext: Uint8Array,
  aad: Uint8Array,
): string {
  const { ciphertext, tag } = new XaesKey(key).seal(nonce, plaintext, aad);
  return bytesToHex(ciphertext) + bytesToHex(tag);
}

test("XAES-256-GCM reproduces the shared vectors", () => {
  const rows = readTomlTables(VECTORS, "xaes");
  assert.equal(rows.length, 3);
  for (const v of rows) {
    const key = unhex(v.str("key_hex"));
    const nonce = unhex(v.str("nonce_hex"));
    const plaintext = unhex(v.str("plaintext_hex"));
    const aad = unhex(v.str("aad_hex"));
    assert.equal(sealed(key, nonce, plaintext, aad), v.str("sealed_hex"), v.str("name"));
    // Open round-trips, and a changed AD fails authentication.
    const all = unhex(v.str("sealed_hex"));
    const ct = all.subarray(0, plaintext.length);
    const tag = all.subarray(plaintext.length);
    const k = new XaesKey(key);
    assert.equal(bytesToHex(k.open(nonce, ct, aad, tag)!), v.str("plaintext_hex"));
    assert.equal(k.open(nonce, ct, Uint8Array.of(0x78), tag), null);
  }
});

test("subkeys and the host-key KEK match the shared vectors", () => {
  const sub = readTomlSection(VECTORS, "subkeys");
  const dek = unhex(sub.str("dek_hex"));
  assert.equal(bytesToHex(hkdf(dek, new Uint8Array(0), "jed/v1/page")), sub.str("page_key_hex"));
  assert.equal(bytesToHex(hkdf(dek, new Uint8Array(0), "jed/v1/meta")), sub.str("meta_key_hex"));
  const kek = readTomlSection(VECTORS, "host_kek");
  assert.equal(
    bytesToHex(hkdf(unhex(kek.str("key_hex")), unhex(kek.str("kdf_salt_hex")), "jed/v1/kek")),
    kek.str("kek_hex"),
  );
});

test("the goldens' key block is the seeded stream's first draws", () => {
  const kb = readTomlSection(VECTORS, "key_block");
  const key = unhex(readTomlSection(VECTORS, "host_kek").str("key_hex"));
  const { block, dek } = newKeyBlock(
    { key },
    DEFAULT_ARGON2_PARAMS,
    seededRandomSource(kb.big("seed")),
  );
  assert.equal(bytesToHex(block), kb.str("block_hex"));
  assert.equal(bytesToHex(dek), kb.str("dek_hex"));
});

test("Argon2id matches the RFC 9106 reference vector", () => {
  const a = readTomlSection(VECTORS, "argon2id");
  const out = argon2id(new TextEncoder().encode(a.str("password")), a.str("salt"), {
    m: a.num("memory_kib"),
    t: a.num("iterations"),
    p: a.num("parallelism"),
    dkLen: 32,
    version: 0x13,
  });
  assert.equal(bytesToHex(out), a.str("key_hex"));
});

test("a key block unwraps with its credential only", () => {
  const rng = seededRandomSource(7n);
  const cred = { key: new Uint8Array(32).fill(9) };
  const codec = createCodec(cred, DEFAULT_ARGON2_PARAMS, rng);
  const page = new Uint8Array(1024);
  page.set(codec.keyBlock, 0);
  unlockCodec(page, page, cred, rng);
  const code = (fn: () => unknown): string =>
    errCode(() => {
      fn();
    });
  assert.equal(
    code(() => unlockCodec(page, page, { key: new Uint8Array(32).fill(8) }, rng)),
    "28P01",
  );
  assert.equal(
    code(() => unlockCodec(page, page, { passphrase: "x" }, rng)),
    "28P01",
  );
  // A torn copy falls back to the other; two torn copies are XX001.
  const torn = page.slice();
  torn[40]! ^= 1;
  unlockCodec(page, torn, cred, rng);
  assert.equal(
    code(() => unlockCodec(torn, torn, cred, rng)),
    "XX001",
  );
});

test("a body page binds its index and the pointer's tag", () => {
  const codec = createCodec(
    { key: new Uint8Array(32).fill(1) },
    DEFAULT_ARGON2_PARAMS,
    seededRandomSource(1n),
  );
  const page = new Uint8Array(1024);
  page[0] = 2;
  page.set([0x72, 0x6f, 0x77, 0x73], 16);
  const plain = page.slice(16, 1024 - 40);
  const tag = codec.sealBody(9, page);
  const opened = codec.openBody(9, page, tag);
  assert.equal(bytesToHex(opened.subarray(16)), bytesToHex(plain));
  const code = (fn: () => unknown): string =>
    errCode(() => {
      fn();
    });
  assert.equal(
    code(() => codec.openBody(10, page, tag)),
    "XX001",
  );
  assert.equal(
    code(() => codec.openBody(9, page, NO_TAG)),
    "XX001",
  );
});
