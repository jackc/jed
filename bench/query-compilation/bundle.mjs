// Reuse Vite's already-installed Rolldown; add no dependency. Prebuilding JS also
// avoids Node's WASM-based TS stripper, which cannot run under --jitless here.
import { rolldown } from '../../impl/ts/node_modules/rolldown/dist/index.mjs';
const bundle = await rolldown({ input: 'bench/query-compilation/kernels.mjs', platform: 'node' });
try { await bundle.write({ file: process.argv[2], format: 'esm', minify: false }); }
finally { await bundle.close(); }
