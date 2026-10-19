import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
const root = resolve(process.env.JED_PROFILE_ROOT);
export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: { alias: { "@jed": root } },
  worker: { format: "es", plugins: () => [{ name: "profile-source-root",
    config: () => ({ resolve: { alias: { "@jed": root } } }) }] },
  build: { target: "esnext", minify: true, sourcemap: true, emptyOutDir: true, chunkSizeWarningLimit: 2000 },
};
