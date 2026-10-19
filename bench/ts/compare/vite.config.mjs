import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

if (!process.env.JED_COMPARE_ROOT) throw new Error("JED_COMPARE_ROOT is required");
const root = resolve(process.env.JED_COMPARE_ROOT);
const mode = process.env.JED_COMPARE_MODE;
if (mode !== "direct" && mode !== "worker") throw new Error("Invalid JED_COMPARE_MODE");
const adapterId = "\0compare-adapter";
// The selected mode has a different dependency tree at build time: master's direct bundle
// never needs (or imports) the async runtime added by the branch being compared.
const adapter = mode === "direct"
  ? `import { buildInMemory } from ${JSON.stringify(`${root}/impl/ts/src/shared.ts`)};
     export const comparisonRuntimeLimits = null;
     export function createComparisonDatabase() { return buildInMemory(8192); }`
  : `import { createDatabase } from ${JSON.stringify(`${root}/impl/ts/src/browser/async.ts`)};
     import { runtimeLimits } from ${JSON.stringify(`${root}/impl/ts/src/async/protocol.ts`)};
     export const comparisonRuntimeLimits = runtimeLimits();
     export function createComparisonDatabase() { return createDatabase({ pageSize: 8192 }); }`;

export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  define: { __COMPARE_MODE__: JSON.stringify(mode) },
  plugins: [{
    name: "comparison-source-snapshot",
    resolveId(id) { if (id === "@compare-adapter") return adapterId; },
    load(id) { if (id === adapterId) return adapter; },
  }],
  worker: { format: "es" },
  build: {
    target: "esnext",
    minify: true,
    emptyOutDir: true,
    chunkSizeWarningLimit: 2000,
  },
};
