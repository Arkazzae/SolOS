// The cases tools/browser-test.ts runs in a real browser: real IndexedDB transactions, the page's own
// worker and module, and a tiny homebrew NES cartridge made here, which writes its battery RAM.

import { Input } from '../src/engine/input.ts';
import type { FromWorker, Saved, ToWorker } from '../src/engine/protocol.ts';
import { Session, type SessionEvents } from '../src/engine/session.ts';
import { Speaker } from '../src/engine/sound.ts';
import * as store from '../src/engine/store.ts';
import { detect } from '../src/engine/systems.ts';

function check(value: unknown, why: string): asserts value {
  if (!value) throw new Error(why);
}

async function bounded<T>(promise: Promise<T>, ms = 3000): Promise<T> {
  let timer = 0;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = window.setTimeout(() => reject(new Error('test timed out')), ms);
    })]);
  } finally {
    clearTimeout(timer);
  }
}

async function rejected(promise: Promise<unknown>): Promise<void> {
  let failure: unknown;
  try {
    await bounded(promise, 1000);
  } catch (error) {
    failure = error;
  }
  check(failure, 'the failed operation was reported as successful');
  check(!(failure instanceof Error) || failure.message !== 'test timed out', 'the failed operation left its promise pending');
}

async function until(test: () => boolean | Promise<boolean>, why: string, ms = 3000): Promise<void> {
  const deadline = performance.now() + ms;
  while (performance.now() < deadline) {
    if (await test()) return;
    await new Promise((resolve) => window.setTimeout(resolve, 10));
  }
  throw new Error(why);
}

const wait = (ms: number) => new Promise((resolve) => window.setTimeout(resolve, ms));

/** Gives a session animation frames for a while, no button held, as the page's loop does. */
async function drive(session: Session, ms: number): Promise<void> {
  const idle = { pads: [0, 0, 0, 0], fast: false, pressed: 0, held: 0, connected: 0 };
  const end = performance.now() + ms;
  while (performance.now() < end) {
    session.frame(performance.now(), idle);
    await wait(16);
  }
}

/** Makes the next put into a store abort its transaction once the request succeeded: the request's
 * success comes before the commit, which never happens. */
function abortNextPut(name: string): { happened: Promise<void>; restore(): void } {
  const put = IDBObjectStore.prototype.put;
  let resolve!: () => void;
  const happened = new Promise<void>((done) => (resolve = done));
  const restore = () => {
    IDBObjectStore.prototype.put = put;
  };
  IDBObjectStore.prototype.put = function (value: unknown, key?: IDBValidKey) {
    const request = put.call(this, value, key);
    if (this.name === name) {
      restore();
      request.addEventListener('success', () => {
        this.transaction.abort();
        resolve();
      }, { once: true });
    }
    return request;
  };
  return { happened, restore };
}

/** An NROM cartridge with battery-backed RAM that writes `value` at $6000, then loops. */
function cartridge(value = 0x5a): Uint8Array<ArrayBuffer> {
  const rom = new Uint8Array(16 + 16384);
  rom.set([0x4e, 0x45, 0x53, 0x1a, 1, 0, 2]);
  rom.fill(0xea, 16);
  rom.set([0x78, 0xa9, value, 0x8d, 0, 0x60, 0x4c, 6, 0x80], 16);
  for (const at of [0x3ffa, 0x3ffc, 0x3ffe]) rom.set([0, 0x80], 16 + at);
  return rom;
}

/** A session on a canvas of its own, with what it says collected. */
async function openSession(game: store.Game, speaker: Speaker) {
  const canvas = document.body.appendChild(document.createElement('canvas'));
  const said = { saveFailed: 0, failed: [] as string[], menu: 0 };
  const events: SessionEvents = {
    status() {},
    menu: () => void said.menu++,
    saveFailed: () => void said.saveFailed++,
    failed: (error) => void said.failed.push(error),
  };
  const session = await Session.open(canvas, game, speaker, new Input(new EventTarget()), { shader: 'clean', quickResume: true }, events);
  return { session, said, done: () => canvas.remove() };
}

async function atomicAdd(): Promise<void> {
  const rom = cartridge(0x21);
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-1', rom));
  const id = [...digest].map((b) => b.toString(16).padStart(2, '0')).join('');
  const file = new File([rom], 'atomic.nes');
  const abort = abortNextPut('games');
  try {
    await rejected(store.add(file));
  } finally {
    abort.restore();
  }
  check((await store.game(id)) === undefined, 'an aborted import left a library record');
  check((await store.rom(id)) === undefined, 'an aborted import left orphan ROM bytes');
  check((await store.add(file)).added, 'the import could not be retried');
  await store.remove(id);
}

/** Each cartridge enables SRAM, writes its marker to the first byte and halts. */
async function segaCartridges(): Promise<void> {
  for (const [system, code, width, height] of [['sms', 4, 256, 192], ['gg', 6, 160, 144]] as const) {
    const rom = new Uint8Array(0x4000);
    rom.set([0x3e, 8, 0x32, 0xfc, 0xff, 0x3e, code, 0x32, 0, 0x80, 0x76]);
    const ext = system.toUpperCase();
    check(detect(`cart.${ext}`, rom) === system, 'headerless cartridge was not recognised');
    rom.set(new TextEncoder().encode('TMR SEGA'), 0x3ff0);
    rom[0x3fff] = code << 4;
    const prefixed = new Uint8Array(512 + rom.length);
    prefixed.set([0xaa, 0xbb], 8); // Also found in Mega Drive copier headers.
    prefixed.set(rom, 512);
    check(detect('cart.bin', prefixed) === system, 'copier-prefixed header was not recognised');
    check(detect('cart.gg', prefixed) === 'gg', 'Game Gear cartridge selected the wrong machine');
    const { game } = await store.add(new File([rom], `diagnostic.${ext}`));
    check(game.system === system, 'import selected the wrong console');
    const speaker = await Speaker.start();
    try {
      const first = await openSession(game, speaker);
      try {
        await drive(first.session, 150);
        check(await first.session.leave(), 'the running game could not save');
      } finally {
        first.done();
      }
      check((await store.sram(game.id))?.[0] === code, 'the cartridge save was lost');
      const saved = await store.game(game.id);
      check(saved?.resume && saved.shot, 'Quick Resume did not keep a picture');
      const shot = await createImageBitmap(saved.shot);
      check(shot.width === width && shot.height === height, 'the picture has the wrong dimensions');
      shot.close();
      const second = await openSession(saved, speaker);
      try {
        check(second.session.resumed, 'the game did not resume');
        check(await second.session.leave(), 'the resumed game could not save');
      } finally {
        second.done();
      }
    } finally {
      speaker.close();
      await store.remove(game.id);
    }
  }
  check(detect('empty.sms', new Uint8Array()) === null, 'an empty cartridge was accepted');
}

async function openRetry(): Promise<void> {
  // A separate module instance keeps this deliberately failed open out of the other cases.
  const path = '/src/engine/store.ts?open-retry-test';
  const isolated = (await import(/* @vite-ignore */ path)) as typeof store;
  const create = IDBDatabase.prototype.createObjectStore;
  IDBDatabase.prototype.createObjectStore = function (name: string, options?: IDBObjectStoreParameters) {
    const objectStore = create.call(this, name, options);
    if (name === 'slots') objectStore.transaction.abort();
    return objectStore;
  };
  try {
    await rejected(isolated.games());
  } finally {
    IDBDatabase.prototype.createObjectStore = create;
  }
  check((await bounded(isolated.games())).length === 0, 'a failed database open was cached permanently');
}

async function abortedWrite(): Promise<void> {
  await store.putSram('retry', new Uint8Array([1]));
  const abort = abortNextPut('saves');
  try {
    await rejected(store.putSram('retry', new Uint8Array([2])));
  } finally {
    abort.restore();
  }
  check((await store.sram('retry'))?.[0] === 1, 'an aborted transaction changed the old save');
  await bounded(store.putSram('retry', new Uint8Array([2])));
  check((await store.sram('retry'))?.[0] === 2, 'the next write could not recover');
}

async function uncommittedWorkerSave(): Promise<void> {
  const worker = new Worker(new URL('../src/engine/worker.ts', import.meta.url), { type: 'module' });
  let next = 0;
  const call = <T>(message: object): Promise<T> => bounded(new Promise((resolve, reject) => {
    const id = ++next;
    const heard = ({ data }: MessageEvent<FromWorker>) => {
      if (data.kind !== 'reply' || data.id !== id) return;
      worker.removeEventListener('message', heard);
      if (data.ok) resolve(data.value as T);
      else reject(new Error(data.error));
    };
    worker.addEventListener('message', heard);
    worker.postMessage({ ...message, id });
  }));
  try {
    await call({ kind: 'open', system: 'nes', rom: cartridge(), sram: new Uint8Array(8192), state: null });
    worker.postMessage({ kind: 'run', frames: 1, pads: [0], speed: 1, budget: 0 } satisfies ToWorker);
    const first = await call<Saved>({ kind: 'save', state: false, shot: false });
    check(first.sram?.[0] === 0x5a, 'the homebrew ROM did not change battery RAM');
    const retry = await call<Saved>({ kind: 'save', state: false, shot: false });
    check(retry.sram?.[0] === 0x5a, 'SRAM disappeared from the retry before a database commit');
    await store.putSram('worker', retry.sram);
    worker.postMessage({ kind: 'sram-kept', sram: retry.sram } satisfies ToWorker);
    const committed = await call<Saved>({ kind: 'save', state: false, shot: false });
    check(committed.sram === null, 'an acknowledged unchanged save was sent again');
  } finally {
    worker.terminate();
  }
}

async function manualRetry(): Promise<void> {
  const { game } = await store.add(new File([cartridge(0x6b)], 'manual.nes'));
  await store.putSram(game.id, new Uint8Array(8192));
  const speaker = await Speaker.start();
  const { session, said, done } = await openSession(game, speaker);
  try {
    // Frames, so that the cartridge writes its RAM.
    await drive(session, 150);
    const abort = abortNextPut('saves');
    try {
      // The game menu keeps the save; that write fails.
      session.pause();
      await bounded(abort.happened);
      await until(() => said.saveFailed > 0, 'the failed automatic save was not reported');
    } finally {
      abort.restore();
    }
    check(await bounded(session.saveSlot()), 'manual save did not recover after the database failure');
    check((await store.sram(game.id))?.[0] === 0x6b, 'manual save consumed but did not persist changed SRAM');
    check((await store.slot(game.id))?.length, 'manual save left no machine state');
    check((await store.game(game.id))?.slot, 'manual state and library flags disagreed');
    check(await bounded(session.leave()), 'leaving after a recovered save failed');
  } finally {
    speaker.close();
    done();
  }
}

async function failedExit(): Promise<void> {
  const { game } = await store.add(new File([cartridge()], 'persistence.nes'));
  await store.putSram(game.id, new Uint8Array(8192));
  const speaker = await Speaker.start();
  const { session, said, done } = await openSession(game, speaker);
  try {
    await drive(session, 150);
    const abort = abortNextPut('resume');
    try {
      check(!(await bounded(session.leave())), 'leaving reported success although its save failed');
      await bounded(abort.happened);
    } finally {
      abort.restore();
    }
    check(said.saveFailed > 0, 'a failed save left no notice');
    check((await store.sram(game.id))?.[0] === 0, 'the aborted checkpoint partially committed SRAM');
    check(!(await store.game(game.id))?.resume, 'the aborted checkpoint changed the library flag');
    check((await store.resumeState(game.id)) === undefined, 'the aborted checkpoint left a machine state');
    check(await bounded(session.leave()), 'leaving could not be retried');
    check((await store.sram(game.id))?.[0] === 0x5a, 'the retry lost changed cartridge data');
    check((await store.game(game.id))?.resume, 'the Quick Resume record was not committed');
  } finally {
    speaker.close();
    done();
  }
}

async function slotAndRestart(): Promise<void> {
  const { game } = await store.add(new File([cartridge(0x52)], 'slot.nes'));
  const speaker = await Speaker.start();
  const { session, said, done } = await openSession(game, speaker);
  try {
    await drive(session, 150);
    check(!(await session.loadSlot()), 'a state loaded from an empty slot');
    check(await bounded(session.saveSlot()), 'the state was not saved');
    check((await store.slot(game.id))?.length, 'the slot is empty after saving');
    check(await bounded(session.loadSlot()), 'the saved state did not load');
    await drive(session, 100);
    await bounded(session.restart());
    await drive(session, 100);
    check(said.failed.length === 0, `the machine stopped: ${said.failed.join(', ')}`);
    check(await bounded(session.leave()), 'leaving after starting over failed');
    check((await store.sram(game.id))?.[0] === 0x52, 'starting over lost the cartridge save');
  } finally {
    speaker.close();
    done();
  }
}

async function workerStartupError(): Promise<void> {
  const { game } = await store.add(new File([cartridge(0x33)], 'startup.nes'));
  const speaker = await Speaker.start();
  const Original = window.Worker;
  const workers: Worker[] = [];
  window.Worker = new Proxy(Original, {
    construct(target, args) {
      const worker = Reflect.construct(target, [new URL('/missing-worker.js', location.href), args[1]]) as Worker;
      workers.push(worker);
      return worker;
    },
  });
  const canvas = document.body.appendChild(document.createElement('canvas'));
  try {
    const events: SessionEvents = { status() {}, menu() {}, saveFailed() {}, failed() {} };
    await rejected(Session.open(canvas, game, speaker, new Input(new EventTarget()), { shader: 'clean', quickResume: true }, events));
  } finally {
    window.Worker = Original;
    for (const worker of workers) worker.terminate();
    speaker.close();
    canvas.remove();
  }
}

async function profilesKeepSavesApart(): Promise<void> {
  const { game } = await store.add(new File([cartridge(0x31)], 'profiles.nes'));
  const before = store.currentProfile();
  const ania = await store.addProfile('Ania');
  const bartek = await store.addProfile('Bartek');
  try {
    store.useProfile(ania.id);
    await store.putSram(game.id, new Uint8Array([1]));
    await store.saveGame({ ...game, fav: true, played: 5 });
    store.useProfile(bartek.id);
    check((await store.sram(game.id)) === undefined, 'one profile saw another profile\'s save');
    const seen = await store.game(game.id);
    check(seen !== undefined && !seen.fav && seen.played === 0, 'one profile saw another profile\'s favourite');
    await store.putSram(game.id, new Uint8Array([2]));
    await store.removeProfile(ania.id);
    store.useProfile(ania.id);
    check((await store.sram(game.id)) === undefined, 'a removed profile left its save');
    store.useProfile(bartek.id);
    check((await store.sram(game.id))?.[0] === 2, 'removing a profile took another profile\'s save');
    await store.remove(game.id);
    check((await store.sram(game.id)) === undefined, 'removing a game left a profile\'s save');
  } finally {
    store.useProfile(before);
    await store.removeProfile(ania.id).catch(() => {});
    await store.removeProfile(bartek.id).catch(() => {});
  }
}

/** The whole app: a game from Start, a failed sound start reported, a double click starting one game,
 * the game running and leaving back to Start. */
async function singleLaunch(): Promise<void> {
  const root = document.body.appendChild(document.createElement('div'));
  root.id = 'app';
  const Original = window.Worker;
  const startSound = Speaker.start;
  Speaker.start = async () => {
    throw new Error('forced audio startup failure');
  };
  const workers: Worker[] = [];
  window.Worker = new Proxy(Original, {
    construct(target, args) {
      const worker = Reflect.construct(target, args) as Worker;
      workers.push(worker);
      return worker;
    },
  });
  try {
    // A profile and a game, so that the app opens on Start with a card.
    if ((await store.profiles()).length === 0) await store.addProfile('Test');
    await store.add(new File([cartridge(0x44)], 'launch.nes'));
    const path = '/src/main.ts';
    await import(/* @vite-ignore */ path);
    const card = '.card[data-game]';
    await until(() => root.querySelector(card) !== null, 'the library did not load');
    root.querySelector<HTMLButtonElement>(card)!.click();
    await until(() => document.querySelector('.toast')?.textContent?.includes('forced audio startup failure') ?? false,
      'the audio startup failure was not reported');
    Speaker.start = startSound;
    const button = root.querySelector<HTMLButtonElement>(card)!;
    button.click();
    button.click();
    await until(() => workers.length > 0, 'the game did not start');
    await until(() => root.querySelector('.player canvas') !== null, 'the game did not show');
    // Started: the page lets go of what it held back while the game opened.
    // The app's own module instance, as the page loaded it.
    const state = '/src/ui/state/play.svelte.ts';
    const { play } = (await import(/* @vite-ignore */ state)) as typeof import('../src/ui/state/play.svelte.ts');
    await until(() => !play.opening, 'the game did not finish starting', 10_000);
    await wait(500);
    check(workers.length === 1, `double click started ${workers.length} game workers`);
    // The bar's first button leaves the game.
    root.querySelector<HTMLButtonElement>('.player .bar button')!.click();
    // Leaving keeps the game first, its last frame as a PNG too: slow on a busy machine.
    await until(() => root.querySelector('.player') === null && root.querySelector('.home') !== null, 'the launched game did not exit', 10_000);
  } finally {
    window.Worker = Original;
    Speaker.start = startSound;
    for (const worker of workers) worker.terminate();
  }
}

export async function run(): Promise<{ ok: boolean; cases: { name: string; result: string }[] }> {
  const cases: { name: string; result: string }[] = [];
  for (const [name, test] of [
    ['database open can be retried', openRetry],
    ['transaction abort rejects and allows retry', abortedWrite],
    ['game import is atomic and can be retried', atomicAdd],
    ['Master System and Game Gear import, play and resume', segaCartridges],
    ['worker retains uncommitted SRAM', uncommittedWorkerSave],
    ['manual save retries failed cartridge persistence', manualRetry],
    ['failed exit retains the game and saves on retry', failedExit],
    ['save, load and start over', slotAndRestart],
    ['worker startup failures reject pending calls', workerStartupError],
    ['profiles keep their saves apart', profilesKeepSavesApart],
    ['double click launches one game', singleLaunch],
  ] as const) {
    try {
      await test();
      cases.push({ name, result: 'passed' });
    } catch (error) {
      cases.push({ name, result: String(error) });
    }
  }
  return { ok: cases.every((test) => test.result === 'passed'), cases };
}
