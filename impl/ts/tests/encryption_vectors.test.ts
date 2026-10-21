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
  encryptingSpillSink,
  hkdf,
  newKeyBlock,
  NO_TAG,
  SCRATCH_STORED,
  unlockCodec,
  unselectedKeyPage,
  XaesKey,
} from "../src/crypto.ts";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";
import { seededRandomSource } from "../src/seam.ts";
import type { SpillByteReader, SpillSink } from "../src/spill.ts";
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

test("a credential change rewraps the goldens' key block", () => {
  const kb = readTomlSection(VECTORS, "key_block");
  const rw = readTomlSection(VECTORS, "rewrap");
  const key = unhex(readTomlSection(VECTORS, "host_kek").str("key_hex"));
  const page = new Uint8Array(1024);
  page.set(unhex(kb.str("block_hex")), 0);
  const codec = unlockCodec(page, page, { key }, seededRandomSource(rw.big("seed")));
  const block = codec.rewrap({ key: unhex(rw.str("key_hex")) }, DEFAULT_ARGON2_PARAMS);
  assert.equal(bytesToHex(block), rw.str("block_hex"));
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
  const page = codec.keyBlockPage(1024);
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

test("a credential change writes the unselected copy first", () => {
  const rng = seededRandomSource(3n);
  const cred = { key: new Uint8Array(32).fill(9) };
  const codec = createCodec(cred, DEFAULT_ARGON2_PARAMS, rng);
  const old = codec.keyBlockPage(1024);
  const next = new Uint8Array(1024);
  next.set(codec.rewrap({ key: new Uint8Array(32).fill(5) }, DEFAULT_ARGON2_PARAMS), 0);
  assert.equal(unselectedKeyPage(old, old), 3);
  assert.equal(unselectedKeyPage(old, next), 2);
  assert.equal(unselectedKeyPage(next, old), 3);
  const torn = old.slice();
  torn[50]! ^= 1;
  assert.equal(unselectedKeyPage(torn, old), 2);
  assert.equal(unselectedKeyPage(old, torn), 3);
  // The newer copy is selected: the new credential unlocks, the old one no longer does.
  unlockCodec(old, next, { key: new Uint8Array(32).fill(5) }, rng);
  assert.equal(
    errCode(() => {
      unlockCodec(old, next, cred, rng);
    }),
    "28P01",
  );
});

// memoryReader streams bytes back as a spill run does (tests only).
function memoryReader(bytes: Uint8Array): SpillByteReader {
  let pos = 0;
  const take = (n: number): Uint8Array => {
    if (pos + n > bytes.length) throw new Error("unexpected EOF in spill run");
    pos += n;
    return bytes.slice(pos - n, pos);
  };
  return {
    byte: () => take(1)[0]!,
    bytes: take,
    u64: () => new DataView(take(8).buffer).getBigUint64(0, true),
    close: () => {},
  };
}

test("sealed scratch round-trips and detects replay", () => {
  const stores: MemoryBlockStore[] = [];
  const runs: Uint8Array[] = [];
  const inner: SpillSink = {
    writeRun(bytes) {
      runs.push(bytes);
      return { open: () => memoryReader(bytes) };
    },
    createScratchStore() {
      const store = new MemoryBlockStore();
      stores.push(store);
      return store;
    },
  };
  const sink = encryptingSpillSink(inner, seededRandomSource(5n));
  assert.ok(sink.encrypts?.());
  const utf8 = (t: string): Uint8Array => new TextEncoder().encode(t);
  const contains = (hay: Uint8Array, needle: string): boolean =>
    Buffer.from(hay).includes(Buffer.from(needle));
  const file = sink.createScratchStore!();
  let model = new Uint8Array(0);
  const write = (at: number, bytes: Uint8Array): Uint8Array => {
    file.writeAt(at, bytes);
    if (model.length < at + bytes.length) {
      const grown = new Uint8Array(at + bytes.length);
      grown.set(model, 0);
      model = grown;
    }
    model.set(bytes, at);
    return model.slice();
  };
  write(0, utf8("secret-scratch-".repeat(700)));
  write(5000, utf8("patched"));
  write(20000, utf8("far away"));
  const expect = write(4090, utf8("straddles a block boundary"));
  assert.equal(file.size(), expect.length);
  assert.deepEqual(file.readAt(0, file.size()), expect);
  // Past the logical size reads zeros.
  assert.deepEqual(file.readAt(expect.length, 4), new Uint8Array(4));
  // Writing another block seals the cached one; nothing is stored in plaintext.
  write(30000, utf8("x"));
  const stored = stores[0]!.readAt(0, stores[0]!.size());
  assert.ok(!contains(stored, "secret-scratch-"));

  // Replay: put back an older sealed version of block 1, which was rewritten since.
  const block1 = stored.slice(SCRATCH_STORED, 2 * SCRATCH_STORED);
  write(4100, utf8("newer"));
  write(30001, utf8("y"));
  assert.deepEqual(file.readAt(4100, 5), utf8("newer"));
  file.readAt(0, 1); // open another block, so block 1 is read from storage again
  stores[0]!.writeAt(SCRATCH_STORED, block1);
  assert.equal(
    errCode(() => {
      file.readAt(4100, 5);
    }),
    "58030",
  );

  // A block sealed before a truncation does not authenticate after the block is rewritten.
  const again = sink.createScratchStore!();
  const raw = stores[stores.length - 1]!;
  again.writeAt(0, new Uint8Array(4096).fill(1));
  again.writeAt(4096, new Uint8Array(4096).fill(2));
  again.writeAt(8192, new Uint8Array(1)); // seals block 1
  const sealed = raw.readAt(SCRATCH_STORED, SCRATCH_STORED);
  again.setSize(4096);
  again.writeAt(4096, new Uint8Array(4096).fill(3));
  again.writeAt(8192, new Uint8Array(1));
  raw.writeAt(SCRATCH_STORED, sealed);
  assert.equal(
    errCode(() => {
      again.readAt(4096, 5);
    }),
    "58030",
  );

  // Truncation zeroes the cut tail.
  const cut = sink.createScratchStore!();
  cut.writeAt(0, new Uint8Array(9000).fill(7));
  cut.setSize(4500);
  cut.writeAt(8999, Uint8Array.of(1));
  const got = cut.readAt(0, cut.size());
  assert.equal(got.length, 9000);
  assert.ok(got.subarray(0, 4500).every((b) => b === 7));
  assert.ok(got.subarray(4500, 8999).every((b) => b === 0));

  // A sort run is sealed block by block and streams back.
  const payload = utf8("secret-run-".repeat(1000));
  const reader = sink.writeRun(payload).open();
  assert.ok(!contains(runs[0]!, "secret-run-"));
  assert.equal(runs[0]!.length, Math.ceil(payload.length / 4096) * SCRATCH_STORED);
  assert.equal(reader.byte(), payload[0]);
  assert.deepEqual(reader.bytes(payload.length - 1), payload.subarray(1));
  assert.throws(() => reader.byte());
  reader.close();
});
