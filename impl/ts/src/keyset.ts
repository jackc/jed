// String-keyed hash sets and maps that stay linear on long keys.
//
// V8 hashes a string of 16384 or more UTF-16 units by its length alone (String::kMaxHashCalcLength),
// so a native Set or Map keyed by such strings puts every same-length key in one chain, and each probe
// compares full keys against all of them. A DISTINCT or GROUP BY over a 1 MiB text value turns
// quadratic. KeySet and KeyMap keep the native structure for keys of at most LONG_KEY units and
// bucket longer keys by a hash of their full content, comparing candidates in full. Membership is
// exactly string equality either way, so the change is physical only: no result, cost, or
// query-memory charge depends on it.

// Comfortably below V8's full-hash limit.
const LONG_KEY = 8192;

const encoder = new TextEncoder();
// Hashing reads a long key through UTF-8 chunks: encodeInto runs natively, and the word loop reads a
// quarter as many elements as a charCodeAt loop. A chunk of HASH_CHUNK units encodes to at most three
// bytes per unit.
const HASH_CHUNK = 16384;
const hashScratch = new Uint8Array(3 * HASH_CHUNK);
const hashWords = new Int32Array(hashScratch.buffer);

// keyHash hashes a key's full content. Distinct strings may collide (a lone surrogate encodes
// as U+FFFD like any other), which only lengthens a bucket; equal strings always hash alike. Four
// independent lanes keep the multiplies from forming one serial dependency chain.
export function keyHash(key: string): number {
  let a = key.length | 0;
  let b = 0x27d4eb2f;
  let c = 0x165667b1;
  let d = 0x61c88647;
  for (let i = 0; i < key.length; i += HASH_CHUNK) {
    const written = encoder.encodeInto(key.slice(i, i + HASH_CHUNK), hashScratch).written;
    const words = written >>> 2;
    const quads = words & ~3;
    let j = 0;
    for (; j < quads; j += 4) {
      a = Math.imul(a ^ hashWords[j]!, 0x9e3779b1);
      b = Math.imul(b ^ hashWords[j + 1]!, 0x85ebca77);
      c = Math.imul(c ^ hashWords[j + 2]!, 0xc2b2ae3d);
      d = Math.imul(d ^ hashWords[j + 3]!, 0x27d4eb2f);
    }
    for (; j < words; j++) a = Math.imul(a ^ hashWords[j]!, 0x9e3779b1);
    for (j = words << 2; j < written; j++) b = Math.imul(b ^ hashScratch[j]!, 0x01000193);
    a ^= a >>> 15;
    b ^= b >>> 13;
    c ^= c >>> 16;
    d ^= d >>> 15;
  }
  return Math.imul(a ^ Math.imul(b ^ Math.imul(c ^ d, 0x9e3779b1), 0x85ebca77), 0xc2b2ae3d);
}

// A key the native structure would hash poorly. A key starting with U+0000 goes the same way, so it
// can never equal a KeyMap surrogate.
function isLong(key: string): boolean {
  return key.length > LONG_KEY || key.charCodeAt(0) === 0;
}

// KeySet is a Set<string> of has/add/size/clear.
export class KeySet {
  private short = new Set<string>();
  private long = new Map<number, string[]>();
  private longCount = 0;

  get size(): number {
    return this.short.size + this.longCount;
  }
  has(key: string): boolean {
    if (!isLong(key)) return this.short.has(key);
    const bucket = this.long.get(keyHash(key));
    return bucket?.includes(key) === true;
  }
  add(key: string): this {
    if (!isLong(key)) {
      this.short.add(key);
      return this;
    }
    const hash = keyHash(key);
    const bucket = this.long.get(hash);
    if (bucket === undefined) this.long.set(hash, [key]);
    else if (!bucket.includes(key)) bucket.push(key);
    else return this;
    this.longCount++;
    return this;
  }
  clear(): void {
    this.short.clear();
    this.long.clear();
    this.longCount = 0;
  }
}

// KeyMap is a Map<string, V> of get/has/set/size/clear and insertion-ordered entries. A long key is
// stored in the native map under a short surrogate ("\0" + serial), resolved through its hash bucket,
// so iteration keeps one insertion order across short and long keys.
export class KeyMap<V> {
  private map = new Map<string, V>();
  private long = new Map<number, { key: string; surrogate: string }[]>();
  private raw = new Map<string, string>(); // surrogate → long key
  private serial = 0;

  get size(): number {
    return this.map.size;
  }
  private surrogate(key: string, hash: number): string | undefined {
    const bucket = this.long.get(hash);
    if (bucket !== undefined) for (const e of bucket) if (e.key === key) return e.surrogate;
    return undefined;
  }
  get(key: string): V | undefined {
    if (!isLong(key)) return this.map.get(key);
    const s = this.surrogate(key, keyHash(key));
    return s === undefined ? undefined : this.map.get(s);
  }
  has(key: string): boolean {
    if (!isLong(key)) return this.map.has(key);
    return this.surrogate(key, keyHash(key)) !== undefined;
  }
  set(key: string, value: V): this {
    if (!isLong(key)) {
      this.map.set(key, value);
      return this;
    }
    const hash = keyHash(key);
    let s = this.surrogate(key, hash);
    if (s === undefined) {
      s = "\0" + (this.serial++).toString();
      const bucket = this.long.get(hash);
      if (bucket === undefined) this.long.set(hash, [{ key, surrogate: s }]);
      else bucket.push({ key, surrogate: s });
      this.raw.set(s, key);
    }
    this.map.set(s, value);
    return this;
  }
  clear(): void {
    this.map.clear();
    this.long.clear();
    this.raw.clear();
  }
  *entries(): Generator<[string, V]> {
    for (const [k, v] of this.map) yield [k.charCodeAt(0) === 0 ? this.raw.get(k)! : k, v];
  }
  [Symbol.iterator](): Generator<[string, V]> {
    return this.entries();
  }
}
