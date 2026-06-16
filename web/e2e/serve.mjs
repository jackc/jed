// A tiny static file server for the e2e suite that serves the built `build/` directory WHOLESALE —
// faithfully to GitHub Pages (which serves the artifact as-is). SvelteKit's own `vite preview` serves
// only its manifest and 404s post-build files like the Pagefind index, so it can't exercise search;
// this can. Plain JS (no build step), zero dependencies.

import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname } from 'node:path';

const ROOT = new URL('../build/', import.meta.url);
const PORT = Number(process.env.PORT || 4173);
// Optional base prefix (e.g. /jed) to emulate a GitHub Pages project page; stripped before lookup.
const BASE = process.env.BASE || '';

const TYPES = {
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript; charset=utf-8',
	'.mjs': 'text/javascript; charset=utf-8',
	'.css': 'text/css; charset=utf-8',
	'.json': 'application/json; charset=utf-8',
	'.svg': 'image/svg+xml',
	'.ico': 'image/x-icon',
	'.wasm': 'application/wasm',
	'.woff2': 'font/woff2',
	'.txt': 'text/plain; charset=utf-8'
};

const server = createServer(async (req, res) => {
	try {
		let path = decodeURIComponent((req.url ?? '/').split('?')[0]);
		if (BASE && path.startsWith(BASE)) path = path.slice(BASE.length) || '/';
		else if (BASE && path !== '/') {
			res.writeHead(404);
			return res.end('not found');
		}
		let file = new URL('.' + path, ROOT);
		const s = await stat(file).catch(() => null);
		if (s?.isDirectory()) {
			if (!path.endsWith('/')) {
				res.writeHead(301, { location: path + '/' });
				return res.end();
			}
			file = new URL('index.html', file);
		} else if (!s && extname(path) === '') {
			// Extension-less route -> its prerendered index.html (trailingSlash: 'always').
			file = new URL('.' + (path.endsWith('/') ? path : path + '/') + 'index.html', ROOT);
		}
		const body = await readFile(file);
		res.writeHead(200, {
			'content-type': TYPES[extname(file.pathname)] ?? 'application/octet-stream'
		});
		res.end(body);
	} catch {
		res.writeHead(404, { 'content-type': 'text/plain' });
		res.end('not found');
	}
});

server.listen(PORT, () => console.log(`static server serving build/ on http://localhost:${PORT}`));
