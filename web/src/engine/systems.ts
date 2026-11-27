// The consoles the page plays, by the ids web/core takes: what they are called, their games' files, how
// a file is told to be a game for one, and their buttons as the keyboard and the pads give them. How the
// page shows each one (its hue, its epic name, its picture) is ui/consoles.ts.

import { Pad } from './core.ts';

export type SystemId = 'nes' | 'md' | 'sms' | 'gg' | 'gb' | 'gbc' | 'gba' | 'pico8';

export interface System {
  id: SystemId;
  /** The name on a card's chip and a shelf's tab, the name in a sentence, and the name in full. */
  short: string;
  name: string;
  full: string;
  /** Its games' file extensions, its own first. */
  extensions: readonly string[];
  /** The pads it takes: players 1 to this. */
  players: number;
  /** Its buttons besides the d-pad, by the pad bit they are on (web/core/src/cores), as its pad
   * names them. */
  buttons: readonly (readonly [bit: number, label: string])[];
}

const NINTENDO = [[Pad.east, 'A'], [Pad.south, 'B'], [Pad.start, 'Start'], [Pad.select, 'Select']] as const;

export const SYSTEMS: Readonly<Record<SystemId, System>> = {
  nes: {
    id: 'nes', short: 'NES', name: 'NES', full: 'Nintendo Entertainment System', extensions: ['nes'], players: 2,
    buttons: NINTENDO,
  },
  md: {
    id: 'md', short: 'MD', name: 'Mega Drive', full: 'Mega Drive', extensions: ['md', 'gen', 'smd', 'bin'], players: 2,
    buttons: [
      [Pad.west, 'A'], [Pad.south, 'B'], [Pad.east, 'C'], [Pad.l1, 'X'], [Pad.north, 'Y'], [Pad.r1, 'Z'],
      [Pad.start, 'Start'], [Pad.select, 'Mode'],
    ],
  },
  sms: {
    id: 'sms', short: 'SMS', name: 'Master System', full: 'Sega Master System', extensions: ['sms'], players: 2,
    buttons: [[Pad.south, '1'], [Pad.east, '2'], [Pad.start, 'Pause']],
  },
  gg: {
    id: 'gg', short: 'GG', name: 'Game Gear', full: 'Sega Game Gear', extensions: ['gg'], players: 1,
    buttons: [[Pad.south, '1'], [Pad.east, '2'], [Pad.start, 'Start']],
  },
  gb: {
    id: 'gb', short: 'GB', name: 'Game Boy', full: 'Game Boy', extensions: ['gb', 'sgb'], players: 1,
    buttons: NINTENDO,
  },
  gbc: {
    id: 'gbc', short: 'GBC', name: 'Game Boy Color', full: 'Game Boy Color', extensions: ['gbc', 'cgb'], players: 1,
    buttons: NINTENDO,
  },
  gba: {
    id: 'gba', short: 'GBA', name: 'Game Boy Advance', full: 'Game Boy Advance', extensions: ['gba'], players: 1,
    buttons: [...NINTENDO, [Pad.l1, 'L'], [Pad.r1, 'R']],
  },
  pico8: {
    id: 'pico8', short: 'PICO-8', name: 'PICO-8', full: 'PICO-8', extensions: ['p8.png', 'p8'], players: 4,
    buttons: [[Pad.south, 'O'], [Pad.east, 'X'], [Pad.start, 'Pause']],
  },
};

/** The consoles in the order the console lists them. */
export const SYSTEM_IDS: readonly SystemId[] = ['nes', 'md', 'sms', 'gb', 'gbc', 'gba', 'gg', 'pico8'];

export const isSystem = (id: unknown): id is SystemId => typeof id === 'string' && id in SYSTEMS;

/** Every extension the page takes, for the file picker: a picker knows a file's last extension only, so
 * PICO-8's .p8.png is .png there. */
export const ACCEPT = [...new Set(SYSTEM_IDS.flatMap((id) => SYSTEMS[id].extensions.map((ext) => `.${ext.replace(/^.*\./, '')}`)))].join(',');

/** A file's extension, lower case: PICO-8's .p8.png whole, else the last. */
export function extension(name: string): string {
  const lower = name.toLowerCase();
  if (lower.endsWith('.p8.png')) return 'p8.png';
  const dot = lower.lastIndexOf('.');
  return dot < 0 ? '' : lower.slice(dot + 1);
}

/** The console a file is a game for: by its header where it has one, else by its extension; null if
 * neither says. A Game Boy game made for the Color (alone or as well) is the Color's: the core runs it as
 * one. */
export function detect(name: string, rom: Uint8Array): SystemId | null {
  const ext = extension(name);
  if (ext === 'p8' || ext === 'p8.png') return 'pico8';
  if (ines(rom)) return 'nes';
  const sega = sega8(ext, rom);
  if (sega) return sega;
  if (megaDrive(rom)) return 'md';
  if (gbaHeader(rom)) return 'gba';
  const color = (rom[0x143] ?? 0) & 0x80 ? 'gbc' : 'gb';
  if (gbHeader(rom) || ['gb', 'sgb', 'gbc', 'cgb'].includes(ext)) return color;
  if (ext === 'gba') return 'gba';
  return null;
}

/** iNES and NES 2.0: "NES" and an end-of-file byte. */
function ines(rom: Uint8Array): boolean {
  return rom[0] === 0x4e && rom[1] === 0x45 && rom[2] === 0x53 && rom[3] === 0x1a;
}

/** "SEGA" at 0x100, byte-swapped ("ESAG"), or a copier's SMD image (512 bytes, then 16 KB blocks). */
function megaDrive(rom: Uint8Array): boolean {
  const at = (text: string) => [...text].every((c, i) => rom[0x100 + i] === c.charCodeAt(0));
  if (at('SEGA') || at('ESAG')) return true;
  return rom.length % 0x4000 === 512 && rom[8] === 0xaa && rom[9] === 0xbb;
}

/** The two consoles share a cartridge format. The extension selects the machine (a .gg can contain
 * a Master System-mode game). For an unlabelled dump, use the region nibble of its TMR SEGA header. */
function sega8(ext: string, rom: Uint8Array): 'sms' | 'gg' | null {
  const offset = rom.length > 512 && rom.length % 0x4000 === 512 ? 512 : 0;
  const size = rom.length - offset;
  if (size === 0 || size > 0x400000) return null;
  if (ext === 'sms' || ext === 'gg') return ext;
  for (const end of [0x8000, 0x4000, 0x2000]) {
    if (size < end) continue;
    const at = offset + end - 16;
    if (!Array.from('TMR SEGA').every((ch, i) => rom[at + i] === ch.charCodeAt(0))) continue;
    const code = rom[at + 15]! >>> 4;
    if (code === 3 || code === 4) return 'sms';
    if (code >= 5 && code <= 7) return 'gg';
  }
  return null;
}

/** A GBA cartridge's header: the fixed value 0x96 at 0xb2, and a checksum over 0xa0-0xbc. */
function gbaHeader(rom: Uint8Array): boolean {
  if (rom.length < 0xc0 || rom[0xb2] !== 0x96) return false;
  let sum = 0;
  for (let i = 0xa0; i <= 0xbc; i++) sum = (sum - rom[i]!) & 0xff;
  return ((sum - 0x19) & 0xff) === rom[0xbd];
}

/** A Game Boy cartridge's header: the start of the Nintendo logo at 0x104, and its checksum at 0x14d. */
function gbHeader(rom: Uint8Array): boolean {
  if (rom.length < 0x150) return false;
  const logo = [0xce, 0xed, 0x66, 0x66, 0xcc, 0x0d, 0x00, 0x0b];
  if (!logo.every((b, i) => rom[0x104 + i] === b)) return false;
  let sum = 0;
  for (let i = 0x134; i <= 0x14c; i++) sum = (sum - rom[i]! - 1) & 0xff;
  return sum === rom[0x14d];
}

/** A game's name from its file's: without the extension, and the bracketed tags of a No-Intro name
 * ("(USA)", "[!]") apart, for the card's second line. */
export function nameOf(file: string): { name: string; tags: string } {
  const ext = extension(file);
  return titleParts((ext ? file.slice(0, file.length - ext.length - 1) : file).replace(/_/g, ' '));
}

/** Split a display title and its release tags after any file extension has been removed. */
export function titleParts(base: string): { name: string; tags: string } {
  const at = base.search(/\s*[([]/);
  if (at <= 0) return { name: base.trim(), tags: '' };
  return { name: base.slice(0, at).trim(), tags: base.slice(at).trim() };
}
