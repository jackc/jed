// Encryption at rest (spec/design/encryption.md; the bytes in spec/fileformat/encrypted.md): the
// XAES-256-GCM page codec, the subkeys, and the key block that wraps a file's data key.
//
// The codec sits in the core above the block seam (hosts.md §6): format.ts builds each page's
// plaintext exactly as the plaintext layout would (with payload capacity C = pageSize − 56) and hands
// it here to be sealed; a reader that follows a bound pointer hands the stored bytes and the pointer's
// tag back to be checked and opened. The hosts never see plaintext. (It is not the index-keyed
// PageTransform seam of pager.ts — opening a page needs the tag its pointer binds.)
//
// No cryptographic primitive is written here (CLAUDE.md §14 clause 3): AES-256, AES-256-GCM, HKDF and
// the key CSPRNG come from an installed synchronous CryptoBackend, Argon2id from the approved
// @noble/hashes. This module stays browser-clean: the Node host installs node:crypto from
// crypto_node.ts, as crc32_node.ts installs zlib's CRC; a host without a backend (the browser/OPFS
// worker) cannot create or open an encrypted database (0A000, encrypted.md §10). Only XAES-256-GCM's
// key derivation (https://c2sp.org/XAES-256-GCM — two AES blocks and a GF(2^128) doubling) is written
// here, over the backend's raw AES block, and checked against the C2SP vectors in
// spec/fileformat/encryption_vectors.toml.

import { argon2id } from "@noble/hashes/argon2.js";

import { NO_TAG, RESERVE, TAG_LEN, tagsEqual } from "./cap.ts";
import { metaChecksum, pageChecksum } from "./commit_manifest.ts";
import { crc32Ieee } from "./crc32.ts";
import { engineError, type EngineError } from "./errors.ts";
import type { RandomSource } from "./seam.ts";

// The encrypted variant's suite byte (meta offset 6, key block offset 6): XAES-256-GCM pages,
// HKDF-SHA-256 subkeys, an AES-256-GCM key wrap (encrypted.md §1).
export const SUITE_XAES = 1;
const NONCE_LEN = 24;
// The smallest page size an encrypted file may have (encrypted.md §1).
export const ENCRYPTED_MIN_PAGE_SIZE = 1024;

// ---- the backend seam -----------------------------------------------------------------------------

// AesBlockCipher encrypts 16-byte blocks under one AES-256 key (ECB, no padding): blocks.length is a
// multiple of 16 and each block is encrypted independently.
export interface AesBlockCipher {
  encrypt(blocks: Uint8Array): Uint8Array;
}

// CryptoBackend is the synchronous primitive set the codec needs, from a vetted library (CLAUDE.md
// §14 clause 3). The core never implements these itself.
export interface CryptoBackend {
  // aes256 keys a raw AES-256 block cipher (32-byte key).
  aes256(key: Uint8Array): AesBlockCipher;
  // gcmSeal encrypts plaintext under AES-256-GCM (32-byte key, 12-byte nonce, 16-byte tag): the
  // ciphertext (plaintext.length bytes) and the detached tag.
  gcmSeal(
    key: Uint8Array,
    nonce: Uint8Array,
    plaintext: Uint8Array,
    aad: Uint8Array,
  ): { ciphertext: Uint8Array; tag: Uint8Array };
  // gcmOpen decrypts and authenticates; null when the tag does not verify.
  gcmOpen(
    key: Uint8Array,
    nonce: Uint8Array,
    ciphertext: Uint8Array,
    aad: Uint8Array,
    tag: Uint8Array,
  ): Uint8Array | null;
  // hkdfSha256 is RFC 5869 HKDF over SHA-256 (an empty salt is HashLen zero bytes).
  hkdfSha256(ikm: Uint8Array, salt: Uint8Array, info: Uint8Array, length: number): Uint8Array;
  // randomFill fills buf from the OS CSPRNG — the production key-entropy source (encrypted.md §8).
  randomFill(buf: Uint8Array): void;
}

let backend: CryptoBackend | null = null;
let backendName = "";

// installCryptoBackend selects the codec's primitives (a host-initialization seam, like
// installCrc32Backend). A second, different backend is refused.
export function installCryptoBackend(name: string, selected: CryptoBackend): void {
  if (backend !== null && backend !== selected) {
    throw new Error(`crypto backend already selected (${backendName})`);
  }
  backend = selected;
  backendName = name;
}

export function selectedCryptoBackend(): string {
  return backendName;
}

// cryptoBackend is the installed backend; with none (a browser/OPFS host) encryption is 0A000.
function cryptoBackend(): CryptoBackend {
  if (backend === null) {
    throw engineError(
      "feature_not_supported",
      "encrypted databases need a synchronous cryptography backend, which this host lacks",
    );
  }
  return backend;
}

// cryptoAvailable reports whether this host can seal and open encrypted pages.
export function cryptoAvailable(): boolean {
  return backend !== null;
}

// osKeyEntropy is the production key-entropy source: the backend's OS CSPRNG (encrypted.md §8).
export function osKeyEntropy(): RandomSource {
  return {
    fill(buf: Uint8Array): void {
      cryptoBackend().randomFill(buf);
    },
  };
}

// ---- credentials ----------------------------------------------------------------------------------

// Credential unlocks an encrypted database: a passphrase (the KEK is Argon2id of its UTF-8 bytes) or
// a 32-byte key the host keeps elsewhere (the KEK is HKDF of it). It is an argument of
// create/open/attach, never a setting, and no error message ever carries it (encryption.md §5). A
// plain object so it crosses the async runtime's worker boundary unchanged.
export type Credential = { passphrase: string } | { key: Uint8Array };

// Argon2Params are the Argon2id parameters a passphrase credential is stretched with at create
// (encrypted.md §5). The default is RFC 9106's second recommendation; a reader checks the stored ones
// against the format's bounds before running the KDF.
export type Argon2Params = { memoryKib: number; iterations: number; parallelism: number };

export const DEFAULT_ARGON2_PARAMS: Argon2Params = {
  memoryKib: 65536,
  iterations: 3,
  parallelism: 4,
};

function argon2Valid(p: Argon2Params): boolean {
  const u32 = (x: number): boolean => Number.isInteger(x) && x >= 0 && x <= 0xffff_ffff;
  return (
    u32(p.memoryKib) &&
    u32(p.iterations) &&
    u32(p.parallelism) &&
    p.parallelism >= 1 &&
    p.parallelism <= 16 &&
    p.iterations >= 1 &&
    p.iterations <= 16 &&
    p.memoryKib >= 8 * p.parallelism &&
    p.memoryKib <= 1 << 20
  );
}

function corrupt(msg: string): EngineError {
  return engineError("data_corrupted", msg);
}

const UTF8 = new TextEncoder();

function u32At(b: Uint8Array, at: number): number {
  return ((b[at]! << 24) | (b[at + 1]! << 16) | (b[at + 2]! << 8) | b[at + 3]!) >>> 0;
}

function putU32(b: Uint8Array, at: number, v: number): void {
  b[at] = (v >>> 24) & 0xff;
  b[at + 1] = (v >>> 16) & 0xff;
  b[at + 2] = (v >>> 8) & 0xff;
  b[at + 3] = v & 0xff;
}

// ---- XAES-256-GCM (C2SP) --------------------------------------------------------------------------

// XaesKey is one XAES-256-GCM key: the AES key schedule and its CMAC subkey K1, both computed once
// (C2SP XAES-256-GCM: L = AES_K(0), K1 = L·x in GF(2^128)).
export class XaesKey {
  private readonly aes: AesBlockCipher;
  private readonly k1: Uint8Array;

  constructor(key: Uint8Array) {
    this.aes = cryptoBackend().aes256(key);
    const l = this.aes.encrypt(new Uint8Array(16));
    const k1 = new Uint8Array(16);
    for (let i = 0; i < 16; i++) k1[i] = ((l[i]! << 1) | (i < 15 ? l[i + 1]! >>> 7 : 0)) & 0xff;
    // Branch-free: XOR 0x87 into the last byte when the shifted-out bit was set.
    k1[15] ^= 0x87 & -(l[0]! >>> 7);
    this.k1 = k1;
  }

  // derive is the per-nonce AES-256-GCM key Kx = AES_K(M1 ⊕ K1) ‖ AES_K(M2 ⊕ K1) with
  // Mi = 00 0i 58 00 ‖ N[0:12].
  private derive(nonce: Uint8Array): Uint8Array {
    const m = new Uint8Array(32);
    for (let half = 0; half < 2; half++) {
      const o = half * 16;
      m[o + 1] = half + 1;
      m[o + 2] = 0x58; // 'X'
      m.set(nonce.subarray(0, 12), o + 4);
      for (let i = 0; i < 16; i++) m[o + i] ^= this.k1[i]!;
    }
    return this.aes.encrypt(m);
  }

  // seal encrypts plaintext under (nonce, aad): the ciphertext and the 16-byte tag.
  seal(
    nonce: Uint8Array,
    plaintext: Uint8Array,
    aad: Uint8Array,
  ): { ciphertext: Uint8Array; tag: Uint8Array } {
    return cryptoBackend().gcmSeal(this.derive(nonce), nonce.subarray(12, 24), plaintext, aad);
  }

  // open authenticates and decrypts; null when the tag does not verify.
  open(
    nonce: Uint8Array,
    ciphertext: Uint8Array,
    aad: Uint8Array,
    tag: Uint8Array,
  ): Uint8Array | null {
    return cryptoBackend().gcmOpen(
      this.derive(nonce),
      nonce.subarray(12, 24),
      ciphertext,
      aad,
      tag,
    );
  }
}

// hkdf is HKDF-SHA-256 to 32 bytes.
export function hkdf(ikm: Uint8Array, salt: Uint8Array, info: string): Uint8Array {
  return cryptoBackend().hkdfSha256(ikm, salt, UTF8.encode(info), 32);
}

const ROLE_BODY = 0x01;
const ROLE_META = 0x02;

// adPrefix is the 10-byte AD prefix of every seal: "JEDB" ‖ suite ‖ role ‖ u32 position
// (encrypted.md §3).
function adPrefix(role: number, position: number): Uint8Array {
  const ad = new Uint8Array(10);
  ad.set([0x4a, 0x45, 0x44, 0x42], 0);
  ad[4] = SUITE_XAES;
  ad[5] = role;
  putU32(ad, 6, position);
  return ad;
}

// ManifestChainPage is one manifest-overflow page as stored: what an encrypted meta's tag covers.
export type ManifestChainPage = { index: number; bytes: Uint8Array };

// PageCodec is one open encrypted database's keys and nonce source: everything a commit needs to
// seal and a reader needs to open. Shared by the database's paging context.
export class PageCodec {
  private readonly page: XaesKey;
  private readonly meta: XaesKey;
  // The database's key-entropy stream (encrypted.md §8): one per database.
  private readonly rng: RandomSource;
  // The selected key-block copy (128 bytes), kept for compaction, which writes it to both pages of
  // the new file (encrypted.md §5).
  readonly keyBlock: Uint8Array;

  constructor(dek: Uint8Array, rng: RandomSource, keyBlock: Uint8Array) {
    this.page = new XaesKey(hkdf(dek, new Uint8Array(0), "jed/v1/page"));
    this.meta = new XaesKey(hkdf(dek, new Uint8Array(0), "jed/v1/meta"));
    this.rng = rng;
    this.keyBlock = keyBlock;
  }

  private nonce(): Uint8Array {
    const n = new Uint8Array(NONCE_LEN);
    this.rng.fill(n);
    return n;
  }

  // sealBody seals body page index in place (encrypted.md §4): page is the full page, its first
  // pageSize − 40 bytes the plaintext (header with a zero CRC field, payload, zero fill). Draws one
  // nonce and returns the tag a pointer to the page binds (a fresh copy).
  sealBody(index: number, page: Uint8Array): Uint8Array {
    const nonce = this.nonce();
    const split = page.length - RESERVE;
    const { ciphertext, tag } = this.page.seal(
      nonce,
      page.subarray(0, split),
      adPrefix(ROLE_BODY, index),
    );
    page.set(ciphertext, 0);
    page.set(nonce, split);
    page.set(tag, split + NONCE_LEN);
    return tag.slice();
  }

  // openBody opens the stored body page index that a pointer binding expected names: check the
  // stored tag against the pointer's, then decrypt. Returns the plaintext page (pageSize − 40 bytes)
  // with its CRC field stamped as the plaintext layout's per-page CRC, so the ordinary page parse
  // accepts it. A mismatch or an authentication failure is XX001; so is a plaintext whose CRC field
  // is not zero.
  openBody(index: number, stored: Uint8Array, expected: Uint8Array): Uint8Array {
    if (stored.length < ENCRYPTED_MIN_PAGE_SIZE) throw corrupt("short encrypted page");
    const split = stored.length - RESERVE;
    const nonce = stored.subarray(split, split + NONCE_LEN);
    const tag = stored.subarray(split + NONCE_LEN);
    if (!tagsEqual(tag, expected)) throw corrupt("page does not match the pointer that names it");
    const body = this.page.open(nonce, stored.subarray(0, split), adPrefix(ROLE_BODY, index), tag);
    if (body === null) throw corrupt("page fails authentication");
    if ((body[12]! | body[13]! | body[14]! | body[15]!) !== 0) {
      throw corrupt("encrypted page carries a CRC");
    }
    putU32(body, 12, pageChecksum(body));
    return body;
  }

  // sealMeta seals meta slot slot in place (encrypted.md §6): every field and the inline manifest are
  // written; manifest is the commit's manifest-overflow chain in chain order. Writes the nonce and
  // tag into the reserve, then the meta CRC.
  sealMeta(slot: number, meta: Uint8Array, manifest: ManifestChainPage[]): void {
    const nonce = this.nonce();
    const ps = meta.length;
    meta.fill(0, 32, 36);
    meta.fill(0, ps - RESERVE);
    const { tag } = this.meta.seal(nonce, new Uint8Array(0), metaAad(slot, meta, manifest));
    meta.set(nonce, ps - RESERVE);
    meta.set(tag, ps - TAG_LEN);
    putU32(meta, 32, metaChecksum(meta));
  }

  // verifyMeta checks the stored meta slot slot against its tag; manifest is its manifest-overflow
  // chain as stored. A failure is XX001 (never a fallback — encrypted.md §6).
  verifyMeta(slot: number, stored: Uint8Array, manifest: ManifestChainPage[]): void {
    const ps = stored.length;
    const nonce = stored.slice(ps - RESERVE, ps - TAG_LEN);
    const tag = stored.slice(ps - TAG_LEN);
    const zeroed = stored.slice();
    zeroed.fill(0, 32, 36);
    zeroed.fill(0, ps - RESERVE);
    if (this.meta.open(nonce, new Uint8Array(0), metaAad(slot, zeroed, manifest), tag) === null) {
      throw corrupt("meta page fails authentication");
    }
  }
}

// metaAad is the meta AD: prefix ‖ the zeroed meta ‖ each manifest page's u32 id and stored bytes.
function metaAad(slot: number, zeroed: Uint8Array, manifest: ManifestChainPage[]): Uint8Array {
  let len = 10 + zeroed.length;
  for (const pg of manifest) len += 4 + pg.bytes.length;
  const aad = new Uint8Array(len);
  aad.set(adPrefix(ROLE_META, slot), 0);
  aad.set(zeroed, 10);
  let off = 10 + zeroed.length;
  for (const pg of manifest) {
    putU32(aad, off, pg.index);
    aad.set(pg.bytes, off + 4);
    off += 4 + pg.bytes.length;
  }
  return aad;
}

// ---- the key block (pages 2 and 3, encrypted.md §5) -----------------------------------------------

const KEY_MAGIC = [0x4a, 0x45, 0x44, 0x4b]; // "JEDK"
const KEY_BLOCK_VERSION = 1;
// The key block's encoded length; the rest of its page is zero.
export const KEY_BLOCK_LEN = 128;
const KDF_ARGON2ID = 1;
const KDF_HOST_KEY = 2;

function isPassphrase(c: Credential): c is { passphrase: string } {
  return "passphrase" in c && typeof c.passphrase === "string";
}

function isHostKey(c: Credential): c is { key: Uint8Array } {
  return "key" in c && c.key instanceof Uint8Array;
}

// checkCredential refuses a malformed credential before anything is read or written: a host key is
// exactly 32 bytes (22023).
export function checkCredential(c: Credential): void {
  if (isHostKey(c) && c.key.length === 32) return;
  if (isPassphrase(c)) return;
  throw engineError("invalid_parameter_value", "a credential is a passphrase or a 32-byte key");
}

// kek is the key-encryption key for one key-block copy (encrypted.md §3), or null when the credential
// is the wrong kind for the copy. The copy's parameters were bounds-checked by parseKeyBlock (or, at
// create, by newKeyBlock) before this runs.
function kek(block: Uint8Array, cred: Credential): Uint8Array | null {
  const salt = block.subarray(32, 64);
  if (block[7] === KDF_HOST_KEY && isHostKey(cred)) return hkdf(cred.key, salt, "jed/v1/kek");
  if (block[7] === KDF_ARGON2ID && isPassphrase(cred)) {
    return argon2id(UTF8.encode(cred.passphrase), salt, {
      m: u32At(block, 16),
      t: u32At(block, 20),
      p: u32At(block, 24),
      dkLen: 32,
      version: 0x13,
      maxmem: 1 << 30,
    });
  }
  return null;
}

function wrongCredential(): EngineError {
  return engineError("invalid_password", "the credential does not unlock this database");
}

// newKeyBlock builds a new key block for cred: draws the DEK, the KDF salt and the wrap nonce from rng,
// in that order (encrypted.md §8). Returns the 128-byte block and the DEK.
export function newKeyBlock(
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): { block: Uint8Array; dek: Uint8Array } {
  const dek = new Uint8Array(32);
  rng.fill(dek);
  const block = new Uint8Array(KEY_BLOCK_LEN);
  block.set(KEY_MAGIC, 0);
  block[4] = KEY_BLOCK_VERSION >>> 8;
  block[5] = KEY_BLOCK_VERSION & 0xff;
  block[6] = SUITE_XAES;
  block[15] = 1; // generation 1 (u64 big-endian)
  if (isPassphrase(cred)) {
    if (!argon2Valid(params)) {
      throw engineError("invalid_parameter_value", "Argon2id parameters are out of range");
    }
    block[7] = KDF_ARGON2ID;
    putU32(block, 16, params.memoryKib);
    putU32(block, 20, params.iterations);
    putU32(block, 24, params.parallelism);
  } else {
    block[7] = KDF_HOST_KEY;
  }
  // kdf_salt ‖ wrap_nonce in one draw (32 + 12 bytes).
  const fresh = new Uint8Array(44);
  rng.fill(fresh);
  block.set(fresh, 32);
  const key = kek(block, cred)!;
  const { ciphertext, tag } = cryptoBackend().gcmSeal(
    key,
    block.subarray(64, 76),
    dek,
    block.subarray(0, 76),
  );
  block.set(ciphertext, 76);
  block.set(tag, 108);
  putU32(block, 124, crc32Ieee(block.subarray(0, 124)));
  return { block, dek };
}

// parseKeyBlock validates one key-block copy (encrypted.md §5): its block and generation, or null
// when it is not a valid copy.
function parseKeyBlock(page: Uint8Array): { block: Uint8Array; generation: bigint } | null {
  if (page.length < KEY_BLOCK_LEN) return null;
  const b = page.subarray(0, KEY_BLOCK_LEN);
  const generation = new DataView(b.buffer, b.byteOffset, b.byteLength).getBigUint64(8, false);
  let kdfOk = false;
  if (b[7] === KDF_ARGON2ID) {
    kdfOk = argon2Valid({
      memoryKib: u32At(b, 16),
      iterations: u32At(b, 20),
      parallelism: u32At(b, 24),
    });
  } else if (b[7] === KDF_HOST_KEY) {
    kdfOk = u32At(b, 16) === 0 && u32At(b, 20) === 0 && u32At(b, 24) === 0;
  }
  let tailZero = true;
  for (let i = KEY_BLOCK_LEN; i < page.length; i++) {
    if (page[i] !== 0) {
      tailZero = false;
      break;
    }
  }
  const valid =
    b[0] === KEY_MAGIC[0] &&
    b[1] === KEY_MAGIC[1] &&
    b[2] === KEY_MAGIC[2] &&
    b[3] === KEY_MAGIC[3] &&
    ((b[4]! << 8) | b[5]!) === KEY_BLOCK_VERSION &&
    b[6] === SUITE_XAES &&
    generation >= 1n &&
    kdfOk &&
    u32At(b, 28) === 0 &&
    u32At(b, 124) === crc32Ieee(b.subarray(0, 124)) &&
    tailZero;
  return valid ? { block: b, generation } : null;
}

// selectKeyBlock selects the key-block copy to unwrap (encrypted.md §5): the valid copy with the
// greater generation; two valid copies of one generation must be identical; no valid copy is XX001.
function selectKeyBlock(page2: Uint8Array, page3: Uint8Array): Uint8Array {
  const a = parseKeyBlock(page2);
  const b = parseKeyBlock(page3);
  if (a === null && b === null) throw corrupt("no valid key block");
  if (a === null) return b!.block;
  if (b === null) return a.block;
  if (a.generation === b.generation) {
    for (let i = 0; i < KEY_BLOCK_LEN; i++) {
      if (a.block[i] !== b.block[i]) throw corrupt("key block copies disagree");
    }
  }
  return b.generation > a.generation ? b.block : a.block;
}

// unwrapDek unwraps the selected copy's DEK with cred: 28P01 when it does not unwrap.
function unwrapDek(block: Uint8Array, cred: Credential): Uint8Array {
  const key = kek(block, cred);
  if (key === null) throw wrongCredential();
  const dek = cryptoBackend().gcmOpen(
    key,
    block.subarray(64, 76),
    block.subarray(76, 108),
    block.subarray(0, 76),
    block.subarray(108, 124),
  );
  if (dek === null) throw wrongCredential();
  return dek;
}

// createCodec is a new encrypted database's codec and key-block copy (create).
export function createCodec(cred: Credential, params: Argon2Params, rng: RandomSource): PageCodec {
  checkCredential(cred);
  const { block, dek } = newKeyBlock(cred, params, rng);
  return new PageCodec(dek, rng, block);
}

// unlockCodec unlocks an encrypted file from its key-block pages: select the copy, unwrap its DEK
// with cred.
export function unlockCodec(
  page2: Uint8Array,
  page3: Uint8Array,
  cred: Credential,
  rng: RandomSource,
): PageCodec {
  checkCredential(cred);
  const block = selectKeyBlock(page2, page3);
  const dek = unwrapDek(block, cred);
  return new PageCodec(dek, rng, block.slice());
}

// fileSuite is the suite a file's meta slots declare (offset 6, with the reserve byte at 7 —
// encrypted.md §6), read from the CRC-valid slots before any key is available: 0 plaintext,
// SUITE_XAES encrypted. Slots that disagree, or an unknown suite, are XX001.
export function fileSuite(slot0: Uint8Array, slot1: Uint8Array): number {
  let suite: number | null = null;
  for (const m of [slot0, slot1]) {
    if (m.length < 64 || u32At(m, 32) !== metaChecksum(m)) continue;
    let s: number;
    if (m[6] === 0 && m[7] === 0) s = 0;
    else if (m[6] === SUITE_XAES && m[7] === RESERVE) s = SUITE_XAES;
    else throw corrupt("unknown encryption suite");
    if (suite !== null && suite !== s) throw corrupt("meta slots disagree on encryption");
    suite = s;
  }
  // Neither slot is valid: slot selection reports the corruption.
  return suite ?? 0;
}

export { NO_TAG };
