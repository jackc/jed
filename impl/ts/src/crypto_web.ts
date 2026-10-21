// The WebCrypto backend for the encrypted file variant (crypto.ts; spec/design/encryption.md §9): the
// asynchronous primitive set a browser worker has, and any host with globalThis.crypto.subtle. No
// primitive is written here (CLAUDE.md §14 clause 3):
//
//   - raw single-block AES, which XAES-256-GCM's CMAC subkey and key derivation need, is AES-CTR over
//     sixteen zero bytes with the input block as the 128-bit counter: the one keystream block is
//     AES_K(counter), so the ciphertext is exactly the block cipher's output (WebCrypto has no ECB);
//   - AES-256-GCM (iv 12 bytes, 128-bit tag, the additional data), HKDF-SHA-256 and the CSPRNG are
//     WebCrypto's own.
//
// Browser-clean: no `node:*` import. The browser worker installs it; Node tests install it to check it
// byte for byte against the node:crypto backend (crypto_node.ts).

import {
  type AsyncAesBlockCipher,
  type AsyncCryptoBackend,
  installAsyncCryptoBackend,
} from "./crypto.ts";

const ZERO_BLOCK = new Uint8Array(16);

// The WebCrypto types, from the host's global (the DOM lib in the browser, node's own under Node).
type Subtle = NonNullable<typeof globalThis.crypto>["subtle"];
type Key = Awaited<ReturnType<Subtle["importKey"]>>;

// subtle is the host's WebCrypto; absent, encryption stays 0A000.
function subtle(): Subtle {
  const s = globalThis.crypto?.subtle;
  if (s === undefined) throw new Error("WebCrypto is unavailable on this host");
  return s;
}

// buf copies bytes into a fresh ArrayBuffer-backed array, the form WebCrypto's BufferSource accepts
// across TypeScript's ArrayBufferLike distinction.
function buf(b: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(b.length);
  out.set(b);
  return out;
}

async function aes256(key: Uint8Array): Promise<AsyncAesBlockCipher> {
  const k = await subtle().importKey("raw", buf(key), { name: "AES-CTR" }, false, ["encrypt"]);
  return {
    async encryptBlock(block: Uint8Array): Promise<Uint8Array> {
      const out = await subtle().encrypt(
        { name: "AES-CTR", counter: buf(block), length: 128 },
        k,
        ZERO_BLOCK,
      );
      return new Uint8Array(out);
    },
  };
}

async function gcmKey(key: Uint8Array, use: "encrypt" | "decrypt"): Promise<Key> {
  return subtle().importKey("raw", buf(key), { name: "AES-GCM" }, false, [use]);
}

export const webCrypto: AsyncCryptoBackend = {
  aes256,
  async gcmSeal(key, nonce, plaintext, aad) {
    const out = new Uint8Array(
      await subtle().encrypt(
        { name: "AES-GCM", iv: buf(nonce), additionalData: buf(aad), tagLength: 128 },
        await gcmKey(key, "encrypt"),
        buf(plaintext),
      ),
    );
    const split = out.length - 16;
    return { ciphertext: out.slice(0, split), tag: out.slice(split) };
  },
  async gcmOpen(key, nonce, ciphertext, aad, tag) {
    const sealed = new Uint8Array(ciphertext.length + tag.length);
    sealed.set(ciphertext, 0);
    sealed.set(tag, ciphertext.length);
    try {
      return new Uint8Array(
        await subtle().decrypt(
          { name: "AES-GCM", iv: buf(nonce), additionalData: buf(aad), tagLength: 128 },
          await gcmKey(key, "decrypt"),
          sealed,
        ),
      );
    } catch {
      return null;
    }
  },
  async hkdfSha256(ikm, salt, info, length) {
    const k = await subtle().importKey("raw", buf(ikm), "HKDF", false, ["deriveBits"]);
    const bits = await subtle().deriveBits(
      { name: "HKDF", hash: "SHA-256", salt: buf(salt), info: buf(info) },
      k,
      length * 8,
    );
    return new Uint8Array(bits);
  },
  randomFill(b) {
    // getRandomValues fills at most 65536 bytes per call.
    for (let at = 0; at < b.length; at += 65536) {
      globalThis.crypto.getRandomValues(b.subarray(at, Math.min(at + 65536, b.length)));
    }
  },
};

// installWebCrypto installs the WebCrypto backend when the host has one.
export function installWebCrypto(): boolean {
  if (globalThis.crypto?.subtle === undefined) return false;
  installAsyncCryptoBackend(webCrypto);
  return true;
}
