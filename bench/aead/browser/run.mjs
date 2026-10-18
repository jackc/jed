import { createServer } from 'node:http';
import { readFileSync, writeFileSync, readdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
const [dir, playwrightPath] = process.argv.slice(2);
const { chromium } = await import(playwrightPath);
const samples = Number(process.env.JED_AEAD_SAMPLES ?? 5), millis = Number(process.env.JED_AEAD_SAMPLE_MS ?? 100);
if (!(samples > 0 && millis > 0)) throw Error('invalid sampling parameters');
const server = createServer((req,res) => {
  res.setHeader('Cross-Origin-Opener-Policy','same-origin');
  res.setHeader('Cross-Origin-Embedder-Policy','require-corp');
  const name = req.url === '/crypto.wasm' ? 'crypto.wasm' : req.url === '/probe.js' ? 'probe.js' : null;
  res.setHeader('Content-Type',name === 'crypto.wasm' ? 'application/wasm' : name ? 'text/javascript' : 'text/html');
  res.end(name ? readFileSync(join(dir,name)) : '<!doctype html><title>AEAD benchmark</title>');
});
await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
let browser;
try {
  browser = await chromium.launch({headless:true});
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const result = await page.evaluate(({samples,millis}) => new Promise((resolve,reject) => {
    const source = `import {run} from '${location.origin}/probe.js'; run(${samples},${millis}).then(result=>postMessage({result}),error=>postMessage({error:String(error.stack)}));`;
    const worker = new Worker(URL.createObjectURL(new Blob([source],{type:'text/javascript'})),{type:'module'});
    worker.onerror = e => { worker.terminate(); reject(Error(e.message)); };
    worker.onmessage = ({data}) => { worker.terminate(); data.error ? reject(Error(data.error)) : resolve(data.result); };
  }),{samples,millis});
  const sourceFiles = ['bench/aead/browser/probe.js','bench/aead/browser/run.mjs','bench/aead/rust/src/browser.rs','bench/aead/rust/Cargo.toml','bench/aead/rust/Cargo.lock','mise-tasks/bench/aead_browser'];
  result.metadata = {date_utc:new Date().toISOString(),samples,sample_ms:millis,warmup_samples:1,
    cpu_affinity:process.env.JED_AEAD_CPU,browser:browser.version(),node:process.version,
    wasm_build:{target:'wasm32-wasip1',lto:true,codegen_units:1,strip:'symbols',simd128:process.env.JED_AEAD_WASM_SIMD==='1',rustflags:process.env.RUSTFLAGS??''},
    revision:execFileSync('git',['rev-parse','HEAD'],{encoding:'utf8'}).trim(),
    rust:execFileSync('rustc',['--version'],{encoding:'utf8'}).trim(),
    cpu_info:readFileSync('/proc/cpuinfo','utf8').split('\n\n')[0],
    source_sha256:Object.fromEntries(sourceFiles.map(p=>[p,createHash('sha256').update(readFileSync(p)).digest('hex')])),
    artifacts:Object.fromEntries(['crypto.wasm','probe.js'].map(name=>{const b=readFileSync(join(dir,name));return [name,{bytes:b.length,gzip_bytes:gzipSync(b).length,sha256:createHash('sha256').update(b).digest('hex')}];})),
  };
  writeFileSync(join(dir,'results.json'),JSON.stringify(result,null,2)+'\n');
  for (const r of result.rows) console.log(`${r.backend} ${r.bytes} ${r.operation}: ${(r.median_ns/1000).toFixed(3)} us/page${r.batch>1?' (amortized batch)':''}`);
} finally { await browser?.close(); server.close(); }
