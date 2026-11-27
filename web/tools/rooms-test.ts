// Exercises a running Sol deployment from two fresh browser profiles. Each
// browser imports its own homebrew ROM. Set SOL_TEST_URL for a subpath or port.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { Machine, Pad } from '../src/engine/core.ts';
import { chromium, type Browser, type Page } from 'playwright-core';

type Trace = {
  socket?: WebSocket; go: number; frames: number; checks: Check[]; bothPads: boolean;
  inputs: [number, number][]; snapshots: Record<number, string>;
  muteDownstream: boolean; heldPress?: number; hold: boolean; release(): void; rewinds: number; head: number; corruptEpoch: number;
};
type Check = { epoch: number; frame: number; hash: string };
declare global {
  interface Window {
    roomTrace: Trace;
  }
}

const origin = new URL(process.env.SOL_TEST_URL ?? 'http://localhost:8080/');
const executablePath = process.env.SOL_BROWSER ?? (process.platform === 'darwin'
  ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : '/usr/bin/chromium');
const browsers: Browser[] = [];

// Latch P1 A/B and P2 B in separate SRAM bytes. Reading each port twice
// distinguishes the actual NES serial inputs as well as their assigned seats.
const rom = Buffer.alloc(16 + 16384);
rom.set([0x4e, 0x45, 0x53, 0x1a, 1, 0, 2]);
rom.fill(0xea, 16);
const latch = (port: number, address: number) => [
  0xad, port, 0x40, 0x29, 1, 0x0d, address, 0x60, 0x8d, address, 0x60,
];
rom.set([0xa9, 1, 0x8d, 0x16, 0x40, 0xa9, 0, 0x8d, 0x16, 0x40,
  ...latch(0x16, 0), ...latch(0x16, 2), 0xad, 0x17, 0x40, ...latch(0x17, 1),
  0x4c, 0, 0x80], 16);
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
    const held: string[] = [];
    window.roomTrace = { go: 0, frames: 0, bothPads: false, checks: [], inputs: [], snapshots: {},
      muteDownstream: false, hold: false, release() {
        this.hold = false;
        for (const packet of held.splice(0)) this.socket!.send(packet);
      }, rewinds: 0, head: 0, corruptEpoch: -1 };
    const trace = window.roomTrace;
    const post = Worker.prototype.postMessage;
    Worker.prototype.postMessage = function(message, options?: StructuredSerializeOptions | Transferable[]) {
      if (message.kind === 'net-frames') {
        trace.head = message.start + message.pads.length;
        if (message.rewind) trace.rewinds++;
        if (trace.go === trace.corruptEpoch) {
          for (const pads of message.pads) pads[0] |= 1; // Extra P1 B on this machine only.
        }
      }
      post.call(this, message, Array.isArray(options) ? { transfer: options } : options);
    };
    window.WebSocket = class extends WebSocket {
      constructor(url: string | URL, protocols?: string | string[]) {
        super(url, protocols);
        if (!new URL(this.url).pathname.endsWith('/netplay')) return;
        trace.socket = this;
        trace.muteDownstream = false;
        this.addEventListener('message', event => {
          if (trace.muteDownstream) { event.stopImmediatePropagation(); return; }
          const m = JSON.parse(event.data);
          if (m.type === 'go') trace.go = m.epoch;
          if (m.type === 'frame') {
            trace.frames++;
            if (trace.go === 1) trace.inputs[m.frame] = m.pads;
            trace.bothPads ||= m.pads[0] === 1 && m.pads[1] === 2;
          }
        });
      }
      override send(data: Parameters<WebSocket['send']>[0]): void {
        if (this === trace.socket && typeof data === 'string') {
          const m = JSON.parse(data);
          if (m.type === 'snapshot') trace.snapshots[m.epoch] = m.data;
          if (m.type === 'check') trace.checks.push(m);
          if (m.type === 'input' && trace.hold) {
            held.push(data);
            if (m.pad === 2) trace.heldPress ??= m.frame;
            return;
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

async function rate(page: Page): Promise<number> {
  return page.evaluate(async () => {
    const start = performance.now(), first = window.roomTrace.frames;
    await new Promise(resolve => setTimeout(resolve, 2500));
    return 1000 * (window.roomTrace.frames - first) / (performance.now() - start);
  });
}

async function inspectState(host: Page, snapshot: string): Promise<Machine> {
  const url = await host.evaluate(() => performance.getEntriesByType('resource').find(e =>
    new URL(e.name).pathname.match(/solweb[^/]*\.wasm$/))!.name);
  const wasm = await (await host.request.get(url)).body();
  const machine = await Machine.load(new Uint8Array(wasm));
  machine.open('nes', rom);
  machine.loadState(Buffer.from(snapshot, 'base64'));
  return machine;
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
  const hz = await rate(host);
  assert.ok(hz > 52 && hz < 70, `60/30 Hz displays delivered ${hz.toFixed(1)} NES frames/s`);

  // Hold P2's wire input while its keyboard changes. The host predicts the
  // previous value for at most six frames, then both resume on confirmed input.
  await guest.evaluate(() => { window.roomTrace.hold = true; });
  await guest.keyboard.down('z');
  await host.keyboard.down('x');
  const pressed = await (await guest.waitForFunction(() => window.roomTrace.heldPress)).jsonValue();
  await host.waitForFunction(n => window.roomTrace.head > n!, pressed);
  const rewinds = await host.evaluate(() => window.roomTrace.rewinds);
  await guest.evaluate(() => window.roomTrace.release());
  await guest.waitForFunction(() => window.roomTrace.bothPads);
  await host.waitForFunction(n => window.roomTrace.rewinds > n, rewinds);
  await host.keyboard.up('x');
  await guest.keyboard.up('z');

  await guest.evaluate(() => { window.roomTrace.hold = true; });
  await host.getByText('Waiting for controller input from the network…').waitFor();
  await host.waitForFunction(() => window.roomTrace.frames === window.roomTrace.head);
  const stopped = await host.evaluate(() => window.roomTrace.head);
  await host.waitForTimeout(150);
  assert.equal(await host.evaluate(() => window.roomTrace.head), stopped);
  await guest.evaluate(() => window.roomTrace.release());
  const checked = await checksum(host, 1);
  assert.deepEqual(checked, await checksum(guest, 1));
  const { initial, inputs } = await host.evaluate(() => ({
    initial: window.roomTrace.snapshots[1]!, inputs: window.roomTrace.inputs,
  }));
  assert.equal(createHash('sha256').update(Buffer.from(initial, 'base64')).digest('hex'), before[1]![0]);
  const reference = await inspectState(host, initial);
  const buttons = [Pad.east, Pad.south, Pad.select, Pad.start, Pad.up, Pad.down, Pad.left, Pad.right];
  for (const pair of inputs.slice(0, checked.frame)) {
    reference.runFrame(pair.map(pad => buttons.reduce<number>((bits, button, i) => bits | (pad >> i & 1 ? button : 0), 0)));
  }
  assert.deepEqual(Array.from(reference.sram()!.slice(0, 3)), [1, 1, 0]);
  assert.equal(createHash('sha256').update(reference.saveState()).digest('hex'), checked.hash);
  console.log(`PASS both NES ports, rollback against independent emulation, bounded stall and ${hz.toFixed(1)} frames/s on 60/30 Hz displays`);

  const emulator = guest.workers()[0]!;
  await emulator.evaluate(() => {
    const slow = (event: MessageEvent) => {
      if (event.data.kind !== 'net-frames') return;
      const until = performance.now() + 32 * event.data.pads.length;
      while (performance.now() < until) { /* Test fixture: a slow emulated frame. */ }
    };
    self.addEventListener('message', slow);
    Object.assign(self, { restoreSpeed: () => self.removeEventListener('message', slow) });
  });
  await guest.getByText('This device is running the emulator slowly.').waitFor();
  await host.getByText('The other player’s device is running the emulator slowly.').waitFor();
  await emulator.evaluate(() => (self as unknown as { restoreSpeed(): void }).restoreSpeed());
  await host.getByText('The other player’s device is running the emulator slowly.').waitFor({ state: 'hidden' });
  await guest.evaluate(() => window.roomTrace.socket!.close());
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.go === 2)));
  await host.keyboard.press('Escape');
  await guest.getByText('Waiting for the other player to be ready…').waitFor();
  await host.locator('[data-act="back"]').click();
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.go === 3)));
  await guest.evaluate(() => { window.roomTrace.corruptEpoch = window.roomTrace.go; });
  assert.deepEqual(await checksum(host, 4), await checksum(guest, 4));
  console.log('PASS slow-device feedback, reconnect, shared pause and recovery from genuine emulator divergence');

  // The link appears open, but even the relay heartbeat cannot get through.
  await guest.evaluate(() => { window.roomTrace.muteDownstream = true; });
  await guest.getByText('Waiting for controller input from the network…').waitFor();
  await Promise.all(players.map(p => p.waitForFunction(() => window.roomTrace.go === 5)));
  assert.deepEqual(await checksum(host, 5), await checksum(guest, 5));
  console.log('PASS silent socket replacement and confirmed recovery');

  await host.keyboard.press('Escape');
  await host.locator('[data-act="exit"]').click();
  await guest.getByText('The room has closed.').waitFor();
  await guest.locator('.room-overlay button:focus').waitFor();
  await guest.keyboard.press('Enter');
  await guest.locator('.player').waitFor({ state: 'detached' });
  assert.deepEqual(await personalSaves(host), before);
  assert.deepEqual(await personalSaves(guest), guestBefore);
  // A typed invitation uses the same server. The closed room must be gone.
  await host.getByRole('button', { name: 'Join by code' }).click();
  await host.getByLabel('Room code').fill(invite.searchParams.get('room')!);
  await host.getByRole('dialog').getByRole('button', { name: 'Join by code' }).click();
  await host.getByText('This room has expired or the code belongs to another server.').waitFor();
  console.log('PASS keyboard exit, local save isolation and expired room code');

  // A later battery save must win over an old Quick Resume when Quick Resume is off.
  await host.evaluate(() => {
    const prefs = JSON.parse(localStorage.getItem('sol-prefs')!);
    localStorage.setItem('sol-prefs', JSON.stringify({ ...prefs, quickResume: false }));
  });
  await host.reload();
  await host.locator('.card[data-game]').first().click();
  await host.locator('.stats').waitFor();
  await host.keyboard.down('z');
  await host.waitForTimeout(180);
  await host.keyboard.up('z');
  await host.keyboard.press('Escape');
  await host.locator('[data-act="exit"]').click();
  await host.locator('.player').waitFor({ state: 'detached' });
  const latest = await personalSaves(host);
  assert.notDeepEqual(latest[0], before[0]);
  assert.deepEqual(latest[1], before[1]);
  await host.locator('.card-more').first().click({ force: true });
  await host.getByRole('button', { name: 'Play online' }).click();
  const code = await host.locator('.room-code').innerText();
  await guest.getByRole('button', { name: 'Join by code' }).click();
  await guest.getByLabel('Room code').fill(code);
  await guest.getByLabel('Room code').press('Enter');
  await host.getByLabel('Start with my local progress').check();
  await Promise.all(players.map(p => p.getByRole('button', { name: 'Ready to play' }).click()));
  await host.waitForFunction(() => !!window.roomTrace.snapshots[1]);
  const fromBattery = await inspectState(host, await host.evaluate(() => window.roomTrace.snapshots[1]!));
  assert.equal(fromBattery.sram()![2], 1);
  console.log('PASS latest battery progress with Quick Resume disabled');

  await host.waitForFunction(() => window.roomTrace.go === 1 && window.roomTrace.frames > 5);
  await host.keyboard.press('Escape');
  await guest.getByText('Waiting for the other player to be ready…').waitFor();
  await guest.locator('.room-overlay button:focus').waitFor();
  await guest.evaluate(() => {
    const pad = { connected: true, axes: [0, 0], buttons: [{ pressed: true, value: 1 }] };
    Object.defineProperty(navigator, 'getGamepads', { configurable: true, value: () => [pad] });
  });
  await guest.locator('.player').waitFor({ state: 'detached' });
  console.log('PASS gamepad A on the waiting screen');
} finally {
  await Promise.all(browsers.map(b => b.close()));
}
