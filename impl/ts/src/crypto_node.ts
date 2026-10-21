// Node-only cryptography backend for the encrypted file variant (crypto.ts; CLAUDE.md §14 clause 3):
// AES-256, AES-256-GCM, HKDF-SHA-256 and the OS CSPRNG from node:crypto, all synchronous. Keep this
// module out of the browser/OPFS graph — it is imported where the Node file host is wired, as
// crc32_node.ts is.

import {
  type Cipheriv,
  createCipheriv,
  createDecipheriv,
  hkdfSync,
  randomFillSync,
} from "node:crypto";

import { type AesBlockCipher, type CryptoBackend, installCryptoBackend } from "./crypto.ts";

// plain returns a Buffer's bytes as a plain Uint8Array view, so the engine's Uint8Array.slice() keeps
// its copying meaning (Buffer.slice is a view).
function plain(b: Uint8Array): Uint8Array {
  return new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
}

function aes256(key: Uint8Array): AesBlockCipher {
  // ECB without padding is a raw block cipher: each update encrypts whole 16-byte blocks
  // independently, so one keyed cipher serves every later call (it is never finalized).
  const cipher: Cipheriv = createCipheriv("aes-256-ecb", key, null);
  cipher.setAutoPadding(false);
  return {
    encrypt(blocks: Uint8Array): Uint8Array {
      return plain(cipher.update(blocks));
    },
  };
}

const nodeCrypto: CryptoBackend = {
  aes256,
  gcmSeal(key, nonce, plaintext, aad) {
    const cipher = createCipheriv("aes-256-gcm", key, nonce, { authTagLength: 16 });
    cipher.setAAD(aad);
    const head = cipher.update(plaintext);
    const tail = cipher.final();
    const ciphertext = tail.length === 0 ? plain(head) : plain(Buffer.concat([head, tail]));
    return { ciphertext, tag: plain(cipher.getAuthTag()) };
  },
  gcmOpen(key, nonce, ciphertext, aad, tag) {
    const decipher = createDecipheriv("aes-256-gcm", key, nonce, { authTagLength: 16 });
    decipher.setAAD(aad);
    decipher.setAuthTag(tag);
    const head = decipher.update(ciphertext);
    try {
      const tail = decipher.final();
      return tail.length === 0 ? plain(head) : plain(Buffer.concat([head, tail]));
    } catch {
      return null;
    }
  },
  hkdfSha256(ikm, salt, info, length) {
    return new Uint8Array(hkdfSync("sha256", ikm, salt, info, length));
  },
  randomFill(buf) {
    randomFillSync(buf);
  },
};

installCryptoBackend("node:crypto", nodeCrypto);
