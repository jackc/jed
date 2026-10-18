// Public fixed benchmark inputs. This is not an engine codec or production nonce scheme.
import { gcm } from '../ts/node_modules/@noble/ciphers/aes.js';
import { xchacha20poly1305 } from '../ts/node_modules/@noble/ciphers/chacha.js';

const key = Uint8Array.from({length:32}, (_, i) => i);
const aad = Uint8Array.from(key, x => x + 0xa0);
const rows = [], fixtures = [];
let sink = 0;
const equal = (a, b) => a.length === b.length && a.every((v,i) => v === b[i]);
const check = (yes, msg) => { if (!yes) throw Error(msg); };
async function rejects(op) { let failed = false; try { await op(); } catch { failed = true; } check(failed, 'accepted invalid ciphertext'); }
const hex = bytes => Array.from(bytes, b => b.toString(16).padStart(2,'0')).join('');

export async function run(samples = 5, millis = 100) {
  const module = await WebAssembly.compile(await (await fetch(new URL('./crypto.wasm',import.meta.url))).arrayBuffer());
  // These WASI imports are unreachable in this crypto-only reactor; fail if any are called.
  // The full SQL wrapper needs a real host and is measured separately under node:wasi.
  const imports = {};
  for (const entry of WebAssembly.Module.imports(module)) {
    check(entry.kind === 'function', 'unexpected WASM import');
    (imports[entry.module] ??= {})[entry.name] = () => { throw Error(`Unexpected WASI call: ${entry.name}`); };
  }
  const wasm = (await WebAssembly.instantiate(module, imports)).exports;
  wasm._initialize?.();
  const cryptoKey = await crypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt','decrypt']);
  const wrongCryptoKey = await crypto.subtle.importKey('raw', key.map(x => x+1), 'AES-GCM', false, ['decrypt']);

  function wasmOps(algorithm, size, secret = key) {
    wasm.setup(algorithm === 'aes-256-gcm' ? 0 : 1, size, secret[0]);
    return {seal:(nonce,input,associated) => invoke(false,nonce,input,associated),
      open:(nonce,input,associated) => invoke(true,nonce,input,associated)};
  }
  function invoke(open, nonce, input, associated) {
    wasm.prepare(input.length);
    const memory = new Uint8Array(wasm.memory.buffer);
    memory.set(input, wasm.buffer_ptr()); memory.set(nonce, wasm.nonce_ptr()); memory.set(associated, wasm.aad_ptr());
    const len = wasm.crypt(open ? 1 : 0, input.length);
    if (!len) throw Error('authentication failure');
    return new Uint8Array(wasm.memory.buffer, wasm.buffer_ptr(), len).slice();
  }
  function nobleOps(algorithm, secret = key) {
    const factory = algorithm === 'aes-256-gcm' ? gcm : xchacha20poly1305;
    return {seal:(n,p,a) => factory(secret,n,a).encrypt(p), open:(n,p,a) => factory(secret,n,a).decrypt(p)};
  }
  const webOps = (secret = cryptoKey) => ({
    seal:async(n,p,a) => new Uint8Array(await crypto.subtle.encrypt({name:'AES-GCM',iv:n,additionalData:a,tagLength:128},secret,p)),
    open:async(n,p,a) => new Uint8Array(await crypto.subtle.decrypt({name:'AES-GCM',iv:n,additionalData:a,tagLength:128},secret,p)),
  });
  async function measure(label, size, operation, op, asynchronous, batch = 1) {
    const times = [];
    for (let sample = 0; sample <= samples; sample++) {
      const start = performance.now(); let count = 0;
      do {
        if (asynchronous) {
          if (batch === 1) sink ^= (await op())[0];
          else { const values = await Promise.all(Array.from({length:batch},op)); for (const v of values) sink ^= v[0]; }
          count += batch;
        } else { for (let i = 0; i < 64; i++) sink ^= op()[0]; count += 64; }
      } while (performance.now() - start < millis);
      if (sample) times.push((performance.now() - start) * 1e6 / count);
    }
    rows.push({backend:label,bytes:size,operation,batch,ns_samples:times,median_ns:[...times].sort((a,b)=>a-b)[Math.floor(times.length/2)]});
  }
  for (const size of [8192,65536]) {
    const plain = Uint8Array.from({length:size}, (_,i) => (i*31+7)&255);
    for (const algorithm of ['aes-256-gcm','xchacha20-poly1305']) {
      const nonce = new Uint8Array(algorithm === 'aes-256-gcm' ? 12 : 24);
      new DataView(nonce.buffer).setUint32(0,size);
      const reference = nobleOps(algorithm).seal(nonce,plain,aad);
      fixtures.push({algorithm,bytes:size,sha256:hex(new Uint8Array(await crypto.subtle.digest('SHA-256',reference)))});
      const backends = ['noble-js','rust-wasm',...(algorithm === 'aes-256-gcm' ? ['webcrypto','webcrypto-batch16'] : [])];
      for (const backend of backends) {
        const factory = secret => backend === 'rust-wasm' ? wasmOps(algorithm,size,secret)
          : backend.startsWith('webcrypto') ? webOps(secret === key ? cryptoKey : wrongCryptoKey) : nobleOps(algorithm,secret);
        let ops = factory(key);
        check(equal(await ops.seal(nonce,plain,aad),reference),'ciphertext mismatch');
        check(equal(await ops.open(nonce,reference,aad),plain),'round trip');
        for (const pos of [0,reference.length-1]) { const bad = reference.slice(); bad[pos]^=1; await rejects(()=>ops.open(nonce,bad,aad)); }
        const badAAD = aad.slice(); badAAD[0]^=1; await rejects(()=>ops.open(nonce,reference,badAAD));
        const badNonce = nonce.slice(); badNonce[0]^=1; await rejects(()=>ops.open(badNonce,reference,aad));
        const wrong = factory(key.map(x=>x+1)); await rejects(()=>wrong.open(nonce,reference,aad));
        ops = factory(key);
        let counter = 0;
        const seal = () => {
          const n = nonce.slice(); new DataView(n.buffer).setUint32(n.length-4,++counter);
          return ops.seal(n,plain,aad);
        };
        const open = () => ops.open(nonce,reference,aad);
        for (const [operation,op] of [['seal',seal],['open',open]])
          await measure(`${backend}/${algorithm}`,size,operation,op,backend.startsWith('webcrypto'),backend.endsWith('16')?16:1);
        // Verify after timed use, including after WASM memory has been mutated repeatedly.
        const finalNonce = nonce.slice(); new DataView(finalNonce.buffer).setUint32(finalNonce.length-4,++counter);
        check(equal(await ops.open(finalNonce,await ops.seal(finalNonce,plain,aad),aad),plain),'post-timing round trip');
      }
    }
  }
  const master = await crypto.subtle.importKey('raw',key,'HKDF',false,['deriveKey']);
  const info = new TextEncoder().encode('jed benchmark writer key, not a format');
  const salt = new Uint8Array(32);
  await measure('webcrypto',0,'random32+hkdf+aes-key',async() => {
    crypto.getRandomValues(salt);
    await crypto.subtle.deriveKey({name:'HKDF',hash:'SHA-256',salt,info},master,{name:'AES-GCM',length:256},false,['encrypt','decrypt']);
    return salt;
  },true);
  const randomNonce = new Uint8Array(24);
  await measure('browser-csprng',24,'random',()=>crypto.getRandomValues(randomNonce),false);
  return {rows,fixtures,sink,userAgent:navigator.userAgent,crossOriginIsolated,wasmImports:WebAssembly.Module.imports(module)};
}
