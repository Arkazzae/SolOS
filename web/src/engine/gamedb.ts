// Offline cartridge metadata. Each console has its own CRC-32 namespace.
import gg from '../data/gamedb/gg.json';
import sms from '../data/gamedb/sms.json';
import type { SystemId } from './systems.ts';

export interface CatalogEntry {
  title: string;
  year?: number;
  studio?: string;
  players?: number;
}

const catalogs: Record<'sms' | 'gg', Readonly<Record<string, CatalogEntry>>> = { sms, gg };

export function hasCatalog(system: SystemId): system is keyof typeof catalogs {
  return system === 'sms' || system === 'gg';
}

export function catalogEntry(system: SystemId, crc?: string): CatalogEntry | undefined {
  return crc && hasCatalog(system) ? catalogs[system][crc] : undefined;
}

// Reflected CRC-32 (the ZIP/No-Intro checksum).
const crcTable = Uint32Array.from({ length: 256 }, (_, byte) => {
  let crc = byte;
  for (let i = 0; i < 8; i++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  return crc;
});

function crc32(bytes: Uint8Array): string {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = (crc >>> 8) ^ crcTable[(crc ^ byte) & 0xff]!;
  return ((~crc) >>> 0).toString(16).toUpperCase().padStart(8, '0');
}

/** Prefer an exact dump. A copier can put a 512-byte prefix before the cartridge. */
export function cartridgeCrc(system: 'sms' | 'gg', rom: Uint8Array): string {
  const full = crc32(rom);
  if (rom.length > 512 && rom.length % 1024 === 512 && !catalogEntry(system, full)) {
    return crc32(rom.subarray(512));
  }
  return full;
}
