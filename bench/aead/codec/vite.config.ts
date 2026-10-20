import { fileURLToPath } from "node:url";
import type { UserConfig } from "../../../impl/ts/node_modules/vite/dist/node/index.js";

// The page driver, the engine worker and the measurement codecs are bundled for one browser target.
// Tooling comes from impl/ts and @noble/ciphers from bench/aead/ts; this benchmark installs nothing.
export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  worker: { format: "es" },
  build: {
    target: "esnext",
    minify: true,
    emptyOutDir: true,
    chunkSizeWarningLimit: 4000,
  },
} satisfies UserConfig;
