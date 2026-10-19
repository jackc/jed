// Benchmark-only browser profile mapping using the source-map utility already installed
// with impl/ts. Maps are read once; the returned CDP call-frame mapper is synchronous.
import { readdir, readFile } from "node:fs/promises";
import { join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import sourceMap from "../../../impl/ts/node_modules/source-map-js/source-map.js";

const { SourceMapConsumer } = sourceMap;

async function mapFiles(folder) {
  const files = [];
  for (const entry of await readdir(folder, { withFileTypes: true })) {
    const path = join(folder, entry.name);
    if (entry.isDirectory()) files.push(...await mapFiles(path));
    else if (entry.isFile() && entry.name.endsWith(".map")) files.push(path);
  }
  return files.sort();
}

function urlPath(value) {
  try {
    return decodeURIComponent(new URL(value, "http://benchmark.invalid/").pathname);
  } catch {
    return undefined;
  }
}

/**
 * Load external .map files below a built site directory. The callback accepts zero-based
 * CDP frames and returns a mapped frame or undefined. Paths are matched against the complete
 * relative bundle path, allowing a served URL prefix without ambiguous basename matching.
 *
 * resolveSource(source, context) can override the default original URL, which follows source
 * map resolution against the local map file. context contains mapFile, bundlePath, frame and
 * original; source already includes the map's sourceRoot. Return undefined to keep the default.
 */
export async function loadSourceMaps(siteFolder, { resolveSource } = {}) {
  if (resolveSource !== undefined && typeof resolveSource !== "function") {
    throw new TypeError("resolveSource must be a function");
  }
  const folder = resolve(siteFolder);
  const maps = [];
  for (const mapFile of await mapFiles(folder)) {
    const raw = JSON.parse(await readFile(mapFile, "utf8"));
    const bundlePath = relative(folder, mapFile.slice(0, -4)).replaceAll("\\", "/");
    maps.push({ mapFile, bundlePath, consumer: new SourceMapConsumer(raw) });
  }
  // Prefer the longest complete path when assets contain a shared suffix.
  maps.sort((a, b) => b.bundlePath.length - a.bundlePath.length ||
    (a.bundlePath < b.bundlePath ? -1 : a.bundlePath > b.bundlePath ? 1 : 0));
  const cache = new Map();

  const mapper = (frame) => {
    if (!Number.isInteger(frame.lineNumber) || frame.lineNumber < 0 ||
        !Number.isInteger(frame.columnNumber) || frame.columnNumber < 0) return undefined;
    let map = cache.get(frame.url);
    if (map === undefined) {
      const pathname = urlPath(frame.url);
      map = pathname === undefined ? null : maps.find((candidate) =>
        pathname.endsWith(`/${candidate.bundlePath}`)) ?? null;
      cache.set(frame.url, map);
    }
    if (map === null) return undefined;
    const original = map.consumer.originalPositionFor({
      line: frame.lineNumber + 1,
      column: frame.columnNumber,
    });
    if (original.source === null || original.line === null || original.column === null) {
      return undefined;
    }
    const custom = resolveSource?.(original.source, {
      mapFile: map.mapFile, bundlePath: map.bundlePath, frame, original,
    });
    if (custom !== undefined && typeof custom !== "string") {
      throw new TypeError("resolveSource must synchronously return a URL string or undefined");
    }
    return {
      url: custom ?? new URL(original.source, pathToFileURL(map.mapFile)).href,
      functionName: original.name || frame.functionName,
      lineNumber: original.line - 1,
      columnNumber: original.column,
    };
  };
  mapper.maps = maps.map(({ mapFile, bundlePath }) => ({ mapFile, bundlePath }));
  return mapper;
}
