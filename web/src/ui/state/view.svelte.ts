// What the page shows: the section, the library's shelf, a game's page over them, the first run's step,
// the hue behind it all, and what the player last used (the hints show its keys or buttons).

import type { SystemId } from '../../engine/systems.ts';

export type Section = 'home' | 'library' | 'settings';
export const SECTIONS: readonly Section[] = ['home', 'library', 'settings'];
/** A shelf of the library: every game, the favourites, or one console's. */
export type Shelf = 'all' | 'fav' | SystemId;
export type SetupStep = 'lang' | 'profile' | 'games';
/** What the player last used: the keyboard, a pad or a touch screen. */
export type Device = 'keys' | 'pad' | 'touch';

export const view = $state({
  section: 'home' as Section,
  shelf: 'all' as Shelf,
  /** The game whose page is open over the section. */
  sheet: null as string | null,
  /** The first run, and its step; null once there is a profile. */
  setup: null as SetupStep | null,
  /** The hue behind the screen: the console in focus. */
  hue: '',
  device: 'keys' as Device,
  /** Pads connected. */
  pads: 0,
  fullscreen: false,
});

export function go(section: Section): void {
  view.sheet = null;
  view.section = section;
}

export function openShelf(shelf: Shelf): void {
  view.shelf = shelf;
  go('library');
}
