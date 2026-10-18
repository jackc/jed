// File-backed Node comparison: independent TS, Rust/WASI, and native Rust/Node-API.
// Warm queries materialize rendered rows; setup excluded. No encryption or parallel queries.
import { createDatabase } from '../../../impl/ts/src/file.ts';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { render } from '../../../impl/ts/src/value.ts';
import { WASI } from 'node:wasi';
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import assert from 'node:assert/strict';
const dir = process.argv[2];
const { Database: NativeDatabase } = await import(pathToFileURL(resolve('impl/node/index.ts')).href);
const native = NativeDatabase.create(`${dir}/native.jed`);
const samples = Number(process.env.JED_AEAD_SAMPLES ?? 5), millis = Number(process.env.JED_AEAD_SAMPLE_MS ?? 100);
const bytes = readFileSync('impl/wasm/target/wasm32-wasip1/release/jed_wasm.wasm');
const wasi = new WASI({version:'preview1',args:[],env:{},preopens:{'/bench':dir}});
const instance = await WebAssembly.instantiate(bytes,wasi.getImportObject());
wasi.initialize(instance.instance);
const w = instance.instance.exports;
assert.equal(w.jed_abi_version(),2);
const decoder = new TextDecoder(), encoder = new TextEncoder();
function parse(ptr) {
  assert.ok(ptr);
  try {
    const v = new DataView(w.memory.buffer); let p = ptr+8; const tag = v.getUint8(p++);
    const text = () => {const n=v.getUint32(p,true);p+=4;const s=decoder.decode(new Uint8Array(w.memory.buffer,p,n));p+=n;return s;};
    if (tag===0) {const code=decoder.decode(new Uint8Array(w.memory.buffer,p,5));p+=5;throw Error(`${code}: ${text()}`);}
    if (tag===3) return Number(v.getBigUint64(p,true));
    if (tag===1 || tag===4) return [];
    assert.equal(tag,2);
    const cols=v.getUint32(p,true),count=v.getUint32(p+4,true);p+=8;
    return Array.from({length:count},()=>Array.from({length:cols},()=>v.getUint8(p++) ? null : text()));
  } finally {w.jed_free(ptr);}
}
let handle;
const pathBytes=encoder.encode('/bench/wasm.jed\0'),pathPtr=w.jed_alloc(pathBytes.length);
new Uint8Array(w.memory.buffer).set(pathBytes,pathPtr);
handle=parse(w.jed_create(pathPtr,3));
w.jed_dealloc(pathPtr,pathBytes.length);
function sqlCall(fn,sql) {
  const data=encoder.encode(sql+'\0'),ptr=w.jed_alloc(data.length);
  try {new Uint8Array(w.memory.buffer).set(data,ptr);return parse(fn(handle,ptr));}
  finally {w.jed_dealloc(ptr,data.length);}
}
// Each backend owns a different scratch file. WASI lacks OS locks; TS uses the same external-ownership assumption.
const db = createDatabase({path:`${dir}/ts.jed`,locking:'none'}), session=db.session();
const setup = ['CREATE TABLE t (id i64 PRIMARY KEY, v i64)', 'BEGIN',
  ...Array.from({length:10},(_,b)=>'INSERT INTO t VALUES '+Array.from({length:1000},(_,i)=>`(${b*1000+i},${(b*1000+i)%97})`).join(',')), 'COMMIT'];
for (const sql of setup) {session.execute(sql,[]);sqlCall(w.jed_execute,sql);native.execute(sql);}
const cases = {
  constant:'SELECT 1', point:'SELECT v FROM t WHERE id = 5000',
  aggregate:'SELECT sum(v) FROM t',
  sort:'SELECT id, v FROM t ORDER BY v DESC, id LIMIT 100',
  return1000:'SELECT id, v FROM t WHERE id < 1000 ORDER BY id',
};
const rows=[]; let sink=0;
for (const [name,sql] of Object.entries(cases)) {
  const tsStmt=session.prepareStatement(sql),wsStmt=sqlCall(w.jed_prepare,sql),nativeStmt=native.prepare(sql);
  const ts=()=>{const cursor=session.queryPrepared(tsStmt,[]);try{return Array.from(cursor,row=>row.map(v=>v.kind==='null'?null:render(v)));}finally{cursor.close();}};
  const wasm=()=>parse(w.jed_stmt_query(wsStmt,handle,0,0));
  const rustNative=()=>nativeStmt.query().map(row=>row.map(v=>v===null?null:String(v)));
  assert.deepEqual(ts(),wasm());assert.deepEqual(ts(),rustNative());
  for (const [backend,op] of [['typescript',ts],['rust-wasm',wasm],['rust-native',rustNative]]) {
    const times=[];
    for (let sample=0;sample<=samples;sample++) {
      const start=performance.now();let count=0;
      do {const out=op();sink^=out.length; if(out.length) sink^=String(out[0][0]).length;count++;} while(performance.now()-start<millis);
      if(sample) times.push((performance.now()-start)*1e6/count);
    }
    const median=[...times].sort((a,b)=>a-b)[Math.floor(times.length/2)];
    rows.push({name,sql,backend,ns_samples:times,median_ns:median});
    console.log(`${name} ${backend}: ${(median/1000).toFixed(3)} us`);
  }
  assert.deepEqual(ts(),wasm());assert.deepEqual(ts(),rustNative());w.jed_stmt_free(wsStmt);nativeStmt.close();
}
session.close();db.close();w.jed_close(handle);native.close();
const sourceFiles=['bench/aead/browser/node_compare.mjs','mise-tasks/bench/aead_node_compare','impl/node/index.ts','impl/node/src/lib.rs','impl/node/Cargo.toml','impl/node/Cargo.lock','impl/wasm/src/lib.rs','impl/wasm/Cargo.toml','impl/wasm/Cargo.lock'];
writeFileSync(`${dir}/results.json`,JSON.stringify({rows,sink,metadata:{date_utc:new Date().toISOString(),node:process.version,samples,sample_ms:millis,warmup_samples:1,cpu_affinity:process.env.JED_AEAD_CPU,
  revision:process.env.JED_BENCH_REVISION,bundle_sha256:createHash('sha256').update(readFileSync(process.argv[1])).digest('hex'),
  native_bytes:readFileSync('impl/node/jed_node.node').length,native_sha256:createHash('sha256').update(readFileSync('impl/node/jed_node.node')).digest('hex'),
  wasm_bytes:bytes.length,wasm_gzip_bytes:gzipSync(bytes).length,wasm_sha256:createHash('sha256').update(bytes).digest('hex'),
  source_sha256:Object.fromEntries(sourceFiles.map(p=>[p,createHash('sha256').update(readFileSync(p)).digest('hex')]))}},null,2)+'\n');
