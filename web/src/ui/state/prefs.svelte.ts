// The player's settings, in the browser's localStorage: small, read at the start. Games and saves are in
// IndexedDB (engine/store.ts). Storage may be off (a private window): the defaults then hold, until the
// page closes.

import { isShader, type ShaderMode } from '../../engine/shaders.ts';
import { isLang, type Lang, setLang } from '../../lib/text.svelte.ts';

export const WALLPAPERS = ['wave', 'arcade', 'orbit'] as const;
export const ACCENTS = ['sol', 'mint', 'blue', 'violet', 'rose'] as const;
export const UI_FONTS = ['figtree', 'system', 'mono'] as const;
export const UI_SCALES = [90, 100, 110] as const;

interface Appearance {
  wallpaper: (typeof WALLPAPERS)[number];
  animateWallpaper: boolean;
  animateOrbit: boolean;
  accent: (typeof ACCENTS)[number];
  uiFont: (typeof UI_FONTS)[number];
  uiScale: (typeof UI_SCALES)[number];
}

export const DEFAULT_APPEARANCE: Appearance = {
  wallpaper: 'wave', animateWallpaper: false, animateOrbit: true, accent: 'sol', uiFont: 'figtree', uiScale: 100,
};

export interface Prefs extends Appearance {
  lang: Lang;
  shader: ShaderMode;
  /** The performance counter in a game's corner. */
  perf: boolean;
  quickResume: boolean;
  /** The current profile's id; 0 before the first run made one. */
  profile: number;
}

const KEY = 'sol-prefs';
const DEFAULTS: Prefs = { ...DEFAULT_APPEARANCE, lang: 'en', shader: 'clean', perf: false, quickResume: true, profile: 0 };

function savedChoice<T>(choices: readonly T[], saved: unknown, fallback: T): T {
  return choices.find((choice) => choice === saved) ?? fallback;
}

function load(): Prefs {
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) ?? '{}') as Partial<Record<keyof Prefs, unknown>>;
    return {
      lang: isLang(saved.lang) ? saved.lang : DEFAULTS.lang,
      shader: isShader(saved.shader) ? saved.shader : DEFAULTS.shader,
      perf: typeof saved.perf === 'boolean' ? saved.perf : DEFAULTS.perf,
      quickResume: typeof saved.quickResume === 'boolean' ? saved.quickResume : DEFAULTS.quickResume,
      profile: Number.isInteger(saved.profile) ? (saved.profile as number) : DEFAULTS.profile,
      wallpaper: savedChoice(WALLPAPERS, saved.wallpaper, DEFAULTS.wallpaper),
      animateWallpaper: typeof saved.animateWallpaper === 'boolean' ? saved.animateWallpaper : DEFAULTS.animateWallpaper,
      animateOrbit: typeof saved.animateOrbit === 'boolean' ? saved.animateOrbit : DEFAULTS.animateOrbit,
      accent: savedChoice(ACCENTS, saved.accent, DEFAULTS.accent),
      uiFont: savedChoice(UI_FONTS, saved.uiFont, DEFAULTS.uiFont),
      uiScale: savedChoice(UI_SCALES, saved.uiScale, DEFAULTS.uiScale),
    };
  } catch {
    return { ...DEFAULTS };
  }
}

export const prefs: Prefs = $state(load());
setLang(prefs.lang);

export function wallpaperMoves(): boolean {
  return prefs.wallpaper === 'orbit' ? prefs.animateOrbit : prefs.animateWallpaper;
}

export function setWallpaperMotion(on: boolean): void {
  setPrefs(prefs.wallpaper === 'orbit' ? { animateOrbit: on } : { animateWallpaper: on });
}

export function setPrefs(change: Partial<Prefs>): void {
  Object.assign(prefs, change);
  if (change.lang) setLang(change.lang);
  try {
    localStorage.setItem(KEY, JSON.stringify($state.snapshot(prefs)));
  } catch {
    // Storage off: the settings last until the page closes.
  }
}

export function clearPrefs(): void {
  try {
    localStorage.removeItem(KEY);
  } catch {
    // Nothing kept.
  }
}
