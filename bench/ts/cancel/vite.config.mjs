import { fileURLToPath } from "node:url";
import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { assertSourcesUnchanged, sourceHashes } from "./metadata.mjs";
const root = resolve(process.env.JED_PROBE_ROOT ?? process.cwd());
const hashes = sourceHashes(root);
let output;
export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: { alias: { "@jed": root } },
  plugins: [
    {
      name: "probe-build-sources",
      configResolved(config) {
        output = config.build.outDir;
      },
      closeBundle() {
        assertSourcesUnchanged(root, hashes);
        writeFileSync(join(output, "probe-sources.json"), JSON.stringify(hashes, null, 2) + "\n");
      },
    },
  ],
  worker: {
    format: "es",
    plugins: () => [
      { name: "probe-source-root", config: () => ({ resolve: { alias: { "@jed": root } } }) },
    ],
  },
  build: { target: "esnext", minify: true, emptyOutDir: true, chunkSizeWarningLimit: 2000 },
};
