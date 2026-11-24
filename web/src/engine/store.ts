// The players' games and saves, in the browser's IndexedDB: nothing leaves the browser. A game is known
// by its file's SHA-1, so the same file added twice is one game, and its saves follow it.
//
// The games are everyone's and the rest is a profile's: its saves, Quick Resume, favourites and what it
// played last. A profile's records are keyed "<profile>/<game>".
//
//   profiles  the people who play in this browser: an id from 1 and a name
//   games     what the library shows of a game: name, console, size, when it was added
//   roms      the game's file, apart, so that the library loads without the games' bytes
//   progress  a profile's game: when played, the last frame as a picture, Quick Resume, slot, favourite
//   saves     the cartridge's own save (battery RAM, flash, EEPROM, PICO-8's cartdata)
//   resume    the whole machine when the player left, for Quick Resume
//   slots     the save state the player made in the game menu
//
// Every write is one transaction, and a promise settles only when it has committed or aborted: a
// request's success comes before the commit, and only the commit means the data is kept.

import type { Saved } from './protocol.ts';
import { detect, extension, isSystem, nameOf, type SystemId } from './systems.ts';

/** A game as everyone sees it. */
export interface Title {
  id: string;
  name: string;
  tags: string;
  system: SystemId;
  size: number;
  added: number;
  /** Its own picture, where its file has one: a PICO-8 cartridge saved as a PNG is its label. */
  cover?: Blob | null;
}

/** A game as one profile has it. */
export interface Progress {
  played: number;
  shot: Blob | null;
  /** Whether a Quick Resume state waits, and a saved slot. */
  resume: boolean;
  slot: boolean;
  fav: boolean;
}

/** A game with the current profile's progress. */
export type Game = Title & Progress;

export interface Profile {
  id: number;
  name: string;
}

/** The profiles a browser holds at most, and a name's length, cut as it is typed. */
export const PROFILES_MAX = 8;
export const NAME_MAX = 24;

type Store = 'profiles' | 'games' | 'roms' | 'progress' | 'saves' | 'resume' | 'slots';
const STORES: readonly Store[] = ['profiles', 'games', 'roms', 'progress', 'saves', 'resume', 'slots'];
/** The stores whose records are a profile's. */
const OWN: readonly Store[] = ['progress', 'saves', 'resume', 'slots'];

const NO_PROGRESS: Progress = { played: 0, shot: null, resume: false, slot: false, fav: false };

let opening: Promise<IDBDatabase> | null = null;
let profile = 1;

/** The profile whose saves the calls below read and write. */
export function useProfile(id: number): void {
  profile = id;
}

export function currentProfile(): number {
  return profile;
}

const own = (id: string, of = profile) => `${of}/${id}`;
/** Every key of one profile in a store. */
const ownRange = (of: number) => IDBKeyRange.bound(`${of}/`, `${of}/￿`);

function db(): Promise<IDBDatabase> {
  opening ??= new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open('sol', 2);
    request.onupgradeneeded = (event) => {
      const base = request.result;
      // Version 1 kept saves by game alone, with no profile: the games stay, their saves start anew.
      if (event.oldVersion < 2) {
        for (const store of ['saves', 'resume', 'slots']) if (base.objectStoreNames.contains(store)) base.deleteObjectStore(store);
      }
      for (const store of STORES) {
        if (!base.objectStoreNames.contains(store)) {
          base.createObjectStore(store, store === 'games' || store === 'profiles' ? { keyPath: 'id' } : {});
        }
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  }).catch((error: unknown) => {
    // A failed open is tried again by the next call, not kept.
    opening = null;
    throw error;
  });
  return opening;
}

function done<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function get<T>(store: Store, key: IDBValidKey): Promise<T | undefined> {
  return done((await db()).transaction(store).objectStore(store).get(key) as IDBRequest<T | undefined>);
}

/** One transaction over the stores, settled by its commit or its abort. */
async function write(stores: readonly Store[], update: (tx: IDBTransaction) => void): Promise<void> {
  const tx = (await db()).transaction(stores, 'readwrite');
  const committed = new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onabort = () => reject(tx.error ?? new DOMException('Storage transaction aborted', 'AbortError'));
  });
  try {
    update(tx);
  } catch (error) {
    tx.abort();
    await committed.catch(() => {});
    throw error;
  }
  await committed;
}

const put = (store: Store, value: unknown, key?: IDBValidKey) => write([store], (tx) => void tx.objectStore(store).put(value, key));

/** A game's record split into what is everyone's and what is the profile's. */
function progressOf({ played, shot, resume, slot, fav }: Game): Progress {
  return { played, shot, resume, slot, fav };
}

function merge(title: Title, progress: Progress | undefined): Game {
  return { cover: null, ...title, ...NO_PROGRESS, ...progress };
}

/** Every game with the current profile's progress, the last played or added first. A game for a
 * console this page does not play is left out: it could not be opened. */
export async function games(): Promise<Game[]> {
  const tx = (await db()).transaction(['games', 'progress']);
  const range = ownRange(profile);
  const [titles, keys, values] = await Promise.all([
    done(tx.objectStore('games').getAll() as IDBRequest<Title[]>),
    done(tx.objectStore('progress').getAllKeys(range)),
    done(tx.objectStore('progress').getAll(range) as IDBRequest<Progress[]>),
  ]);
  const prefix = `${profile}/`.length;
  const progress = new Map(keys.map((key, i) => [String(key).slice(prefix), values[i]]));
  return titles
    .filter((title) => isSystem(title.system))
    .map((title) => merge(title, progress.get(title.id)))
    .sort((a, b) => Math.max(b.played, b.added) - Math.max(a.played, a.added));
}

export async function game(id: string): Promise<Game | undefined> {
  const tx = (await db()).transaction(['games', 'progress']);
  const [title, progress] = await Promise.all([
    done(tx.objectStore('games').get(id) as IDBRequest<Title | undefined>),
    done(tx.objectStore('progress').get(own(id)) as IDBRequest<Progress | undefined>),
  ]);
  return title && isSystem(title.system) ? merge(title, progress) : undefined;
}

/** Keeps the profile's side of a game (when played, its picture, its flags). */
export async function saveGame(record: Game): Promise<void> {
  await put('progress', progressOf(record), own(record.id));
}

/** Adds a game from a file, and whether it is new (the same file added before is the game already
 * there); throws when the file is no game the page plays. The file and its record go in together. */
export async function add(file: File): Promise<{ game: Game; added: boolean }> {
  const rom = new Uint8Array(await file.arrayBuffer());
  const system = detect(file.name, rom);
  if (!system) throw new Error('unknown');
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-1', rom));
  const id = [...digest].map((b) => b.toString(16).padStart(2, '0')).join('');
  const known = await game(id);
  if (known) return { game: known, added: false };
  const cover = extension(file.name) === 'p8.png' ? new Blob([rom], { type: 'image/png' }) : null;
  const title: Title = { id, ...nameOf(file.name), system, size: rom.length, added: Date.now(), cover };
  await write(['roms', 'games'], (tx) => {
    tx.objectStore('roms').put(rom, id);
    tx.objectStore('games').put(title);
  });
  return { game: merge(title, undefined), added: true };
}

/** Removes a game for everyone: its file, and every profile's saves of it. */
export async function remove(id: string): Promise<void> {
  const ids = new Set([...(await profiles()).map((p) => p.id), profile]);
  await write(['games', 'roms', ...OWN], (tx) => {
    tx.objectStore('games').delete(id);
    tx.objectStore('roms').delete(id);
    for (const store of OWN) for (const of of ids) tx.objectStore(store).delete(own(id, of));
  });
}

/** One snapshot of a game being played: the cartridge's save if it changed, the machine's state for
 * Quick Resume or the slot, and the library's flags, all kept or none. */
export async function checkpoint(record: Game, saved: Saved, stateStore: 'resume' | 'slots' | null): Promise<void> {
  if (stateStore && !saved.state) throw new Error('The machine returned no state');
  if (!stateStore && !saved.sram) return;
  const stores: Store[] = ['progress'];
  if (saved.sram) stores.push('saves');
  if (stateStore) stores.push(stateStore);
  await write(stores, (tx) => {
    if (saved.sram) tx.objectStore('saves').put(saved.sram, own(record.id));
    if (stateStore) tx.objectStore(stateStore).put(saved.state, own(record.id));
    tx.objectStore('progress').put(progressOf(record), own(record.id));
  });
}

export const rom = (id: string) => get<Uint8Array>('roms', id);
export const sram = (id: string) => get<Uint8Array>('saves', own(id));
export const putSram = (id: string, data: Uint8Array) => put('saves', data, own(id));
export const resumeState = (id: string) => get<Uint8Array>('resume', own(id));
export const slot = (id: string) => get<Uint8Array>('slots', own(id));

/** Drops a game's Quick Resume, so that it starts from its title screen. */
export async function dropResumeState(id: string): Promise<void> {
  const record = await game(id);
  await write(['resume', 'progress'], (tx) => {
    tx.objectStore('resume').delete(own(id));
    if (record) tx.objectStore('progress').put({ ...progressOf(record), resume: false }, own(id));
  });
}

/** Replaces a game's cartridge save with one the player brought, and drops its Quick Resume, whose
 * machine state holds the old save and would put it back. */
export async function replaceSram(id: string, data: Uint8Array): Promise<void> {
  const record = await game(id);
  await write(['saves', 'resume', 'progress'], (tx) => {
    tx.objectStore('saves').put(data, own(id));
    tx.objectStore('resume').delete(own(id));
    if (record) tx.objectStore('progress').put({ ...progressOf(record), resume: false }, own(id));
  });
}

export async function profiles(): Promise<Profile[]> {
  const all = await done((await db()).transaction('profiles').objectStore('profiles').getAll() as IDBRequest<Profile[]>);
  return all.sort((a, b) => a.id - b.id);
}

/** A new profile, with the next id after the highest, so that an id is not used again while a higher
 * one exists. */
export async function addProfile(name: string): Promise<Profile> {
  const all = await profiles();
  if (all.length >= PROFILES_MAX) throw new Error('too many profiles');
  const record = { id: all.reduce((most, p) => Math.max(most, p.id), 0) + 1, name: name.trim().slice(0, NAME_MAX) };
  await put('profiles', record);
  return record;
}

export async function renameProfile(id: number, name: string): Promise<void> {
  await put('profiles', { id, name: name.trim().slice(0, NAME_MAX) });
}

/** Removes a profile with all its saves, Quick Resume states, favourites and frames. */
export async function removeProfile(id: number): Promise<void> {
  await write(['profiles', ...OWN], (tx) => {
    tx.objectStore('profiles').delete(id);
    for (const store of OWN) tx.objectStore(store).delete(ownRange(id));
  });
}

/** Removes every game and every save, keeping the profiles. */
export async function removeAllGames(): Promise<void> {
  const stores: Store[] = ['games', 'roms', ...OWN];
  await write(stores, (tx) => {
    for (const store of stores) tx.objectStore(store).clear();
  });
}

/** Removes the whole database: games, saves and profiles. */
export async function wipe(): Promise<void> {
  const base = await opening?.catch(() => null);
  base?.close();
  opening = null;
  await new Promise<void>((resolve, reject) => {
    const request = indexedDB.deleteDatabase('sol');
    request.onsuccess = () => resolve();
    request.onerror = () => reject(request.error);
    request.onblocked = () => resolve();
  });
}

/** Asks the browser to keep the games when it runs short of room, rather than clear them first. */
export async function persist(): Promise<boolean> {
  try {
    return (await navigator.storage?.persist?.()) ?? false;
  } catch {
    return false;
  }
}

/** Whether the browser has promised to keep the data. */
export async function persisted(): Promise<boolean> {
  try {
    return (await navigator.storage?.persisted?.()) ?? false;
  } catch {
    return false;
  }
}

/** Space used and the browser's limit for this page, in bytes; null where the browser does not say. */
export async function space(): Promise<{ used: number; quota: number } | null> {
  try {
    const estimate = await navigator.storage?.estimate?.();
    return estimate ? { used: estimate.usage ?? 0, quota: estimate.quota ?? 0 } : null;
  } catch {
    return null;
  }
}
