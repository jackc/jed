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

import { argon2id, argon2idAsync } from "@noble/hashes/argon2.js";

import type { BlockStore } from "./blockstore.ts";
import { NO_TAG, RESERVE, TAG_LEN, tagsEqual } from "./cap.ts";
import { metaChecksum, pageChecksum } from "./commit_manifest.ts";
import { crc32Ieee } from "./crc32.ts";
import { ioSteps, type Steps } from "./execution.ts";
import { engineError, type EngineError } from "./errors.ts";
import type { Environment } from "./environment.ts";
import type { RandomSource } from "./seam.ts";
import type { SpillByteReader, SpillRun, SpillScratch, SpillSink } from "./spill.ts";

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

// AsyncAesBlockCipher encrypts one 16-byte block under one AES-256 key (raw single-block AES).
export interface AsyncAesBlockCipher {
  encryptBlock(block: Uint8Array): Promise<Uint8Array>;
}

// AsyncCryptoBackend is the asynchronous primitive set (WebCrypto, crypto_web.ts): the same
// operations as CryptoBackend, awaited. It is what a browser has; the codec then seals and opens
// through the awaitable stages of the core (ts-runtime.md §11). Argon2id runs through @noble/hashes'
// asynchronous form. randomFill stays synchronous (crypto.getRandomValues).
export interface AsyncCryptoBackend {
  aes256(key: Uint8Array): Promise<AsyncAesBlockCipher>;
  gcmSeal(
    key: Uint8Array,
    nonce: Uint8Array,
    plaintext: Uint8Array,
    aad: Uint8Array,
  ): Promise<{ ciphertext: Uint8Array; tag: Uint8Array }>;
  gcmOpen(
    key: Uint8Array,
    nonce: Uint8Array,
    ciphertext: Uint8Array,
    aad: Uint8Array,
    tag: Uint8Array,
  ): Promise<Uint8Array | null>;
  hkdfSha256(
    ikm: Uint8Array,
    salt: Uint8Array,
    info: Uint8Array,
    length: number,
  ): Promise<Uint8Array>;
  randomFill(buf: Uint8Array): void;
}

let asyncBackend: AsyncCryptoBackend | null = null;
let preferAsync = false;

// installAsyncCryptoBackend selects the asynchronous primitives (the browser worker installs
// WebCrypto). A host with a synchronous backend keeps using it unless a test prefers the
// asynchronous one.
export function installAsyncCryptoBackend(selected: AsyncCryptoBackend): void {
  asyncBackend = selected;
}

// preferAsyncCryptoForTesting makes new codecs use the asynchronous backend even where a synchronous
// one is installed — the Node tests that check the WebCrypto provider byte for byte. Test-only.
export function preferAsyncCryptoForTesting(on: boolean): void {
  preferAsync = on;
}

// asyncCrypto reports whether new codecs are asynchronous: no synchronous backend (or a test prefers
// the asynchronous one) and an asynchronous backend installed.
export function asyncCrypto(): boolean {
  return (backend === null || preferAsync) && asyncBackend !== null;
}

function asyncCryptoBackend(): AsyncCryptoBackend {
  if (asyncBackend === null) {
    throw engineError(
      "feature_not_supported",
      "encrypted databases need a cryptography backend, which this host lacks",
    );
  }
  return asyncBackend;
}

// asyncCodecError is a synchronous use of an asynchronous codec: a path the asynchronous drivers
// should have awaited (ts-runtime.md §11).
function asyncCodecError(): EngineError {
  return engineError(
    "feature_not_supported",
    "this page codec is asynchronous; the operation must run as an awaited continuation",
  );
}

// cryptoAvailable reports whether this host can seal and open encrypted pages.
export function cryptoAvailable(): boolean {
  return backend !== null || asyncBackend !== null;
}

// osKeyEntropy is the production key-entropy source: the backend's OS CSPRNG (encrypted.md §8).
export function osKeyEntropy(): RandomSource {
  return {
    fill(buf: Uint8Array): void {
      if (backend !== null) backend.randomFill(buf);
      else asyncCryptoBackend().randomFill(buf);
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
    this.k1 = cmacSubkey(this.aes.encrypt(new Uint8Array(16)));
  }

  // derive is the per-nonce AES-256-GCM key Kx = AES_K(M1 ⊕ K1) ‖ AES_K(M2 ⊕ K1) with
  // Mi = 00 0i 58 00 ‖ N[0:12].
  private derive(nonce: Uint8Array): Uint8Array {
    return this.aes.encrypt(deriveBlocks(this.k1, nonce));
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

// cmacSubkey is K1 = L·x in GF(2^128), L = AES_K(0) (C2SP XAES-256-GCM).
function cmacSubkey(l: Uint8Array): Uint8Array {
  const k1 = new Uint8Array(16);
  for (let i = 0; i < 16; i++) k1[i] = ((l[i]! << 1) | (i < 15 ? l[i + 1]! >>> 7 : 0)) & 0xff;
  // Branch-free: XOR 0x87 into the last byte when the shifted-out bit was set.
  k1[15] ^= 0x87 & -(l[0]! >>> 7);
  return k1;
}

// deriveBlocks is M1 ⊕ K1 ‖ M2 ⊕ K1 with Mi = 00 0i 58 00 ‖ N[0:12]: the two blocks whose AES
// encryptions are the per-nonce key.
function deriveBlocks(k1: Uint8Array, nonce: Uint8Array): Uint8Array {
  const m = new Uint8Array(32);
  for (let half = 0; half < 2; half++) {
    const o = half * 16;
    m[o + 1] = half + 1;
    m[o + 2] = 0x58; // 'X'
    m.set(nonce.subarray(0, 12), o + 4);
    for (let i = 0; i < 16; i++) m[o + i] ^= k1[i]!;
  }
  return m;
}

// XaesKeyAsync is XaesKey over the asynchronous backend: the same derivation, each AES block and the
// AES-256-GCM seal awaited.
export class XaesKeyAsync {
  private readonly aes: AsyncAesBlockCipher;
  private readonly k1: Uint8Array;

  private constructor(aes: AsyncAesBlockCipher, k1: Uint8Array) {
    this.aes = aes;
    this.k1 = k1;
  }

  static async create(key: Uint8Array): Promise<XaesKeyAsync> {
    const aes = await asyncCryptoBackend().aes256(key);
    return new XaesKeyAsync(aes, cmacSubkey(await aes.encryptBlock(new Uint8Array(16))));
  }

  private async derive(nonce: Uint8Array): Promise<Uint8Array> {
    const m = deriveBlocks(this.k1, nonce);
    const [a, b] = await Promise.all([
      this.aes.encryptBlock(m.subarray(0, 16)),
      this.aes.encryptBlock(m.subarray(16, 32)),
    ]);
    const k = new Uint8Array(32);
    k.set(a!, 0);
    k.set(b!, 16);
    return k;
  }

  async seal(
    nonce: Uint8Array,
    plaintext: Uint8Array,
    aad: Uint8Array,
  ): Promise<{ ciphertext: Uint8Array; tag: Uint8Array }> {
    const key = await this.derive(nonce);
    return asyncCryptoBackend().gcmSeal(key, nonce.subarray(12, 24), plaintext, aad);
  }

  async open(
    nonce: Uint8Array,
    ciphertext: Uint8Array,
    aad: Uint8Array,
    tag: Uint8Array,
  ): Promise<Uint8Array | null> {
    const key = await this.derive(nonce);
    return asyncCryptoBackend().gcmOpen(key, nonce.subarray(12, 24), ciphertext, aad, tag);
  }
}

// hkdf is HKDF-SHA-256 to 32 bytes.
export function hkdf(ikm: Uint8Array, salt: Uint8Array, info: string): Uint8Array {
  return cryptoBackend().hkdfSha256(ikm, salt, UTF8.encode(info), 32);
}

// hkdfAsync is hkdf over the asynchronous backend.
export function hkdfAsync(ikm: Uint8Array, salt: Uint8Array, info: string): Promise<Uint8Array> {
  return asyncCryptoBackend().hkdfSha256(ikm, salt, UTF8.encode(info), 32);
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

// keyBlockGeneration is a key block's u64 big-endian generation (offset 8, encrypted.md §5).
function keyBlockGeneration(b: Uint8Array): bigint {
  return new DataView(b.buffer, b.byteOffset, b.byteLength).getBigUint64(8, false);
}

// ManifestChainPage is one manifest-overflow page as stored: what an encrypted meta's tag covers.
export type ManifestChainPage = { index: number; bytes: Uint8Array };

// CodecKeys are a codec's page and meta subkeys: synchronous (the Node backend) or asynchronous
// (WebCrypto, crypto_web.ts).
type CodecKeys =
  | { async: false; page: XaesKey; meta: XaesKey }
  | { async: true; page: XaesKeyAsync; meta: XaesKeyAsync };

// PageCodec is one open encrypted database's keys and nonce source: everything a commit needs to
// seal and a reader needs to open. Shared by the database's paging context.
//
// An asynchronous codec (over the WebCrypto backend) has no synchronous seal or open. Reads open
// pages through openBodySteps, an awaited effect of the continuation that reads them; the meta is
// sealed and verified the same way. Serialization stays synchronous and pure (format.ts), so a
// commit's body pages are sealed through a SealSession: each pass seals what it can from the
// session's results, the continuation awaits the rest, and the pass runs again — the nonce of each
// seal fixed by its position in the pass, so the bytes equal the synchronous codec's.
export class PageCodec {
  private readonly keys: CodecKeys;
  // The database's key-entropy stream (encrypted.md §8): one per database.
  private readonly rng: RandomSource;
  // The data key, kept so a credential change can rewrap it (encrypted.md §5).
  private readonly dek: Uint8Array;
  // The selected key-block copy (128 bytes), kept for compaction, which writes it to both pages of
  // the new file, and replaced by a credential change (encrypted.md §5).
  private block: Uint8Array;
  // session is the open seal session of an asynchronous codec's serialization pass (null otherwise).
  private session: SealSession | null = null;

  constructor(dek: Uint8Array, rng: RandomSource, keyBlock: Uint8Array, keys?: CodecKeys) {
    this.keys = keys ?? {
      async: false,
      page: new XaesKey(hkdf(dek, new Uint8Array(0), "jed/v1/page")),
      meta: new XaesKey(hkdf(dek, new Uint8Array(0), "jed/v1/meta")),
    };
    this.rng = rng;
    this.dek = dek.slice();
    this.block = keyBlock;
  }

  // createAsync is a codec over the asynchronous backend (its subkeys derived through it).
  static async createAsync(
    dek: Uint8Array,
    rng: RandomSource,
    keyBlock: Uint8Array,
  ): Promise<PageCodec> {
    const empty = new Uint8Array(0);
    const [page, meta] = await Promise.all([
      hkdfAsync(dek, empty, "jed/v1/page").then((k) => XaesKeyAsync.create(k)),
      hkdfAsync(dek, empty, "jed/v1/meta").then((k) => XaesKeyAsync.create(k)),
    ]);
    return new PageCodec(dek, rng, keyBlock, { async: true, page, meta });
  }

  // async reports whether the codec seals and opens only through awaited operations.
  get async(): boolean {
    return this.keys.async;
  }

  // dataKey is the data key, for a page-service owner's worker, which checks and reseals a client
  // commit's meta (spec/fileformat/encrypted.md §14). It never leaves the process.
  dataKey(): Uint8Array {
    return this.dek.slice();
  }

  // keyEntropy is the database's key-entropy stream: a rotation draws the new data key from it
  // (encrypted.md §12).
  keyEntropy(): RandomSource {
    return this.rng;
  }

  // keyBlock is the selected key-block copy (128 bytes; the rest of its page is zero).
  get keyBlock(): Uint8Array {
    return this.block;
  }

  // keyBlockPage is the selected copy as a whole key-block page of pageSize bytes.
  keyBlockPage(pageSize: number): Uint8Array {
    const page = new Uint8Array(pageSize);
    page.set(this.block, 0);
    return page;
  }

  // rewrap is the key block for a credential change (encrypted.md §5): the same DEK wrapped for cred
  // under a fresh KDF salt and wrap nonce, drawn in that order from the key-entropy stream, at the next
  // generation. Runs the KDF, so a caller derives it before taking the writer gate.
  rewrap(cred: Credential, params: Argon2Params): Uint8Array {
    checkCredential(cred);
    return wrapKeyBlock(this.dek, this.nextGeneration(), cred, params, this.rng);
  }

  // rewrapAsync is rewrap over the asynchronous backend.
  rewrapAsync(cred: Credential, params: Argon2Params): Promise<Uint8Array> {
    checkCredential(cred);
    return wrapKeyBlockAsync(this.dek, this.nextGeneration(), cred, params, this.rng);
  }

  private nextGeneration(): bigint {
    const generation = keyBlockGeneration(this.block);
    if (generation === 0xffff_ffff_ffff_ffffn) throw corrupt("key block generation exhausted");
    return generation + 1n;
  }

  // follows reports whether block is the next generation of the selected copy: a credential change
  // derived before the writer gate fails 55006 when another change won the race (encrypted.md §5).
  follows(block: Uint8Array): boolean {
    return keyBlockGeneration(block) === BigInt.asUintN(64, keyBlockGeneration(this.block) + 1n);
  }

  // setKeyBlock adopts block as the selected copy once a credential change has written both pages.
  setKeyBlock(block: Uint8Array): void {
    this.block = block;
  }

  private nonce(): Uint8Array {
    const n = new Uint8Array(NONCE_LEN);
    this.rng.fill(n);
    return n;
  }

  // beginSession opens a seal session for an asynchronous codec's serialization (see the class
  // comment); null for a synchronous codec, which seals inline.
  beginSession(): SealSession | null {
    if (!this.keys.async) return null;
    const session = new SealSession(this, this.rng);
    this.session = session;
    return session;
  }

  // endSession closes the open seal session.
  endSession(): void {
    this.session = null;
  }

  // sealBody seals body page index in place (encrypted.md §4): page is the full page, its first
  // pageSize − 40 bytes the plaintext (header with a zero CRC field, payload, zero fill). Draws one
  // nonce and returns the tag a pointer to the page binds (a fresh copy).
  sealBody(index: number, page: Uint8Array): Uint8Array {
    if (this.session !== null) return this.session.sealBody(index, page);
    const keys = this.keys;
    if (keys.async) throw asyncCodecError();
    const nonce = this.nonce();
    const split = page.length - RESERVE;
    const { ciphertext, tag } = keys.page.seal(
      nonce,
      page.subarray(0, split),
      adPrefix(ROLE_BODY, index),
    );
    page.set(ciphertext, 0);
    page.set(nonce, split);
    page.set(tag, split + NONCE_LEN);
    return tag.slice();
  }

  // sealBodyAsyncWith seals body page index in place under nonce (a seal session's resolution).
  async sealBodyAsyncWith(index: number, page: Uint8Array, nonce: Uint8Array): Promise<Uint8Array> {
    const keys = this.keys;
    if (!keys.async) throw new Error("sealBodyAsyncWith on a synchronous codec");
    const split = page.length - RESERVE;
    const { ciphertext, tag } = await keys.page.seal(
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
    const keys = this.keys;
    if (keys.async) throw asyncCodecError();
    const split = checkStored(stored, expected);
    const tag = stored.subarray(split + NONCE_LEN);
    const body = keys.page.open(
      stored.subarray(split, split + NONCE_LEN),
      stored.subarray(0, split),
      adPrefix(ROLE_BODY, index),
      tag,
    );
    return finishBody(body);
  }

  // openBodyAsync is openBody awaited (an asynchronous codec; a synchronous one answers at once).
  async openBodyAsync(
    index: number,
    stored: Uint8Array,
    expected: Uint8Array,
  ): Promise<Uint8Array> {
    const keys = this.keys;
    if (!keys.async) return this.openBody(index, stored, expected);
    const split = checkStored(stored, expected);
    const tag = stored.subarray(split + NONCE_LEN);
    const body = await keys.page.open(
      stored.subarray(split, split + NONCE_LEN),
      stored.subarray(0, split),
      adPrefix(ROLE_BODY, index),
      tag,
    );
    return finishBody(body);
  }

  // openBodySteps is openBody as a continuation step: inline for a synchronous codec, an awaited
  // effect for an asynchronous one.
  *openBodySteps(index: number, stored: Uint8Array, expected: Uint8Array): Steps<Uint8Array> {
    if (!this.keys.async) return this.openBody(index, stored, expected);
    return yield* ioSteps(
      () => this.openBody(index, stored, expected),
      () => this.openBodyAsync(index, stored, expected),
    );
  }

  // sealMeta seals meta slot slot in place (encrypted.md §6): every field and the inline manifest are
  // written; manifest is the commit's manifest-overflow chain in chain order. Writes the nonce and
  // tag into the reserve, then the meta CRC.
  sealMeta(slot: number, meta: Uint8Array, manifest: ManifestChainPage[]): void {
    if (this.session !== null) {
      this.session.sealMeta(slot, meta, manifest);
      return;
    }
    const keys = this.keys;
    if (keys.async) throw asyncCodecError();
    const nonce = this.nonce();
    zeroMeta(meta);
    const { tag } = keys.meta.seal(nonce, new Uint8Array(0), metaAad(slot, meta, manifest));
    finishMeta(meta, nonce, tag);
  }

  // sealMetaAsyncWith seals meta slot slot in place under nonce.
  async sealMetaAsyncWith(
    slot: number,
    meta: Uint8Array,
    manifest: ManifestChainPage[],
    nonce: Uint8Array,
  ): Promise<void> {
    const keys = this.keys;
    if (!keys.async) throw new Error("sealMetaAsyncWith on a synchronous codec");
    zeroMeta(meta);
    const { tag } = await keys.meta.seal(nonce, new Uint8Array(0), metaAad(slot, meta, manifest));
    finishMeta(meta, nonce, tag);
  }

  // sealMetaSteps is sealMeta as a continuation step (the commit's meta, sealed after every body
  // page): inline for a synchronous codec, an awaited effect drawing the next nonce for an
  // asynchronous one.
  *sealMetaSteps(slot: number, meta: Uint8Array, manifest: ManifestChainPage[]): Steps<void> {
    if (!this.keys.async) {
      this.sealMeta(slot, meta, manifest);
      return;
    }
    const nonce = this.nonce();
    yield* ioSteps(
      () => {
        throw asyncCodecError();
      },
      () => this.sealMetaAsyncWith(slot, meta, manifest, nonce),
    );
  }

  // verifyMeta checks the stored meta slot slot against its tag; manifest is its manifest-overflow
  // chain as stored. A failure is XX001 (never a fallback — encrypted.md §6).
  verifyMeta(slot: number, stored: Uint8Array, manifest: ManifestChainPage[]): void {
    const keys = this.keys;
    if (keys.async) throw asyncCodecError();
    const { nonce, tag, aad } = metaCheckInputs(slot, stored, manifest);
    if (keys.meta.open(nonce, new Uint8Array(0), aad, tag) === null) {
      throw corrupt("meta page fails authentication");
    }
  }

  // verifyMetaAsync is verifyMeta awaited.
  async verifyMetaAsync(
    slot: number,
    stored: Uint8Array,
    manifest: ManifestChainPage[],
  ): Promise<void> {
    const keys = this.keys;
    if (!keys.async) {
      this.verifyMeta(slot, stored, manifest);
      return;
    }
    const { nonce, tag, aad } = metaCheckInputs(slot, stored, manifest);
    if ((await keys.meta.open(nonce, new Uint8Array(0), aad, tag)) === null) {
      throw corrupt("meta page fails authentication");
    }
  }

  // verifyMetaSteps is verifyMeta as a continuation step.
  *verifyMetaSteps(slot: number, stored: Uint8Array, manifest: ManifestChainPage[]): Steps<void> {
    if (!this.keys.async) {
      this.verifyMeta(slot, stored, manifest);
      return;
    }
    yield* ioSteps(
      () => this.verifyMeta(slot, stored, manifest),
      () => this.verifyMetaAsync(slot, stored, manifest),
    );
  }
}

// checkStored checks a stored encrypted body page's size and its tag against the pointer's,
// returning the plaintext length.
function checkStored(stored: Uint8Array, expected: Uint8Array): number {
  if (stored.length < ENCRYPTED_MIN_PAGE_SIZE) throw corrupt("short encrypted page");
  const split = stored.length - RESERVE;
  if (!tagsEqual(stored.subarray(split + NONCE_LEN), expected)) {
    throw corrupt("page does not match the pointer that names it");
  }
  return split;
}

// finishBody checks an opened body page (null: authentication failed) and stamps its CRC.
function finishBody(body: Uint8Array | null): Uint8Array {
  if (body === null) throw corrupt("page fails authentication");
  if ((body[12]! | body[13]! | body[14]! | body[15]!) !== 0) {
    throw corrupt("encrypted page carries a CRC");
  }
  putU32(body, 12, pageChecksum(body));
  return body;
}

// zeroMeta zeroes a meta's CRC field and reserve, the bytes its tag does not cover.
function zeroMeta(meta: Uint8Array): void {
  meta.fill(0, 32, 36);
  meta.fill(0, meta.length - RESERVE);
}

// finishMeta writes a sealed meta's nonce and tag into the reserve, then its CRC.
function finishMeta(meta: Uint8Array, nonce: Uint8Array, tag: Uint8Array): void {
  const ps = meta.length;
  meta.set(nonce, ps - RESERVE);
  meta.set(tag, ps - TAG_LEN);
  putU32(meta, 32, metaChecksum(meta));
}

// metaCheckInputs are a stored meta's nonce, tag and AD.
function metaCheckInputs(
  slot: number,
  stored: Uint8Array,
  manifest: ManifestChainPage[],
): { nonce: Uint8Array; tag: Uint8Array; aad: Uint8Array } {
  const ps = stored.length;
  const nonce = stored.slice(ps - RESERVE, ps - TAG_LEN);
  const tag = stored.slice(ps - TAG_LEN);
  const zeroed = stored.slice();
  zeroMeta(zeroed);
  return { nonce, tag, aad: metaAad(slot, zeroed, manifest) };
}

// SealRequest is one seal a pass could not answer from its session: the pass position, what to seal,
// and its nonce.
type SealRequest =
  | { pos: number; kind: "body"; index: number; page: Uint8Array }
  | { pos: number; kind: "meta"; slot: number; meta: Uint8Array; manifest: ManifestChainPage[] };

// SealResult is a resolved seal: its input (compared byte for byte on the next pass) and output.
type SealResult = { input: Uint8Array; output: Uint8Array; tag: Uint8Array };

// SealSession is one serialization's seals under an asynchronous codec (see PageCodec). Seals are
// numbered by their position in a pass; position k always uses the k-th nonce the session drew, drawn
// in position order from the database's key-entropy stream, so the nonces — and the bytes — are the
// synchronous codec's. A pass seals a page from the session's results when its input matches the
// result at that position; otherwise it records a request and continues with a placeholder (an
// all-zero tag), and the pass is repeated after resolve awaited every request. A placeholder only
// ever reaches pages that are sealed again on a later pass, never storage.
export class SealSession {
  private readonly codec: PageCodec;
  private readonly rng: RandomSource;
  private readonly nonces: Uint8Array[] = [];
  private readonly results: (SealResult | undefined)[] = [];
  private requests: SealRequest[] = [];
  private pos = 0;
  // passes counts the passes begun (test tooling: a serialization's dependency depth).
  passes = 0;

  constructor(codec: PageCodec, rng: RandomSource) {
    this.codec = codec;
    this.rng = rng;
  }

  // beginPass starts a serialization pass.
  beginPass(): void {
    this.pos = 0;
    this.requests = [];
    this.passes++;
  }

  // pending reports whether the pass left seals to await.
  pending(): boolean {
    return this.requests.length > 0;
  }

  private nonceAt(k: number): Uint8Array {
    while (this.nonces.length <= k) {
      const n = new Uint8Array(NONCE_LEN);
      this.rng.fill(n);
      this.nonces.push(n);
    }
    return this.nonces[k]!;
  }

  sealBody(index: number, page: Uint8Array): Uint8Array {
    const k = this.pos++;
    this.nonceAt(k);
    const split = page.length - RESERVE;
    const input = new Uint8Array(4 + split);
    putU32(input, 0, index);
    input.set(page.subarray(0, split), 4);
    const r = this.results[k];
    if (r !== undefined && bytesEqual(r.input, input)) {
      page.set(r.output, 0);
      return r.tag.slice();
    }
    this.requests.push({ pos: k, kind: "body", index, page: page.slice() });
    return new Uint8Array(TAG_LEN);
  }

  sealMeta(slot: number, meta: Uint8Array, manifest: ManifestChainPage[]): void {
    const k = this.pos++;
    this.nonceAt(k);
    const zeroed = meta.slice();
    zeroMeta(zeroed);
    const aad = metaAad(slot, zeroed, manifest);
    const r = this.results[k];
    if (r !== undefined && bytesEqual(r.input, aad)) {
      meta.set(r.output, 0);
      return;
    }
    this.requests.push({
      pos: k,
      kind: "meta",
      slot,
      meta: meta.slice(),
      manifest: manifest.map((pg) => ({ index: pg.index, bytes: pg.bytes.slice() })),
    });
  }

  // resolve awaits every request the pass recorded, each under its position's nonce.
  async resolve(): Promise<void> {
    const requests = this.requests;
    this.requests = [];
    await Promise.all(
      requests.map(async (q) => {
        const nonce = this.nonceAt(q.pos);
        if (q.kind === "body") {
          const split = q.page.length - RESERVE;
          const input = new Uint8Array(4 + split);
          putU32(input, 0, q.index);
          input.set(q.page.subarray(0, split), 4);
          const tag = await this.codec.sealBodyAsyncWith(q.index, q.page, nonce);
          this.results[q.pos] = { input, output: q.page, tag };
        } else {
          const zeroed = q.meta.slice();
          zeroMeta(zeroed);
          const input = metaAad(q.slot, zeroed, q.manifest);
          await this.codec.sealMetaAsyncWith(q.slot, q.meta, q.manifest, nonce);
          this.results[q.pos] = { input, output: q.meta, tag: new Uint8Array(0) };
        }
      }),
    );
  }

  // resolveSteps is resolve as an awaited continuation effect.
  *resolveSteps(): Steps<void> {
    yield* ioSteps(
      () => {
        throw asyncCodecError();
      },
      () => this.resolve(),
    );
  }
}

// sealPassesSteps runs pass under codec's seal session until every seal is answered (the
// asynchronous codec; a synchronous or absent codec runs it once). before runs ahead of each repeated
// pass — the caller undoes the previous pass's page assignments there.
export function* sealPassesSteps<T>(
  codec: PageCodec | null,
  pass: () => T,
  before: (last: T) => void = () => {},
): Steps<T> {
  const session = codec?.beginSession() ?? null;
  if (session === null) return pass();
  try {
    for (;;) {
      session.beginPass();
      const out = pass();
      if (!session.pending()) return out;
      before(out);
      yield* session.resolveSteps();
    }
  } finally {
    codec!.endSession();
  }
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
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

// kekAsync is kek over the asynchronous backend (Argon2id through @noble/hashes' asynchronous form).
async function kekAsync(block: Uint8Array, cred: Credential): Promise<Uint8Array | null> {
  const salt = block.subarray(32, 64);
  if (block[7] === KDF_HOST_KEY && isHostKey(cred)) return hkdfAsync(cred.key, salt, "jed/v1/kek");
  if (block[7] === KDF_ARGON2ID && isPassphrase(cred)) {
    return argon2idAsync(UTF8.encode(cred.passphrase), salt, {
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
  return { block: wrapKeyBlock(dek, 1n, cred, params, rng), dek };
}

// wrapKeyBlock is the 128-byte key block wrapping dek for cred at generation: draws the KDF salt, then
// the wrap nonce, from rng (encrypted.md §5, §8).
function wrapKeyBlock(
  dek: Uint8Array,
  generation: bigint,
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Uint8Array {
  const block = keyBlockHead(generation, cred, params, rng);
  const key = kek(block, cred)!;
  const { ciphertext, tag } = cryptoBackend().gcmSeal(
    key,
    block.subarray(64, 76),
    dek,
    block.subarray(0, 76),
  );
  return finishKeyBlock(block, ciphertext, tag);
}

// wrapKeyBlockAsync is wrapKeyBlock over the asynchronous backend (the same draws, in the same order).
async function wrapKeyBlockAsync(
  dek: Uint8Array,
  generation: bigint,
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Promise<Uint8Array> {
  const block = keyBlockHead(generation, cred, params, rng);
  const key = (await kekAsync(block, cred))!;
  const { ciphertext, tag } = await asyncCryptoBackend().gcmSeal(
    key,
    block.subarray(64, 76),
    dek,
    block.subarray(0, 76),
  );
  return finishKeyBlock(block, ciphertext, tag);
}

// keyBlockHead is a new key block's first 76 bytes (the wrap's AD): magic, version, suite, KDF and its
// parameters, generation, and the KDF salt and wrap nonce, drawn in that order in one draw.
function keyBlockHead(
  generation: bigint,
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Uint8Array {
  const block = new Uint8Array(KEY_BLOCK_LEN);
  block.set(KEY_MAGIC, 0);
  block[4] = KEY_BLOCK_VERSION >>> 8;
  block[5] = KEY_BLOCK_VERSION & 0xff;
  block[6] = SUITE_XAES;
  new DataView(block.buffer).setBigUint64(8, generation, false);
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
  return block;
}

// finishKeyBlock writes the wrapped DEK, its tag and the CRC.
function finishKeyBlock(block: Uint8Array, ciphertext: Uint8Array, tag: Uint8Array): Uint8Array {
  block.set(ciphertext, 76);
  block.set(tag, 108);
  putU32(block, 124, crc32Ieee(block.subarray(0, 124)));
  return block;
}

// newKeyBlockAsync is newKeyBlock over the asynchronous backend.
export async function newKeyBlockAsync(
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Promise<{ block: Uint8Array; dek: Uint8Array }> {
  const dek = new Uint8Array(32);
  rng.fill(dek);
  return { block: await wrapKeyBlockAsync(dek, 1n, cred, params, rng), dek };
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

// unselectedKeyPage is the key-block page a credential change writes first (encrypted.md §5): the copy
// that is not the selected one — the older or invalid copy, page 3 when both are equal — so the only
// valid copy is never the first overwritten.
export function unselectedKeyPage(page2: Uint8Array, page3: Uint8Array): number {
  const a = parseKeyBlock(page2);
  const b = parseKeyBlock(page3);
  if (a !== null && b !== null && b.generation > a.generation) return 2;
  if (a === null && b !== null) return 2;
  return 3;
}

// selectedKeyPage is the whole key-block page holding the selected copy (encrypted.md §5): a backup
// writes it to both pages 2 and 3 (§13), and a promoting client adopts it (§14).
export function selectedKeyPage(page2: Uint8Array, page3: Uint8Array): Uint8Array {
  const block = selectKeyBlock(page2, page3);
  return (
    block.buffer === page3.buffer && block.byteOffset === page3.byteOffset ? page3 : page2
  ).slice();
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

// unwrapDekAsync is unwrapDek over the asynchronous backend.
async function unwrapDekAsync(block: Uint8Array, cred: Credential): Promise<Uint8Array> {
  const key = await kekAsync(block, cred);
  if (key === null) throw wrongCredential();
  const dek = await asyncCryptoBackend().gcmOpen(
    key,
    block.subarray(64, 76),
    block.subarray(76, 108),
    block.subarray(0, 76),
    block.subarray(108, 124),
  );
  if (dek === null) throw wrongCredential();
  return dek;
}

// createCodecAsync is createCodec over the asynchronous backend (an asynchronous codec).
export async function createCodecAsync(
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Promise<PageCodec> {
  checkCredential(cred);
  const { block, dek } = await newKeyBlockAsync(cred, params, rng);
  return PageCodec.createAsync(dek, rng, block);
}

// unlockCodecAsync is unlockCodec over the asynchronous backend (an asynchronous codec).
export async function unlockCodecAsync(
  page2: Uint8Array,
  page3: Uint8Array,
  cred: Credential,
  rng: RandomSource,
): Promise<PageCodec> {
  checkCredential(cred);
  const block = selectKeyBlock(page2, page3);
  const dek = await unwrapDekAsync(block, cred);
  return PageCodec.createAsync(dek, rng, block.slice());
}

// createCodecSteps / unlockCodecSteps pick the backend: a synchronous codec inline, an asynchronous one
// awaited (asyncCrypto).
export function* createCodecSteps(
  cred: Credential,
  params: Argon2Params,
  rng: RandomSource,
): Steps<PageCodec> {
  if (!asyncCrypto()) return createCodec(cred, params, rng);
  return yield* ioSteps(
    () => {
      throw asyncCodecError();
    },
    () => createCodecAsync(cred, params, rng),
  );
}

export function* unlockCodecSteps(
  page2: Uint8Array,
  page3: Uint8Array,
  cred: Credential,
  rng: RandomSource,
): Steps<PageCodec> {
  if (!asyncCrypto()) return unlockCodec(page2, page3, cred, rng);
  return yield* ioSteps(
    () => {
      throw asyncCodecError();
    },
    () => unlockCodecAsync(page2, page3, cred, rng),
  );
}

// createCodec is a new encrypted database's codec and key-block copy (create).
export function createCodec(cred: Credential, params: Argon2Params, rng: RandomSource): PageCodec {
  checkCredential(cred);
  const { block, dek } = newKeyBlock(cred, params, rng);
  return new PageCodec(dek, rng, block);
}

// scratchCodec is a throwaway codec for a rebuild's scratch domain (encrypted.md §12): a random key from
// the OS CSPRNG, never persisted. The scratch pages it seals are discarded, so they never touch a
// database's key-entropy stream.
export function scratchCodec(): PageCodec {
  const rng = osKeyEntropy();
  const key = new Uint8Array(32);
  rng.fill(key);
  return createCodec({ key }, DEFAULT_ARGON2_PARAMS, rng);
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

// ---- scratch encryption (spec/fileformat/encrypted.md §11) ----------------------------------------

// SCRATCH_BLOCK is the plaintext bytes of one sealed scratch block; each is stored as the block's
// ciphertext, its 24-byte nonce and its 16-byte tag.
export const SCRATCH_BLOCK = 4096;
export const SCRATCH_STORED = SCRATCH_BLOCK + RESERVE;
const ROLE_SCRATCH = 0x03;

// sealScratch is sink sealed when encrypt (scratch encryption, encrypted.md §11), with every file's key
// and id drawn from a scratch stream of environment's key entropy — a stream of its own, so it never
// perturbs a database's. A sink that already seals is returned as it is; so is a null sink (a host with
// no scratch target has nothing to seal).
export function sealScratch(
  sink: SpillSink | null,
  encrypt: boolean,
  environment: Environment,
): SpillSink | null {
  if (sink === null || !encrypt || (sink.encrypts?.() ?? false)) return sink;
  return encryptingSpillSink(sink, environment.keyStream());
}

// encryptingSpillSink is a spill sink whose every scratch file is sealed: each file gets a random key and
// id from rng, never persisted, and stores its bytes in sealed 4 KiB blocks whose associated data binds
// the file id, the block ordinal and the block's write counter, so a replayed older version of a
// rewritten block fails. Positional scratch and a spilled temp domain's store go through one sealed
// file over the inner sink's own; a sort run is sealed block by block into the inner sink's run (each
// block at write counter 1) and opened as it streams back. Results and cost do not change.
export function encryptingSpillSink(inner: SpillSink, rng: RandomSource): SpillSink {
  const sink: SpillSink = {
    encrypts: () => true,
    writeRun(bytes: Uint8Array): SpillRun {
      const file = new SealedScratchFile(nullRaw, rng);
      const blocks = Math.ceil(bytes.length / SCRATCH_BLOCK);
      const stored = new Uint8Array(blocks * SCRATCH_STORED);
      for (let i = 0; i < blocks; i++) {
        const block = new Uint8Array(SCRATCH_BLOCK);
        block.set(bytes.subarray(i * SCRATCH_BLOCK, (i + 1) * SCRATCH_BLOCK), 0);
        stored.set(file.seal(i, 1, block), i * SCRATCH_STORED);
      }
      const run = inner.writeRun(stored);
      return { open: () => new SealedRunReader(file, run.open(), bytes.length) };
    },
  };
  const createScratch = inner.createScratch?.bind(inner);
  if (createScratch !== undefined) {
    sink.createScratch = () => {
      const raw = createScratch();
      return new SealedSpillScratch(
        new SealedScratchFile(
          {
            read: (position, length) => raw.read(position, length),
            write: (position, bytes) => raw.write(position, bytes),
            // Spill scratch never shrinks; only a temp domain's store truncates.
            truncate: () => {},
            close: () => raw.close(),
          },
          rng,
        ),
      );
    };
  }
  const createScratchStore = inner.createScratchStore?.bind(inner);
  if (createScratchStore !== undefined) {
    sink.createScratchStore = () => {
      const store = createScratchStore();
      return new SealedScratchStore(
        new SealedScratchFile(
          {
            read: (position, length) => store.readAt(position, length),
            write: (position, bytes) => store.writeAt(position, bytes),
            truncate: (n) => store.setSize(n),
            close: () => store.close(),
          },
          rng,
        ),
      );
    };
  }
  return sink;
}

// RawScratch is the inner scratch file a sealed file stores its blocks in.
type RawScratch = {
  read(position: number, length: number): Uint8Array;
  write(position: number, bytes: Uint8Array): void;
  truncate(n: number): void;
  close(): void;
};

// nullRaw backs a sort run's sealer, which only seals and opens blocks and never stores them itself.
const nullRaw: RawScratch = {
  read: () => {
    throw engineError("io_error", "a sealed run has no positional backing");
  },
  write: () => {
    throw engineError("io_error", "a sealed run has no positional backing");
  },
  truncate: () => {},
  close: () => {},
};

function scratchAuthError(): EngineError {
  return engineError("io_error", "a scratch block fails authentication");
}

// SealedScratchFile is one sealed scratch file's logical bytes over raw: block i stored at i·4136, the
// write counters (never reset), which blocks are live, the logical size and the one-block plaintext
// write cache, all in memory.
class SealedScratchFile {
  private readonly raw: RawScratch;
  private readonly key: XaesKey;
  private readonly id: Uint8Array;
  private readonly rng: RandomSource;
  // size is the logical (plaintext) size.
  size = 0;
  // Each block's write counter, never reset (truncation included), so no sealed version of a block can
  // authenticate again once a newer one was sealed.
  private counters: number[] = [];
  // Whether each block holds written bytes; a block never written, or cut by a truncation, reads as
  // zeros.
  private live: boolean[] = [];
  // The block last written, kept in plaintext until another block is written, so a run of small
  // writes into one block seals it once.
  private cached: { ordinal: number; block: Uint8Array } | null = null;
  // The block last opened from storage at its write counter, so a run of small reads from one block
  // opens it once. In memory only; dropped when that block is stored again or the file truncates.
  private opened: { ordinal: number; counter: number; block: Uint8Array } | null = null;

  constructor(raw: RawScratch, rng: RandomSource) {
    this.raw = raw;
    this.rng = rng;
    const key = new Uint8Array(32);
    rng.fill(key);
    const id = new Uint8Array(16);
    rng.fill(id);
    this.key = new XaesKey(key);
    this.id = id;
  }

  // ad is the associated data of block ordinal at write counter counter.
  private ad(ordinal: number, counter: number): Uint8Array {
    const ad = new Uint8Array(42);
    ad.set(adPrefix(ROLE_SCRATCH, 0).subarray(0, 6), 0);
    ad.set(this.id, 6);
    const view = new DataView(ad.buffer);
    view.setBigUint64(22, BigInt(ordinal), false);
    view.setBigUint64(30, BigInt(counter), false);
    return ad;
  }

  // seal is block ordinal's stored form at write counter counter (ciphertext ‖ nonce ‖ tag).
  seal(ordinal: number, counter: number, block: Uint8Array): Uint8Array {
    const nonce = new Uint8Array(NONCE_LEN);
    this.rng.fill(nonce);
    const { ciphertext, tag } = this.key.seal(nonce, block, this.ad(ordinal, counter));
    const stored = new Uint8Array(SCRATCH_STORED);
    stored.set(ciphertext, 0);
    stored.set(nonce, SCRATCH_BLOCK);
    stored.set(tag, SCRATCH_BLOCK + NONCE_LEN);
    return stored;
  }

  // open is the plaintext of block ordinal's stored form at write counter counter; a failure is 58030.
  open(ordinal: number, counter: number, stored: Uint8Array): Uint8Array {
    if (stored.length !== SCRATCH_STORED) throw scratchAuthError();
    const plain = this.key.open(
      stored.subarray(SCRATCH_BLOCK, SCRATCH_BLOCK + NONCE_LEN),
      stored.subarray(0, SCRATCH_BLOCK),
      this.ad(ordinal, counter),
      stored.subarray(SCRATCH_BLOCK + NONCE_LEN),
    );
    if (plain === null) throw scratchAuthError();
    return plain;
  }

  // load is block ordinal's plaintext: the cached block, zeros for a block never written, else the
  // stored block opened against its counter. Never the cache's own buffer.
  private load(ordinal: number): Uint8Array {
    if (this.cached !== null && this.cached.ordinal === ordinal) return this.cached.block.slice();
    return this.view(ordinal).slice();
  }

  // view is block ordinal's plaintext as load, but possibly the read cache's own buffer: the caller
  // must not modify it.
  private view(ordinal: number): Uint8Array {
    if (this.cached !== null && this.cached.ordinal === ordinal) return this.cached.block;
    if (!(this.live[ordinal] ?? false)) return new Uint8Array(SCRATCH_BLOCK);
    const counter = this.counters[ordinal]!;
    const o = this.opened;
    if (o !== null && o.ordinal === ordinal && o.counter === counter) return o.block;
    const block = this.open(
      ordinal,
      counter,
      this.raw.read(ordinal * SCRATCH_STORED, SCRATCH_STORED),
    );
    this.opened = { ordinal, counter, block };
    return block;
  }

  // flushCached seals and stores the cached block, if any, at its next write counter.
  private flushCached(): void {
    const cached = this.cached;
    if (cached === null) return;
    this.cached = null;
    const { ordinal, block } = cached;
    while (this.counters.length <= ordinal) {
      this.counters.push(0);
      this.live.push(false);
    }
    const counter = this.counters[ordinal]! + 1;
    if (this.opened?.ordinal === ordinal) this.opened = null;
    this.raw.write(ordinal * SCRATCH_STORED, this.seal(ordinal, counter, block));
    this.counters[ordinal] = counter;
    this.live[ordinal] = true;
  }

  // read is the logical bytes [offset, offset + length), zero past the logical size.
  read(offset: number, length: number): Uint8Array {
    const out = new Uint8Array(length);
    const n = Math.max(0, Math.min(length, this.size - offset));
    let done = 0;
    while (done < n) {
      const at = offset + done;
      const ordinal = Math.floor(at / SCRATCH_BLOCK);
      const within = at % SCRATCH_BLOCK;
      const take = Math.min(SCRATCH_BLOCK - within, n - done);
      out.set(this.view(ordinal).subarray(within, within + take), done);
      done += take;
    }
    return out;
  }

  write(offset: number, bytes: Uint8Array): void {
    let done = 0;
    while (done < bytes.length) {
      const at = offset + done;
      const ordinal = Math.floor(at / SCRATCH_BLOCK);
      const within = at % SCRATCH_BLOCK;
      const take = Math.min(SCRATCH_BLOCK - within, bytes.length - done);
      if (this.cached === null || this.cached.ordinal !== ordinal) {
        const block = take === SCRATCH_BLOCK ? new Uint8Array(SCRATCH_BLOCK) : this.load(ordinal);
        this.flushCached();
        this.cached = { ordinal, block };
      }
      this.cached.block.set(bytes.subarray(done, done + take), within);
      done += take;
    }
    this.size = Math.max(this.size, offset + bytes.length);
  }

  // truncate sets the logical size to n; shrinking zeroes the cut part of a kept block and marks the
  // cut blocks not live, so growing again reads zeros past n. Their counters stay.
  truncate(n: number): void {
    const blocks = Math.ceil(n / SCRATCH_BLOCK);
    if (n < this.size) {
      this.opened = null;
      const within = n % SCRATCH_BLOCK;
      if (within !== 0) {
        const ordinal = Math.floor(n / SCRATCH_BLOCK);
        const block = this.load(ordinal);
        block.fill(0, within);
        if (this.cached === null || this.cached.ordinal !== ordinal) this.flushCached();
        this.cached = { ordinal, block };
      }
      if (this.cached !== null && this.cached.ordinal >= blocks) this.cached = null;
      for (let i = blocks; i < this.live.length; i++) this.live[i] = false;
      this.raw.truncate(blocks * SCRATCH_STORED);
    }
    this.size = n;
  }

  close(): void {
    this.cached = null;
    this.opened = null;
    this.raw.close();
  }
}

// SealedSpillScratch is positional spill scratch over a sealed file.
class SealedSpillScratch implements SpillScratch {
  private readonly file: SealedScratchFile;

  constructor(file: SealedScratchFile) {
    this.file = file;
  }

  get size(): number {
    return this.file.size;
  }

  read(position: number, length: number): Uint8Array {
    if (position + length > this.file.size) {
      throw engineError("io_error", "I/O error: unexpected EOF in spill scratch");
    }
    return this.file.read(position, length);
  }

  write(position: number, bytes: Uint8Array): void {
    this.file.write(position, bytes);
  }

  close(): void {
    this.file.close();
  }
}

// SealedScratchStore is a spilled temp domain's scratch BlockStore over a sealed file (never durable,
// never reopened: sync is a no-op).
class SealedScratchStore implements BlockStore {
  private readonly file: SealedScratchFile;

  constructor(file: SealedScratchFile) {
    this.file = file;
  }

  readAt(offset: number, len: number): Uint8Array {
    return this.file.read(offset, len);
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    this.file.write(offset, bytes);
  }

  sync(): void {
    // Never reopened, never durable: no barrier.
  }

  size(): number {
    return this.file.size;
  }

  setSize(bytes: number): void {
    this.file.truncate(bytes);
  }

  skipsSync(): boolean {
    return true;
  }

  close(): void {
    this.file.close();
  }
}

// SealedRunReader streams a sealed sort run back, opening one stored block at a time.
class SealedRunReader implements SpillByteReader {
  private readonly file: SealedScratchFile;
  private readonly inner: SpillByteReader;
  private readonly length: number;
  private block: Uint8Array = new Uint8Array(0);
  private ordinal = 0;
  private pos = 0;
  private consumed = 0;

  constructor(file: SealedScratchFile, inner: SpillByteReader, length: number) {
    this.file = file;
    this.inner = inner;
    this.length = length;
  }

  // refill opens the next stored block; past the run's logical end is EOF.
  private refill(): void {
    if (this.consumed >= this.length) throw new Error("unexpected EOF in spill run");
    this.block = this.file.open(this.ordinal, 1, this.inner.bytes(SCRATCH_STORED));
    this.ordinal++;
    this.pos = 0;
  }

  private available(): number {
    return Math.min(this.block.length - this.pos, this.length - this.consumed);
  }

  byte(): number {
    if (this.available() <= 0) this.refill();
    this.consumed++;
    return this.block[this.pos++]!;
  }

  bytes(n: number): Uint8Array {
    const out = new Uint8Array(n);
    let off = 0;
    while (off < n) {
      if (this.available() <= 0) this.refill();
      const take = Math.min(n - off, this.available());
      out.set(this.block.subarray(this.pos, this.pos + take), off);
      this.pos += take;
      this.consumed += take;
      off += take;
    }
    return out;
  }

  u64(): bigint {
    let n = 0n;
    for (let i = 0n; i < 8n; i++) n |= BigInt(this.byte()) << (i * 8n);
    return n;
  }

  close(): void {
    this.inner.close();
  }
}

export { NO_TAG };
