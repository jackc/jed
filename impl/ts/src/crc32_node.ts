// Node-only CRC-32/IEEE backend. Keep this module out of the browser/OPFS graph: node:zlib provides
// the same finalized incremental API as crc32.ts and uses the runtime's optimized native path.

import { crc32 } from "node:zlib";

import { type Crc32Backend, installCrc32Backend } from "./crc32.ts";

const nodeZlibCrc32: Crc32Backend = (previous, data) => crc32(data, previous) >>> 0;

installCrc32Backend("node:zlib", nodeZlibCrc32);
