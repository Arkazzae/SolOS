// The machine's worker: the WebAssembly cores run here, off the page's thread, so that the page stays
// smooth whatever a frame costs. The page tells it when to run frames and with which buttons (the
// Gamepad API is the page's alone); it draws each run's last frame on the page's canvas and sends the
// run's sound straight to the audio worklet.

import wasmUrl from '../../../target/wasm32-unknown-unknown/release/solweb.wasm?url';
import { Machine } from './core.ts';
import type { FromWorker, Opened, Request, Saved, SoundChunk, ToWorker } from './protocol.ts';
import { Screen } from './render.ts';

const scope = self as unknown as {
  onmessage: ((event: MessageEvent<ToWorker>) => void) | null;
  postMessage(message: FromWorker, transfer?: Transferable[]): void;
};

/** The module, compiled once; each game gets an instance of its own, with fresh memory. */
const compiled: Promise<WebAssembly.Module> = WebAssembly.compileStreaming(fetch(wasmUrl)).catch(async () =>
  WebAssembly.compile(await (await fetch(wasmUrl)).arrayBuffer()),
);

let screen: Screen | null = null;
let sound: MessagePort | null = null;
let machine: Machine | null = null;
let game: { system: string; rom: Uint8Array } | null = null;
/** The cartridge's save last committed by the page, to tell when it changes. */
let keptSram: Uint8Array | null = null;
/** The last snapshot's save, for powering on again after a core's fault or a failed database write. */
let lastSram: Uint8Array | null = null;
/** A run's sound, gathered frame by frame. */
let samples = new Int16Array(16384);

function same(a: Uint8Array | null, b: Uint8Array | null): boolean {
  if (a === null || b === null) return a === b;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** A fresh machine with the game in, its clock set and its save put back. */
async function power(system: string, rom: Uint8Array, sram: Uint8Array | null): Promise<Machine> {
  const next = await Machine.load(await compiled);
  next.open(system, rom);
  next.setClock();
  if (sram) next.loadSram(sram);
  return next;
}

function draw(m: Machine): void {
  screen?.draw(m.picture(), m.width, m.height, m.pixelAspect);
}

/** Runs the frames asked, draws the last and sends their sound; how many ran. Fast (a budget), it stops
 * before a frame that would not end within the budget, and sends only the last frame's sound: the ring
 * keeps its level and the sound its pitch, what lies between is skipped. */
function run(frames: number, pads: number[], speed: number, budget: number): number {
  const m = machine;
  if (!m) return 0;
  const start = performance.now();
  let length = 0;
  let ran = 0;
  let last = 0;
  while (ran < frames) {
    if (budget > 0 && ran > 0 && performance.now() - start + last > budget) break;
    const frameStart = performance.now();
    m.runFrame(pads);
    last = performance.now() - frameStart;
    ran++;
    const audio = m.audio();
    if (budget > 0) length = 0;
    if (length + audio.length > samples.length) {
      const grown = new Int16Array(Math.max(samples.length * 2, length + audio.length));
      grown.set(samples.subarray(0, length));
      samples = grown;
    }
    samples.set(audio, length);
    length += audio.length;
  }
  draw(m);
  if (sound && length > 0) {
    const chunk: SoundChunk = { samples: samples.slice(0, length), stereo: m.stereo, rate: m.sampleHz * speed };
    sound.postMessage(chunk, [chunk.samples.buffer]);
  }
  return ran;
}

/** The last frame as a PNG at the console's own size, for the library's card. */
async function shot(m: Machine): Promise<Blob> {
  const canvas = new OffscreenCanvas(m.width, m.height);
  canvas.getContext('2d')!.putImageData(new ImageData(m.picture().slice(), m.width, m.height), 0, 0);
  return canvas.convertToBlob({ type: 'image/png' });
}

async function request(message: Request): Promise<unknown> {
  switch (message.kind) {
    case 'open': {
      machine = null;
      game = { system: message.system, rom: message.rom };
      const m = await power(message.system, message.rom, message.sram);
      keptSram = lastSram = message.sram;
      let resumed = false;
      if (message.state) {
        try {
          m.loadState(message.state);
          // The state holds the cartridge's clock as it was: the time now again.
          m.setClock();
          resumed = true;
        } catch (error) {
          console.warn('Quick Resume state did not load:', error);
        }
      }
      machine = m;
      if (resumed) draw(m);
      return { frameHz: m.frameHz, resumed } satisfies Opened;
    }
    case 'save': {
      const m = machine;
      if (!m) return { sram: null, state: null, shot: null } satisfies Saved;
      // A cartridge's clock keeps the time the save was taken beside it.
      m.setClock();
      const sram = m.sram();
      lastSram = sram;
      return {
        sram: sram !== null && !same(sram, keptSram) ? sram : null,
        state: message.state ? m.saveState() : null,
        shot: message.shot ? await shot(m) : null,
      } satisfies Saved;
    }
    case 'load':
      machine?.loadState(message.state);
      machine?.setClock();
      if (machine) draw(machine);
      return null;
    case 'reset': {
      // Power off and on, the cartridge's save kept; after a core's fault too, with the last save.
      if (!game) return null;
      const m = await power(game.system, game.rom, machine?.sram() ?? lastSram ?? keptSram);
      machine = m;
      draw(m);
      return null;
    }
    case 'close':
      machine = game = keptSram = lastSram = null;
      return null;
  }
}

/** Messages are handled one after another, a request's awaits included. */
let queue: Promise<void> = Promise.resolve();

scope.onmessage = ({ data: message }) => {
  queue = queue.then(async () => {
    try {
      switch (message.kind) {
        case 'init':
          screen = new Screen(message.canvas, message.shader);
          sound = message.sound;
          return;
        case 'resize':
          screen?.resize(message.width, message.height);
          return;
        case 'shader':
          screen?.setShader(message.shader);
          return;
        case 'sram-kept':
          keptSram = message.sram;
          return;
        case 'run':
          scope.postMessage({ kind: 'ran', frames: run(message.frames, message.pads, message.speed, message.budget) });
          return;
        default:
          try {
            scope.postMessage({ kind: 'reply', id: message.id, ok: true, value: await request(message) });
          } catch (error) {
            scope.postMessage({ kind: 'reply', id: message.id, ok: false, error: String(error) });
          }
      }
    } catch (error) {
      // A core's panic traps the module; the machine cannot go on.
      machine = null;
      scope.postMessage({ kind: 'failed', error: String(error) });
    }
  });
};
