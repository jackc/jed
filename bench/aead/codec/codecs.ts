// Measurement-only page codecs for the integrated browser experiment (README.md in this directory).
//
// Every codec here pays a real cryptographic cost per page and then returns the ORIGINAL plaintext
// bytes, so the database file, rows and costs stay byte-identical to a run without a codec. None is
// an encryption implementation: the keys, nonces and AAD are public benchmark constants and the
// nonce schemes are NEVER a production nonce policy (spec/design/encryption.md §3 is still open).
import { xchacha20poly1305 } from "../ts/node_modules/@noble/ciphers/chacha.js";
import type { PageTransform } from "../../../impl/ts/src/pager.ts";

export type VariantName =
  | "none"
  | "identity-async"
  | "aes-gcm-w16"
  | "aes-gcm-w4"
  | "aes-gcm-w1"
  | "aes-gcm-yield"
  | "xchacha-sync";

export const VARIANTS: VariantName[] = [
  "none",
  "identity-async",
  "aes-gcm-w16",
  "aes-gcm-w4",
  "aes-gcm-w1",
  "aes-gcm-yield",
  "xchacha-sync",
];

// Public benchmark key: bytes 00..1f, as in bench/aead. Not a secret, not a key policy.
const KEY = Uint8Array.from({ length: 32 }, (_, i) => i);
const TAG_BYTES = 16;

// Counts of real crypto operations, so a report can state pages sealed/opened per operation, and
// cryptoMs, the wall time from issuing each crypto call to holding its result (5 µs timer
// resolution, summed). Comparing it with an operation's delta against `none` separates the
// primitive's cost from everything else a codec adds.
export interface Counted {
  encoded: number;
  decoded: number;
  cryptoMs: number;
}

export type MeasurementCodec = (PageTransform & Counted) | null;

function fixturePlaintext(length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, i) => (31 * i + 7) & 0xff);
}

function pageAad(index: number): Uint8Array {
  const aad = new Uint8Array(8);
  new DataView(aad.buffer).setBigUint64(0, BigInt(index), false);
  return aad;
}

// Limit bounds this codec's in-flight crypto operations below the engine's fixed 16-page window
// (ts-runtime.md §11.3). It is how windows 1 and 4 are emulated without changing the engine: the
// engine still asks for up to 16 encodes at once, and the codec admits only `max` of them.
class Limit {
  private inflight = 0;
  private readonly waiters: (() => void)[] = [];
  constructor(private readonly max: number) {}
  // Null when a slot is free (no extra await); otherwise a promise resolved when one is handed over.
  acquire(): Promise<void> | null {
    if (this.inflight < this.max) {
      this.inflight++;
      return null;
    }
    return new Promise((resolve) => this.waiters.push(resolve));
  }
  release(): void {
    const next = this.waiters.shift();
    if (next) next();
    else this.inflight--;
  }
}

// Pure suspension: an async form that does no work. It isolates the cost of the asynchronous
// write-side mode (generators, promises, task yields) from the cost of cryptography.
class IdentityAsync implements PageTransform, Counted {
  encoded = 0;
  decoded = 0;
  cryptoMs = 0;
  async encodeAsync(_index: number, page: Uint8Array): Promise<Uint8Array> {
    this.encoded++;
    return page;
  }
  async decodeAsync(_index: number, stored: Uint8Array): Promise<Uint8Array> {
    this.decoded++;
    return stored;
  }
}

// AES-256-GCM through WebCrypto, async only (no synchronous form exists in the browser).
//
// encode seals the logical page with a 96-bit nonce made of a fixed "BENCH" prefix and a per-codec
// counter, AAD = big-endian page index, and checks the ciphertext+tag length. That is a fixed
// benchmark nonce sequence, never a production nonce policy (the derived-writer-key design of
// encryption.md §3 is not implemented; key derivation per commit is not measured).
//
// decode cannot open the stored bytes, which stay plaintext. It performs a full authenticated open of
// a page-sized fixture sealed at setup (same key, a fixed nonce and AAD), checks it, and returns the
// stored page unchanged. Authenticated-open cost depends on length, not content.
//
// yieldEvery > 0 makes every yieldEvery-th crypto operation first await a MessageChannel task turn
// (the production worker's scheduler). Chromium's dedicated-worker WebCrypto runs the operation
// inside the call and resolves without a task boundary, so without this a chain of awaited crypto
// never lets the worker's event loop admit a message (README.md, *Findings*).
class AesGcmMeasurement implements PageTransform, Counted {
  encoded = 0;
  decoded = 0;
  cryptoMs = 0;
  sink = 0;
  private counter = 0n;
  private readonly limit: Limit | null;
  private operations = 0;
  private constructor(
    private readonly key: CryptoKey,
    private readonly fixture: { nonce: Uint8Array; aad: Uint8Array; sealed: Uint8Array },
    private readonly pageSize: number,
    window: number,
    private readonly yieldEvery: number,
  ) {
    this.limit = window >= 16 ? null : new Limit(window);
  }

  private async turn(): Promise<void> {
    if (this.yieldEvery === 0 || ++this.operations % this.yieldEvery !== 0) return;
    await new Promise<void>((resolve) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = () => {
        channel.port1.close();
        resolve();
      };
      channel.port2.postMessage(null);
    });
  }

  static async create(pageSize: number, window: number, yieldEvery = 0): Promise<AesGcmMeasurement> {
    const key = await crypto.subtle.importKey("raw", KEY, "AES-GCM", false, ["encrypt", "decrypt"]);
    const nonce = Uint8Array.from([0x46, 0x49, 0x58, 0x54, 0, 0, 0, 0, 0, 0, 0, 0]); // "FIXT"
    const aad = pageAad(0xffffffff);
    const plain = fixturePlaintext(pageSize);
    const sealed = new Uint8Array(
      await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, plain),
    );
    // Setup validation, outside every timed region: round trip, then three rejections.
    const opened = new Uint8Array(
      await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, sealed),
    );
    if (opened.length !== plain.length || opened.some((b, i) => b !== plain[i]))
      throw new Error("AES-GCM fixture round trip failed");
    const tampered = sealed.slice();
    tampered[17] ^= 1;
    const tag = sealed.slice();
    tag[tag.length - 1] ^= 1;
    for (const [bytes, extra] of [
      [tampered, aad],
      [tag, aad],
      [sealed, pageAad(1)],
    ] as const) {
      let rejected = false;
      try {
        await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce, additionalData: extra }, key, bytes);
      } catch {
        rejected = true;
      }
      if (!rejected) throw new Error("AES-GCM accepted an altered fixture");
    }
    return new AesGcmMeasurement(key, { nonce, aad, sealed }, pageSize, window, yieldEvery);
  }

  private nextNonce(): Uint8Array {
    const nonce = Uint8Array.from([0x42, 0x45, 0x4e, 0x43, 0, 0, 0, 0, 0, 0, 0, 0]); // "BENC"
    new DataView(nonce.buffer).setBigUint64(4, this.counter++, false);
    return nonce;
  }

  async encodeAsync(index: number, page: Uint8Array): Promise<Uint8Array> {
    const wait = this.limit?.acquire();
    if (wait) await wait;
    try {
      await this.turn();
      const iv = this.nextNonce();
      const additionalData = pageAad(index);
      const started = performance.now();
      const sealed = new Uint8Array(
        await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData }, this.key, page),
      );
      this.cryptoMs += performance.now() - started;
      if (sealed.length !== page.length + TAG_BYTES) throw new Error("unexpected AES-GCM length");
      this.sink ^= sealed[sealed.length - 1]!;
      this.encoded++;
    } finally {
      this.limit?.release();
    }
    return page;
  }

  async decodeAsync(_index: number, stored: Uint8Array): Promise<Uint8Array> {
    if (stored.length !== this.pageSize) throw new Error("unexpected stored page length");
    const wait = this.limit?.acquire();
    if (wait) await wait;
    try {
      await this.turn();
      const f = this.fixture;
      const started = performance.now();
      const opened = await crypto.subtle.decrypt(
        { name: "AES-GCM", iv: f.nonce, additionalData: f.aad },
        this.key,
        f.sealed,
      );
      this.cryptoMs += performance.now() - started;
      if (opened.byteLength !== this.pageSize) throw new Error("unexpected AES-GCM open length");
      this.decoded++;
    } finally {
      this.limit?.release();
    }
    return stored;
  }
}

// XChaCha20-Poly1305 in pure JS (@noble/ciphers, the bench/aead-approved version), synchronous only:
// a synchronous transform keeps the pager's write side inline (ts-runtime.md §11.4). encode seals
// with a fresh 24-byte random nonce from crypto.getRandomValues (that suite's nonce cost); decode
// authenticates a page-sized fixture as above. Benchmark key; not a production nonce/key policy.
class XChaChaSyncMeasurement implements PageTransform, Counted {
  encoded = 0;
  decoded = 0;
  cryptoMs = 0;
  sink = 0;
  private readonly fixture: { nonce: Uint8Array; aad: Uint8Array; sealed: Uint8Array };
  constructor(private readonly pageSize: number) {
    const nonce = Uint8Array.from({ length: 24 }, (_, i) => 0x80 + i);
    const aad = pageAad(0xffffffff);
    const plain = fixturePlaintext(pageSize);
    const sealed = xchacha20poly1305(KEY, nonce, aad).encrypt(plain);
    const opened = xchacha20poly1305(KEY, nonce, aad).decrypt(sealed);
    if (opened.length !== plain.length || opened.some((b, i) => b !== plain[i]))
      throw new Error("XChaCha fixture round trip failed");
    const tampered = sealed.slice();
    tampered[17] ^= 1;
    let rejected = false;
    try {
      xchacha20poly1305(KEY, nonce, aad).decrypt(tampered);
    } catch {
      rejected = true;
    }
    if (!rejected) throw new Error("XChaCha accepted an altered fixture");
    this.fixture = { nonce, aad, sealed };
  }
  encode(index: number, page: Uint8Array): Uint8Array {
    const started = performance.now();
    const nonce = crypto.getRandomValues(new Uint8Array(24));
    const sealed = xchacha20poly1305(KEY, nonce, pageAad(index)).encrypt(page);
    this.cryptoMs += performance.now() - started;
    if (sealed.length !== page.length + TAG_BYTES) throw new Error("unexpected XChaCha length");
    this.sink ^= sealed[sealed.length - 1]!;
    this.encoded++;
    return page;
  }
  decode(_index: number, stored: Uint8Array): Uint8Array {
    if (stored.length !== this.pageSize) throw new Error("unexpected stored page length");
    const f = this.fixture;
    const started = performance.now();
    const opened = xchacha20poly1305(KEY, f.nonce, f.aad).decrypt(f.sealed);
    this.cryptoMs += performance.now() - started;
    if (opened.length !== this.pageSize) throw new Error("unexpected XChaCha open length");
    this.decoded++;
    return stored;
  }
}

export async function makeCodec(variant: VariantName, pageSize: number): Promise<MeasurementCodec> {
  switch (variant) {
    case "none":
      return null;
    case "identity-async":
      return new IdentityAsync();
    case "aes-gcm-w16":
      return AesGcmMeasurement.create(pageSize, 16);
    case "aes-gcm-w4":
      return AesGcmMeasurement.create(pageSize, 4);
    case "aes-gcm-w1":
      return AesGcmMeasurement.create(pageSize, 1);
    case "aes-gcm-yield":
      return AesGcmMeasurement.create(pageSize, 16, 16);
    case "xchacha-sync":
      return new XChaChaSyncMeasurement(pageSize);
  }
}
