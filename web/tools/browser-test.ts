// Runs tools/persistence-cases.ts in a real browser: headless Chromium (or Chrome) with a fresh, temporary
// profile, the page's own modules served by Vite, and the DevTools protocol to run them. No browser
// automation package is needed. SOL_BROWSER names the browser's executable; by default Chromium on the
// PATH, or Google Chrome where macOS keeps it.
//
//     pnpm test

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';

interface RpcReply {
  id?: number;
  result?: unknown;
  error?: { message: string };
}

const MAC_CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const executable = process.env.SOL_BROWSER ?? (process.platform === 'darwin' && existsSync(MAC_CHROME) ? MAC_CHROME : 'chromium');

const profile = await mkdtemp(join(tmpdir(), 'sol-browser-test-'));
// The page's own Vite settings (vite.config.ts); the cases load its modules directly, its Svelte app too.
const server = await createServer({
  configFile: fileURLToPath(new URL('../vite.config.ts', import.meta.url)),
  server: { host: '127.0.0.1', port: 0 },
  logLevel: 'warn',
});
let browser: ReturnType<typeof spawn> | undefined;
let socket: WebSocket | undefined;
try {
  await server.listen();
  const origin = server.resolvedUrls!.local[0]!;
  browser = spawn(executable, [
    '--headless', '--no-sandbox', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--mute-audio',
    '--autoplay-policy=no-user-gesture-required', '--no-first-run', '--no-default-browser-check', 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let launchError: Error | undefined;
  let diagnostics = '';
  browser.on('error', (error) => (launchError = error));
  browser.stderr?.on('data', (data: Buffer) => (diagnostics = (diagnostics + String(data)).slice(-4000)));
  let port = 0;
  for (let i = 0; i < 100 && !port; i++) {
    if (launchError) throw launchError;
    if (browser.exitCode !== null) throw new Error(`the browser exited: ${diagnostics}`);
    port = await readFile(join(profile, 'DevToolsActivePort'), 'utf8').then((s) => Number(s.split('\n')[0]), () => 0);
    if (!port) await delay(100);
  }
  if (!port) throw new Error(`the browser did not start: ${diagnostics}`);
  const targets = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()) as { type: string; webSocketDebuggerUrl: string }[];
  const target = targets.find((t) => t.type === 'page');
  if (!target) throw new Error('the browser has no page to test in');
  const ws = (socket = new WebSocket(target.webSocketDebuggerUrl));
  await new Promise<void>((resolve, reject) => {
    ws.addEventListener('open', () => resolve(), { once: true });
    ws.addEventListener('error', () => reject(new Error('DevTools connection failed')), { once: true });
  });
  let sequence = 0;
  const pending = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>();
  ws.addEventListener('close', () => {
    for (const call of pending.values()) call.reject(new Error('DevTools connection closed'));
    pending.clear();
  });
  ws.addEventListener('message', ({ data }) => {
    const reply = JSON.parse(String(data)) as RpcReply;
    if (reply.id === undefined) return;
    const call = pending.get(reply.id);
    pending.delete(reply.id);
    if (reply.error) call?.reject(new Error(reply.error.message));
    else call?.resolve(reply.result);
  });
  const call = <T>(method: string, params: object, timeout = 10_000): Promise<T> =>
    new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`DevTools timed out: ${method}`));
      }, timeout);
      pending.set(id, {
        resolve(value) {
          clearTimeout(timer);
          resolve(value as T);
        },
        reject(error) {
          clearTimeout(timer);
          reject(error);
        },
      });
      ws.send(JSON.stringify({ id, method, params }));
    });
  const page = `${origin}tools/browser-test.html`;
  await call('Page.navigate', { url: page });
  for (let i = 0; ; i++) {
    const ready = await call<{ result: { value: string } }>('Runtime.evaluate', { expression: 'location.href + " " + document.readyState', returnByValue: true });
    if (ready.result.value === `${page} complete`) break;
    if (i === 100) throw new Error('the test page did not load');
    await delay(50);
  }
  const result = await call<{
    result: { value?: { ok: boolean; cases: { name: string; result: string }[] } };
    exceptionDetails?: { text: string };
  }>('Runtime.evaluate', {
    expression: 'import("/tools/persistence-cases.ts").then((m) => m.run())',
    awaitPromise: true,
    returnByValue: true,
    timeout: 60_000,
  }, 65_000);
  if (!result.result.value) throw new Error(JSON.stringify(result.exceptionDetails ?? result));
  for (const test of result.result.value.cases) console.log(`${test.result === 'passed' ? 'PASS' : 'FAIL'} ${test.name}: ${test.result}`);
  if (!result.result.value.ok) process.exitCode = 1;
} finally {
  socket?.close();
  if (browser && browser.exitCode === null && browser.signalCode === null) {
    const closed = new Promise<void>((resolve) => browser!.once('close', () => resolve()));
    browser.kill();
    await Promise.race([closed, delay(3000)]);
    if (browser.exitCode === null && browser.signalCode === null) {
      browser.kill('SIGKILL');
      await closed;
    }
  }
  await server.close();
  await rm(profile, { recursive: true, force: true });
}
