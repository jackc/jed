import { fileURLToPath } from "node:url";
import type { UserConfig } from "../../../impl/ts/node_modules/vite/dist/node/index.js";

// Both the direct engine and the production browser execution worker are bundled for the same
// browser target. Dependencies already belong to impl/ts; this benchmark installs nothing.
export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  worker: { format: "es" },
  build: {
    target: "esnext",
    minify: true,
    emptyOutDir: true,
    chunkSizeWarningLimit: 2000,
  },
} satisfies UserConfig;
