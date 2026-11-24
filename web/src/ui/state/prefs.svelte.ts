// The player's settings, in the browser's localStorage: small, read at the start. Games and saves are in
// IndexedDB (engine/store.ts). Storage may be off (a private window): the defaults then hold, until the
// page closes.

import { isShader, type ShaderMode } from '../../engine/shaders.ts';
import { isLang, type Lang, setLang } from '../../lib/text.svelte.ts';

export interface Prefs {
  lang: Lang;
  shader: ShaderMode;
  /** The performance counter in a game's corner. */
  perf: boolean;
  quickResume: boolean;
  /** The current profile's id; 0 before the first run made one. */
  profile: number;
}

const KEY = 'sol-prefs';
const DEFAULTS: Prefs = { lang: 'en', shader: 'clean', perf: false, quickResume: true, profile: 0 };

function load(): Prefs {
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) ?? '{}') as Partial<Record<keyof Prefs, unknown>>;
    return {
      lang: isLang(saved.lang) ? saved.lang : DEFAULTS.lang,
      shader: isShader(saved.shader) ? saved.shader : DEFAULTS.shader,
      perf: typeof saved.perf === 'boolean' ? saved.perf : DEFAULTS.perf,
      quickResume: typeof saved.quickResume === 'boolean' ? saved.quickResume : DEFAULTS.quickResume,
      profile: Number.isInteger(saved.profile) ? (saved.profile as number) : DEFAULTS.profile,
    };
  } catch {
    return { ...DEFAULTS };
  }
}

export const prefs: Prefs = $state(load());
setLang(prefs.lang);

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
