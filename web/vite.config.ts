// Sol in a browser: one static page. The build (dist/) is plain files that any web server can serve.
// SOL_BASE sets the path it is served under, e.g. SOL_BASE=/sol/ for https://example.org/sol/.

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';
import { offline } from './tools/offline.ts';

// The version Settings › System shows: package.json's, and the commit when the build has git.
const { version } = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string };
let commit = process.env.SOL_COMMIT ?? '';
try {
  commit ||= execFileSync('git', ['rev-parse', '--short', 'HEAD'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
} catch {
  // A build without git (a Docker build context): the version alone.
}

export default defineConfig({
  base: process.env.SOL_BASE ?? '/',
  plugins: [svelte(), offline()],
  define: { __SOL_VERSION__: JSON.stringify(commit ? `${version} · ${commit}` : version) },
  worker: { format: 'es' },
  build: { target: 'es2022' },
  // The module is built into the workspace's target/, outside this folder.
  server: { fs: { allow: ['..'] } },
});
