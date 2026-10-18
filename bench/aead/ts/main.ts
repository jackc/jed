// Public fixed inputs for measurement only, never a production nonce scheme.
import assert from 'node:assert/strict';
import { createCipheriv, createDecipheriv, webcrypto } from 'node:crypto';
import { gcm } from '@noble/ciphers/aes.js';
import { chacha20poly1305 } from '@noble/ciphers/chacha.js';

const [backend, samplesText, millisText] = process.argv.slice(2);
const samples = Number(samplesText);
const millis = Number(millisText);
const key = Uint8Array.from({length:32}, (_, i) => i);
const wrongKey = Uint8Array.from(key, b => b + 1);
const aad = Uint8Array.from(key, b => b + 0xa0);
let sink = 0;
const emit = (value: unknown) => console.log(JSON.stringify(value));
type Op = (nonce: Uint8Array, input: Uint8Array, associated: Uint8Array) => Uint8Array;

function syncOps(algorithm: string, secret: Uint8Array): {seal: Op; open: Op} {
  if (backend === 'noble-js') {
    const factory = algorithm === 'aes-256-gcm' ? gcm : chacha20poly1305;
    return {
      seal: (nonce, input, associated) => factory(secret, nonce, associated).encrypt(input),
      open: (nonce, input, associated) => factory(secret, nonce, associated).decrypt(input),
    };
  }
  return {
    seal(nonce, input, associated) {
      const cipher = createCipheriv(algorithm, secret, nonce, {authTagLength:16});
      cipher.setAAD(associated);
      return Buffer.concat([cipher.update(input), cipher.final(), cipher.getAuthTag()]);
    },
    open(nonce, input, associated) {
      const cipher = createDecipheriv(algorithm, secret, nonce, {authTagLength:16});
      cipher.setAAD(associated);
      cipher.setAuthTag(input.subarray(input.length - 16));
      return Buffer.concat([cipher.update(input.subarray(0, -16)), cipher.final()]);
    },
  };
}

function measureSync(op: () => void): number[] {
  const values: number[] = [];
  for (let sample = 0; sample <= samples; sample++) {
    const start = process.hrtime.bigint();
    let count = 0;
    do {
      for (let i = 0; i < 64; i++) op();
      count += 64;
    } while (Number(process.hrtime.bigint() - start) / 1e6 < millis);
    const ns = Number(process.hrtime.bigint() - start) / count;
    if (sample > 0) values.push(ns);
  }
  return values;
}

async function measureAsync(op: () => Promise<void>): Promise<number[]> {
  const values: number[] = [];
  for (let sample = 0; sample <= samples; sample++) {
    const start = process.hrtime.bigint();
    let count = 0;
    do {
      for (let i = 0; i < 64; i++) await op();
      count += 64;
    } while (Number(process.hrtime.bigint() - start) / 1e6 < millis);
    const ns = Number(process.hrtime.bigint() - start) / count;
    if (sample > 0) values.push(ns);
  }
  return values;
}

for (const algorithm of backend === 'node-webcrypto' ? ['aes-256-gcm'] : ['aes-256-gcm', 'chacha20-poly1305']) {
  const cryptoKey = backend === 'node-webcrypto'
    ? await webcrypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt','decrypt']) : null;
  const wrongCryptoKey = backend === 'node-webcrypto'
    ? await webcrypto.subtle.importKey('raw', wrongKey, 'AES-GCM', false, ['decrypt']) : null;
  const ops = syncOps(algorithm, key);
  for (const size of [4096,8192,16384,65536]) {
    const plain = Uint8Array.from({length:size}, (_, i) => (i * 31 + 7) & 255);
    const nonce = new Uint8Array(12);
    const view = new DataView(nonce.buffer);
    view.setUint32(0, size);
    const parameters = (associated: Uint8Array) => ({name:'AES-GCM', iv:nonce, additionalData:associated, tagLength:128});
    const seal = async () => new Uint8Array(await webcrypto.subtle.encrypt(parameters(aad), cryptoKey!, plain));
    const open = async (input: Uint8Array, associated = aad, secret = cryptoKey) =>
      new Uint8Array(await webcrypto.subtle.decrypt(parameters(associated), secret!, input));
    const sealed = cryptoKey ? await seal() : ops.seal(nonce, plain, aad);
    assert.deepEqual(cryptoKey ? await open(sealed) : new Uint8Array(ops.open(nonce, sealed, aad)), plain);
    for (const index of [0,sealed.length - 1]) {
      const bad = Uint8Array.from(sealed); bad[index] ^= 1;
      if (cryptoKey) await assert.rejects(() => open(bad));
      else assert.throws(() => ops.open(nonce, bad, aad));
    }
    const badAAD = aad.slice(); badAAD[0] ^= 1;
    if (cryptoKey) {
      await assert.rejects(() => open(sealed, badAAD));
      await assert.rejects(() => open(sealed, aad, wrongCryptoKey));
    } else {
      assert.throws(() => ops.open(nonce, sealed, badAAD));
      assert.throws(() => syncOps(algorithm, wrongKey).open(nonce, sealed, aad));
    }
    emit({kind:'fixture', backend, algorithm, bytes:size, hex:Buffer.from(sealed).toString('hex')});
    if (samples === 0) continue;
    let counter = 0;
    let result: Uint8Array = new Uint8Array();
    for (const operation of ['seal','open']) {
      view.setBigUint64(4, 0n);
      function nextNonce() {
        counter++;
        view.setUint32(4, Math.floor(counter / 0x100000000));
        view.setUint32(8, counter >>> 0);
      }
      const times = cryptoKey ? await measureAsync(async () => {
        if (operation === 'seal') nextNonce();
        result = operation === 'seal' ? await seal() : await open(sealed);
        sink ^= result[0];
      }) : measureSync(() => {
        if (operation === 'seal') nextNonce();
        result = operation === 'seal' ? ops.seal(nonce, plain, aad) : ops.open(nonce, sealed, aad);
        sink ^= result[0];
      });
      const checked = operation === 'seal'
        ? cryptoKey ? await open(result) : ops.open(nonce, result, aad)
        : result;
      assert.deepEqual(new Uint8Array(checked), plain);
      emit({kind:'timing', backend, algorithm, bytes:size, mode:'allocating', operation, ns_samples:times});
    }
  }
}
// Observable use of each timed result without printing within the timed region.
assert(Number.isInteger(sink));
