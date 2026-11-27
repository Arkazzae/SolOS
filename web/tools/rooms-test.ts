// Exercises a running Sol deployment from two fresh browser profiles. Each
// browser imports its own homebrew ROM. Set SOL_TEST_URL for a subpath or port.
import assert from 'node:assert/strict';
import { chromium, type Browser, type Page } from 'playwright-core';

type Check = { epoch: number; frame: number; hash: string };
declare global {
  interface Window {
    roomTrace: { socket?: WebSocket; go: number; frames: number; bothPads: boolean; checks: Check[]; mismatch: boolean; initial?: string };
  }
}

const origin = new URL(process.env.SOL_TEST_URL ?? 'http://localhost:8080/');
const executablePath = process.env.SOL_BROWSER ?? (process.platform === 'darwin'
  ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : '/usr/bin/chromium');
const browsers: Browser[] = [];

// Strobe both NES controllers and copy their A buttons into battery RAM.
const rom = Buffer.alloc(16 + 16384);
rom.set([0x4e, 0x45, 0x53, 0x1a, 1, 0, 2]);
rom.fill(0xea, 16);
rom.set([0xa9, 1, 0x8d, 0x16, 0x40, 0xa9, 0, 0x8d, 0x16, 0x40,
  0xad, 0x16, 0x40, 0x29, 1, 0x8d, 0, 0x60, 0xad, 0x17, 0x40, 0x29, 1,
  0x8d, 1, 0x60, 0x4c, 0, 0x80], 16);
for (const vector of [0x3ffa, 0x3ffc, 0x3ffe]) rom.set([0, 128], 16 + vector);

async function player(name: string, url: URL, slowerDisplay = false): Promise<Page> {
  const browser = await chromium.launch({ executablePath, args: [
    '--no-sandbox', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--autoplay-policy=no-user-gesture-required',
  ] });
  browsers.push(browser);
  const page = await browser.newPage();
  await page.addInitScript(slow => {
    if (slow) {
      const raf = window.requestAnimationFrame.bind(window);
      window.requestAnimationFrame = callback => raf(() => raf(callback));
    }
    window.roomTrace = { go: 0, frames: 0, bothPads: false, checks: [], mismatch: false };
    const trace = window.roomTrace;
    window.WebSocket = class extends WebSocket {
      constructor(url: string | URL, protocols?: string | string[]) {
        super(url, protocols);
        if (!new URL(this.url).pathname.endsWith('/netplay')) return;
        trace.socket = this;
        this.addEventListener('message', event => {
          const m = JSON.parse(event.data);
          if (m.type === 'go') trace.go = m.epoch;
          if (m.type === 'frame') {
            trace.frames++;
            trace.bothPads ||= m.pads[0] === 1 && m.pads[1] === 2;
          }
        });
      }
      override send(data: Parameters<WebSocket['send']>[0]): void {
        if (this === trace.socket && typeof data === 'string') {
          const m = JSON.parse(data);
          if (m.type === 'snapshot' && m.epoch === 1) {
            const state = Uint8Array.from(atob(m.data), c => c.charCodeAt(0));
            void crypto.subtle.digest('SHA-256', state).then(result => {
              trace.initial = Array.from(new Uint8Array(result), b => b.toString(16).padStart(2, '0')).join('');
            });
          }
          if (m.type === 'check') {
            trace.checks.push(m);
            if (trace.mismatch) {
              trace.mismatch = false;
              data = JSON.stringify({ ...m, hash: '0'.repeat(64) });
            }
          }
        }
        super.send(data);
      }
    };
  }, slowerDisplay);
  await page.goto(url.href);
  await page.getByRole('button', { name: 'English' }).click();
  await page.locator('input.field').fill(name);
  await page.locator('.press-a').click();
  return page;
}

async function upload(page: Page, name: string, buffer: Buffer): Promise<void> {
  const scope = await page.getByRole('dialog').count() ? page.getByRole('dialog') : page;
  const [chooser] = await Promise.all([page.waitForEvent('filechooser'), scope.getByRole('button', { name: 'Choose files' }).click()]);
  await chooser.setFiles({ name, mimeType: 'application/octet-stream', buffer });
}

async function checksum(page: Page, epoch: number): Promise<Check> {
  const found = await page.waitForFunction(n => window.roomTrace.checks.find(c => c.epoch === n), epoch);
  return await found.jsonValue() as Check;
}

async function personalSaves(page: Page): Promise<string[][]> {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>(resolve => {
      const request = indexedDB.open('sol');
      request.onsuccess = () => resolve(request.result);
    });
    try {
      return await Promise.all(['saves', 'resume', 'slots'].map(async store => {
        const values = await new Promise<Uint8Array<ArrayBuffer>[]>(resolve => {
          const request = db.transaction(store).objectStore(store).getAll();
          request.onsuccess = () => resolve(request.result);
        });
        return Promise.all(values.map(async value => {
          const digest = await crypto.subtle.digest('SHA-256', value);
          return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
        }));
      }));
    } finally { db.close(); }
  });
}

try {
  const hostUrl = new URL(origin);
  hostUrl.searchParams.set('stats', '1');
  const host = await player('P1', hostUrl);
  await upload(host, 'Two pads.nes', rom);
  await host.locator('.card[data-game]').first().click();
  await host.locator('.stats').waitFor();
  await host.keyboard.press('Escape');
  await host.locator('[data-act="exit"]').click();
  await host.locator('.player').waitFor({ state: 'detached' });
  const before = await personalSaves(host);
  assert.equal(before[1]!.length, 1, 'solo play created a Quick Resume');
  await host.locator('.card-more').first().click({ force: true });
  await host.getByRole('button', { name: 'Play online' }).click();
  const invite = new URL(origin);
  invite.searchParams.set('room', await host.locator('.room-code').innerText());
  const guest = await player('P2', invite, true);
  await guest.getByRole('button', { name: 'Later' }).click();
  await guest.getByText('Choose the same NES file as the host.').waitFor();
  const wrong = Buffer.from(rom);
  wrong[35] = wrong[35]! ^ 1;
  await upload(guest, 'Other.nes', wrong);
  await guest.getByText('This is a different game file.').waitFor();
  await upload(guest, 'Same bytes with a different name.nes', rom);
  await host.getByLabel('Start with my local progress').check();
  const guestBefore = await personalSaves(guest);
  const players = [host, guest];
  await Promise.all(players.map(p => p.getByRole('button', { name: 'Ready to play' }).click()));
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.frames > 10)));
  await host.keyboard.down('x');
  await guest.keyboard.down('z');
  await guest.waitForFunction(() => window.roomTrace.bothPads);
  await host.keyboard.up('x');
  await guest.keyboard.up('z');
  assert.deepEqual(await checksum(host, 1), await checksum(guest, 1));
  assert.equal(await host.evaluate(() => window.roomTrace.initial), before[1]![0], 'host started from the selected save');
  console.log('PASS invite, ROM matching, separate pads and identical state across display rates');

  await guest.evaluate(() => window.roomTrace.socket!.close());
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.go === 2)));
  await host.keyboard.press('Escape');
  await guest.getByText('Waiting for the other player to be ready…').waitFor();
  await host.locator('[data-act="back"]').click();
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.go === 3)));
  await guest.evaluate(() => { window.roomTrace.mismatch = true; });
  assert.deepEqual(await checksum(host, 4), await checksum(guest, 4));
  console.log('PASS reconnect, shared pause and recovery from a mismatching checksum');

  await host.keyboard.press('Escape');
  await host.locator('[data-act="exit"]').click();
  await guest.getByText('The room has closed.').waitFor();
  await guest.locator('.room-overlay button').click();
  assert.deepEqual(await personalSaves(host), before);
  assert.deepEqual(await personalSaves(guest), guestBefore);
  // A typed invitation uses the same server. The closed room must be gone.
  await host.getByRole('button', { name: 'Join by code' }).click();
  await host.getByLabel('Room code').fill(invite.searchParams.get('room')!);
  await host.getByRole('dialog').getByRole('button', { name: 'Join by code' }).click();
  await host.getByText('This room has expired or the code belongs to another server.').waitFor();
  console.log('PASS leave, local save isolation and expired room code');
} finally {
  await Promise.all(browsers.map(b => b.close()));
}
