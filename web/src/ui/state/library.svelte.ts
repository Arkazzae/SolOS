// The library of the current profile and the profiles themselves, as the screens show them, and what
// the screens do with them. Every change goes to the database first (engine/store.ts) and the library is
// read again from it, so what is shown is what is kept.

import * as store from '../../engine/store.ts';
import { ACCEPT, SYSTEM_IDS } from '../../engine/systems.ts';
import { t } from '../../lib/text.svelte.ts';
import { ask, askName, toast } from './notify.svelte.ts';
import { clearPrefs, prefs, setPrefs } from './prefs.svelte.ts';
import { type Shelf, view } from './view.svelte.ts';

export const library = $state({
  /** The library and the profiles have been read. */
  ready: false,
  games: [] as store.Game[],
  profiles: [] as store.Profile[],
});

export const profileName = (p: store.Profile | undefined) => (p ? p.name || t.profileN(p.id) : '');
export const currentProfile = () => library.profiles.find((p) => p.id === prefs.profile);
export const gameById = (id: string | null) => (id ? library.games.find((g) => g.id === id) : undefined);

/** Reads the profiles and the current one's library; the first run when there is no profile. */
export async function start(): Promise<void> {
  try {
    library.profiles = await store.profiles();
    const chosen = currentProfile() ?? library.profiles[0];
    if (chosen) {
      store.useProfile(chosen.id);
      if (chosen.id !== prefs.profile) setPrefs({ profile: chosen.id });
      await refresh();
    } else {
      view.setup = 'lang';
    }
  } catch (error) {
    console.error('the library could not be read:', error);
    toast(t.openFailed(String(error)), 'warn', undefined, 'danger');
  }
  library.ready = true;
}

export async function refresh(): Promise<void> {
  library.games = await store.games();
}

/** The first run's profile, made with its name. */
export async function makeFirstProfile(name: string): Promise<void> {
  const profile = await store.addProfile(name);
  library.profiles = await store.profiles();
  store.useProfile(profile.id);
  setPrefs({ profile: profile.id });
  await refresh();
}

/** Adds files to the library, one by one, saying what came of them; how many games are new. */
export async function addFiles(files: File[]): Promise<number> {
  let added = 0;
  let last = '';
  for (const file of files) {
    try {
      const result = await store.add(file);
      if (result.added) {
        added++;
        last = result.game.name;
      } else if (files.length === 1) {
        toast(t.known(result.game.name), 'cards');
      }
    } catch {
      toast(t.notGame(file.name), 'warn', t.supported, 'accent');
    }
  }
  if (added === 1) toast(t.added(last), 'check');
  else if (added > 1) toast(t.addedMany(added), 'check');
  if (added > 0) void store.persist();
  await refresh();
  return added;
}

/** A file picker, made once. */
let picker: HTMLInputElement | null = null;
let picked: ((files: File[]) => void) | null = null;

export function pickFiles(accept: string, multiple = true): Promise<File[]> {
  if (!picker) {
    const input = (picker = Object.assign(document.createElement('input'), { type: 'file', hidden: true }));
    const answer = (files: File[]) => {
      picked?.(files);
      picked = null;
      input.value = '';
    };
    input.addEventListener('change', () => answer([...(input.files ?? [])]));
    input.addEventListener('cancel', () => answer([]));
    document.body.append(input);
  }
  picker.accept = accept;
  picker.multiple = multiple;
  return new Promise((resolve) => {
    picked = resolve;
    picker!.click();
  });
}

/** Games from the file picker into the library; how many are new. */
export async function pickGames(): Promise<number> {
  const files = await pickFiles(ACCEPT);
  return files.length ? addFiles(files) : 0;
}

export async function toggleFav(id: string): Promise<void> {
  const game = gameById(id);
  if (!game) return;
  await store.saveGame({ ...$state.snapshot(game), fav: !game.fav });
  toast(game.fav ? t.unstarred(game.name) : t.starred(game.name), game.fav ? 'starLine' : 'star');
  await refresh();
}

export async function removeGame(id: string): Promise<void> {
  const game = gameById(id);
  if (!game || !(await ask(t.removeAsk(game.name), t.removeAskLine, t.remove))) return;
  await store.remove(id);
  view.sheet = null;
  toast(t.removed(game.name), 'trash');
  await refresh();
}

/** The cartridge's save as a .sav file, for other emulators. */
export async function downloadSave(id: string): Promise<void> {
  const game = gameById(id);
  const data = game && (await store.sram(id));
  if (!game || !data) {
    toast(t.noSave, 'info');
    return;
  }
  const link = document.createElement('a');
  link.href = URL.createObjectURL(new Blob([data as BlobPart], { type: 'application/octet-stream' }));
  link.download = `${game.name}${game.tags ? ` ${game.tags}` : ''}.sav`;
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 10_000);
}

/** A save from another emulator in place of the game's own; its Quick Resume goes with the old save. */
export async function uploadSave(id: string): Promise<void> {
  const game = gameById(id);
  if (!game) return;
  const [file] = await pickFiles('.sav,.srm,.sram,.eep,.fla', false);
  if (!file) return;
  if (!(await ask(t.uploadSaveAsk(game.name), t.uploadSaveAskLine, t.uploadSave, false))) return;
  await store.replaceSram(id, new Uint8Array(await file.arrayBuffer()));
  toast(t.saveUploaded, 'save');
  await refresh();
}

export async function switchProfile(id: number): Promise<void> {
  if (id === prefs.profile) return;
  store.useProfile(id);
  setPrefs({ profile: id });
  view.sheet = null;
  await refresh();
  toast(t.profileSwitched(profileName(currentProfile())), 'user');
}

export async function addProfile(): Promise<void> {
  if (library.profiles.length >= store.PROFILES_MAX) return;
  const name = await askName(t.newProfileName, '', t.addProfile);
  if (name === null) return;
  const profile = await store.addProfile(name);
  library.profiles = await store.profiles();
  await switchProfile(profile.id);
}

export async function renameProfile(): Promise<void> {
  const profile = currentProfile();
  if (!profile) return;
  const name = await askName(t.newName, profile.name, t.save);
  if (name === null) return;
  await store.renameProfile(profile.id, name);
  library.profiles = await store.profiles();
}

export async function removeProfile(): Promise<void> {
  const profile = currentProfile();
  if (!profile || library.profiles.length < 2) return;
  if (!(await ask(t.removeProfileAsk(profileName(profile)), t.removeProfileAskLine, t.removeProfile))) return;
  await store.removeProfile(profile.id);
  library.profiles = await store.profiles();
  const next = library.profiles[0];
  if (next) await switchProfile(next.id);
}

export async function removeAll(): Promise<void> {
  if (!(await ask(t.removeAllAsk, t.removeAllAskLine, t.remove))) return;
  await store.removeAllGames();
  toast(t.allRemoved, 'trash');
  await refresh();
}

/** Every game, save, profile and setting goes, and the page starts as new. The service worker's cache
 * is the page itself, not the player's: it stays, so Sol still opens offline afterwards. */
export async function clearAll(): Promise<void> {
  if (!(await ask(t.clearAllAsk, t.clearAllAskLine, t.clearAll))) return;
  await store.wipe();
  clearPrefs();
  location.reload();
}

const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });

/** The games on a shelf, by name, as the console's library has them. */
export function shelfGames(shelf: Shelf): store.Game[] {
  const list =
    shelf === 'all' ? library.games
    : shelf === 'fav' ? library.games.filter((g) => g.fav)
    : library.games.filter((g) => g.system === shelf);
  return [...list].sort((a, b) => collator.compare(a.name, b.name));
}

/** The consoles with games, in the console's order. */
export function consolesWithGames() {
  return SYSTEM_IDS.filter((id) => library.games.some((g) => g.system === id));
}

/** The shelves that have games: every game, the favourites once there is one, each console. */
export function shelves(): Shelf[] {
  return ['all', ...(library.games.some((g) => g.fav) ? ['fav' as const] : []), ...consolesWithGames()];
}

/** Days since a time, by the calendar. */
export function daysAgo(ms: number): number {
  const day = (d: Date) => Date.UTC(d.getFullYear(), d.getMonth(), d.getDate());
  return Math.round((day(new Date()) - day(new Date(ms))) / 86_400_000);
}
