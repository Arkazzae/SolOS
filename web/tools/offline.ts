// The page for offline use: after a build, a service worker (dist/sw.js) that holds every file of the
// build, so a page loaded once starts with no network, as an installed app should. The files' list and a
// hash of their bytes are written into it, so each build is a new worker: the browser installs it beside
// the running one, and it takes over at the next start (never under a game being played).

import { createHash } from 'node:crypto';
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { join, relative, resolve, sep } from 'node:path';
import type { Plugin } from 'vite';

async function files(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await files(path)));
    else out.push(path);
  }
  return out;
}

const WORKER = (build: string, list: string[]) => `// Sol's service worker, made by tools/offline.ts: every file of build ${build}.
const CACHE = 'sol-${build}';
const FILES = ${JSON.stringify(list)};
const scope = self.registration.scope;
const url = (file) => new URL(file, scope).href;

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(FILES.map(url))));
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    for (const key of await caches.keys()) if (key.startsWith('sol-') && key !== CACHE) await caches.delete(key);
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET' || !request.url.startsWith(scope)) return;
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    // The page itself, at any address under the scope, is the build's index.html.
    // ignoreVary: a module script or a font asks with an Origin header the install's requests had not.
    const known = await cache.match(request.mode === 'navigate' ? url('index.html') : request, { ignoreSearch: true, ignoreVary: true });
    return known ?? fetch(request);
  })());
});
`;

export function offline(): Plugin {
  let outDir = '';
  return {
    name: 'sol-offline',
    apply: 'build',
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    async closeBundle() {
      const paths = (await files(outDir)).filter((path) => !path.endsWith(`${sep}sw.js`)).sort();
      const hash = createHash('sha256');
      const list: string[] = [];
      for (const path of paths) {
        const name = relative(outDir, path).split(sep).join('/');
        hash.update(name).update(await readFile(path));
        list.push(name);
      }
      const build = hash.digest('hex').slice(0, 12);
      await writeFile(join(outDir, 'sw.js'), WORKER(build, list));
      this.info(`sw.js: ${list.length} files, build ${build}`);
    },
  };
}
