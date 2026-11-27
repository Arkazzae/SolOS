// Runs games in the WebAssembly build of web/core under Node, through the same wrapper the page uses
// (src/engine/core.ts), and natively (core/examples/check.rs), and compares a hash of every frame's
// picture and sound: the browser's machine must be the PC's bit for bit. Without games given it runs the
// cores' test ROMs that are fetched (cores/*/tests/roms/fetch.sh), the Sega cores' diagnostic cartridges and a
// PICO-8 cartridge from the repository.
//
//     pnpm check                        the test ROMs, 600 frames each
//     pnpm check -- nes=game.nes 1800   one game, 1800 frames

import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Machine, Pad } from '../src/engine/core.ts';

const root = fileURLToPath(new URL('../../', import.meta.url));
const wasm = `${root}target/wasm32-unknown-unknown/release/solweb.wasm`;
/** The clock both builds give (examples/check.rs's CLOCK), in UTC. */
const CLOCK = 1_700_000_000;

const scratch = mkdtempSync(join(tmpdir(), 'sol-check-'));
const mdRom = join(scratch, 'sol-md.md');
const smsRom = join(scratch, 'sol-sms.sms');
const ggRom = join(scratch, 'sol-gg.gg');

const DEFAULT_GAMES = [
  'nes=cores/nes/tests/roms/nes-test-roms/ny2011/ny2011.nes',
  'nes=cores/nes/tests/roms/nes-test-roms/stomper/smwstomp.nes',
  'nes=cores/nes/tests/roms/nes-test-roms/nes15-1.0.0/nes15-NTSC.nes',
  'gb=cores/gb/tests/roms/game-boy-test-roms/dmg-acid2/dmg-acid2.gb',
  'gbc=cores/gb/tests/roms/game-boy-test-roms/cgb-acid2/cgb-acid2.gbc',
  'gb=cores/gb/tests/roms/game-boy-test-roms/blargg/cpu_instrs/cpu_instrs.gb',
  'gba=cores/gba/tests/roms/tonc/bin/m7_demo.gba',
  'gba=cores/gba/tests/roms/tonc/bin/snd1_demo.gba',
  'gba=cores/gba/tests/roms/armwrestler/armwrestler-gba-fixed.gba',
  `md=${mdRom}`,
  `sms=${smsRom}`,
  `gg=${ggRom}`,
  'pico8=cores/pico8/examples/carts/orbit.p8',
].map((game) => {
  const at = game.indexOf('=');
  return `${game.slice(0, at)}=${resolve(root, game.slice(at + 1))}`;
});

/** FNV-1a, 32 bits, as examples/check.rs computes it. */
function fnv(hash: number, bytes: Uint8Array): number {
  for (let i = 0; i < bytes.length; i++) hash = Math.imul(hash ^ bytes[i]!, 0x01000193) >>> 0;
  return hash;
}

/** The buttons pressed at a frame, as examples/check.rs presses them. */
function padAt(frame: number): number {
  return frame % 120 >= 100 && frame % 120 < 106 ? Pad.start : 0;
}

interface Run {
  picture: number;
  audio: number;
  state: number;
  width: number;
  height: number;
  fps: number;
}

async function inWasm(module: WebAssembly.Module, sys: string, rom: Uint8Array, frames: number): Promise<Run> {
  const machine = await Machine.load(module);
  machine.open(sys, rom);
  machine.setClock(CLOCK, 0);
  let picture = 0x811c9dc5;
  let audio = 0x811c9dc5;
  // The time the page would take: a frame and its picture, not the hashing.
  let spent = 0;
  for (let frame = 0; frame < frames; frame++) {
    const start = performance.now();
    machine.runFrame([padAt(frame)]);
    const pixels = machine.picture();
    spent += performance.now() - start;
    picture = fnv(picture, new Uint8Array(pixels.buffer, pixels.byteOffset, pixels.byteLength));
    const sound = machine.audio();
    audio = fnv(audio, new Uint8Array(sound.buffer, sound.byteOffset, sound.byteLength));
  }
  const fps = frames / (spent / 1000);
  const state = machine.saveState();
  // A state goes back in and the machine runs on from it.
  machine.loadState(state);
  machine.runFrame([0]);
  return { picture, audio, state: state.length, width: machine.width, height: machine.height, fps };
}

function natively(sys: string, path: string, frames: number): Run {
  const out = execFileSync('cargo', ['run', '--release', '-q', '-p', 'solweb', '--example', 'check', '--', sys, path, String(frames)], {
    cwd: root,
    encoding: 'utf8',
  });
  return JSON.parse(out) as Run;
}

const args = process.argv.slice(2).filter((arg) => arg !== '--');
const frames = Number(args.find((arg) => /^\d+$/.test(arg)) ?? 600);
const given = args
  .filter((arg) => arg.includes('='))
  .map((arg) => `${arg.slice(0, arg.indexOf('='))}=${resolve(arg.slice(arg.indexOf('=') + 1))}`);
if (given.length === 0) {
  execFileSync('cargo', ['run', '--release', '-q', '-p', 'md', '--example', 'make-test-rom', '--', mdRom], { cwd: root, stdio: 'inherit' });
  execFileSync('cargo', ['run', '--release', '-q', '-p', 'sms', '--example', 'make-sms-test-rom', '--', smsRom, ggRom], { cwd: root, stdio: 'inherit' });
}
const list = given.length > 0 ? given : DEFAULT_GAMES.filter((game) => existsSync(game.slice(game.indexOf('=') + 1)));
if (list.length === 0) {
  console.error('no games: give sys=path, or fetch the test ROMs (cores/*/tests/roms/fetch.sh)');
  process.exit(2);
}

execFileSync('cargo', ['build', '--release', '-q', '--target', 'wasm32-unknown-unknown', '-p', 'solweb'], { cwd: root, stdio: 'inherit' });
const module = await WebAssembly.compile(readFileSync(wasm));
let failed = 0;
try {
  for (const game of list) {
    const [sys, path] = [game.slice(0, game.indexOf('=')), game.slice(game.indexOf('=') + 1)];
    const rom = new Uint8Array(readFileSync(path));
    const web = await inWasm(module, sys, rom, frames);
    const native = natively(sys, path, frames);
    const same = web.picture === native.picture && web.audio === native.audio && web.state === native.state;
    if (!same) failed++;
    const name = path.slice(path.lastIndexOf('/') + 1);
    console.log(
      `${same ? 'same' : 'DIFFERENT'}  ${sys} ${name}: ${frames} frames ${web.width}x${web.height}, ` +
        `wasm ${web.fps.toFixed(0)} fps, native ${native.fps.toFixed(0)} fps` +
        (same ? '' : `\n  wasm ${JSON.stringify(web)}\n  native ${JSON.stringify(native)}`),
    );
  }
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
process.exit(failed > 0 ? 1 : 0);
