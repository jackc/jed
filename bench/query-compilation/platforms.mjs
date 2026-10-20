// Actual script execution under policy, not DevTools evaluate (which can bypass CSP).
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createContext, runInContext } from 'node:vm';

function probe() {
  const result = {};
  for (const [name, fn] of [
    ['eval', () => (0, eval)('1 + 2') === 3],
    ['Function', () => new Function('a', 'return a + 2')(1) === 3],
    ['wasm', () => { new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0])); return true; }],
  ]) {
    try { result[name] = fn() ? 'allowed' : 'wrong'; }
    catch (e) { result[name] = e.name; }
  }
  return result;
}

if (process.argv[2] !== 'browser') {
  const restricted = createContext({}, { codeGeneration: { strings: false, wasm: false } });
  const result = { node: process.version, execArgv: process.execArgv, normal: probe(),
    restrictedContext: runInContext(`(${probe.toString()})()`, restricted) };
  const blocked = process.execArgv.includes('--disallow-code-generation-from-strings');
  assert.equal(result.normal.Function, blocked ? 'EvalError' : 'allowed');
  assert.equal(result.restrictedContext.Function, 'EvalError');
  console.log(JSON.stringify(result));
} else {
  const { chromium } = await import(process.argv[3]);
  const policies = {
    none: null,
    strict: "script-src 'self'; worker-src 'self' blob:",
    eval: "script-src 'self' 'unsafe-eval'; worker-src 'self' blob:",
    wasm: "script-src 'self' 'wasm-unsafe-eval'; worker-src 'self' blob:",
    trusted: "script-src 'self' 'unsafe-eval'; require-trusted-types-for 'script'",
  };
  const workerSource = `postMessage((${probe.toString()})());`;
  const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    const policy = policies[url.searchParams.get('policy')];
    if (policy) res.setHeader('Content-Security-Policy', policy);
    res.setHeader('Content-Type', url.pathname.endsWith('.js') ? 'text/javascript' : 'text/html');
    if (url.pathname === '/worker.js') res.end(workerSource);
    else if (url.pathname === '/main.js') res.end(`
      const probe = ${probe.toString()};
      const result = {page: probe()};
      async function worker(url) {
        return await new Promise((resolve,reject) => {
          const w = new Worker(url);
          const timer = setTimeout(() => { w.terminate(); reject(Error('worker timeout')); }, 5000);
          w.onmessage = e => { clearTimeout(timer); w.terminate(); resolve(e.data); };
          w.onerror = e => { clearTimeout(timer); w.terminate(); reject(Error(e.message)); };
        });
      }
      (async () => {
        const policy = new URL(location.href).searchParams.get('policy');
        if (policy !== 'trusted') {
          result.worker = await worker('/worker.js?policy=' + policy);
          result.workerWithoutPolicy = await worker('/worker.js?policy=none');
          const blob = URL.createObjectURL(new Blob([${JSON.stringify(workerSource)}], {type:'text/javascript'}));
          try { result.blobWorker = await worker(blob); } finally { URL.revokeObjectURL(blob); }
        }
        window.result = result;
      })().catch(e => {window.failure = String(e)});
    `);
    else res.end('<!doctype html><script src="/main.js"></script>');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const rows = [];
    for (const policy of Object.keys(policies)) {
      const page = await browser.newPage();
      await page.goto(`http://127.0.0.1:${server.address().port}/?policy=${policy}`);
      await page.waitForFunction(() => window.result || window.failure);
      const result = await page.evaluate(() => window.result ?? { failure: window.failure });
      assert(!result.failure, result.failure);
      rows.push({ policy, ...result });
      await page.close();
    }
    for (const row of rows) {
      if (row.policy === 'trusted') assert(['TypeError', 'EvalError'].includes(row.page.Function));
      else assert.equal(row.page.Function, ['none', 'eval'].includes(row.policy) ? 'allowed' : 'EvalError');
      if (row.worker) {
        assert.equal(row.worker.Function, row.page.Function);
        assert.equal(row.blobWorker.Function, row.page.Function);
        assert.equal(row.workerWithoutPolicy.Function, 'allowed');
      }
    }
    console.log(JSON.stringify({ browser: browser.version(), rows }));
  } finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
}
