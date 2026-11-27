// How the page shows each console: its hue, its epic name, its picture. The same identity as the
// console's own interface; the page names a console by its picture and its name, never by its logo.

import type { SystemId } from '../engine/systems.ts';
import { lang } from '../lib/text.svelte.ts';

interface Identity {
  /** The console's colour: the wave behind the screen, the ground of a drawn cover and of its card. */
  hue: string;
  epic: { en: string; pl: string };
}

const IDENTITY: Readonly<Record<SystemId, Identity>> = {
  nes: {
    hue: '#964944',
    epic: { en: 'The 8-Bit Pioneer', pl: 'Ośmiobitowy Pionier' },
  },
  md: {
    hue: '#116b98',
    epic: { en: 'Blue Lightning', pl: 'Błękitna Błyskawica' },
  },
  sms: {
    hue: '#9a5a2e',
    epic: { en: 'The Arcade in the Living Room', pl: 'Automat w Salonie' },
  },
  gg: {
    hue: '#3a7627',
    epic: { en: 'The Pocket Rival', pl: 'Kieszonkowy Rywal' },
  },
  gb: {
    hue: '#65690f',
    epic: { en: 'The Green Veteran', pl: 'Zielony Weteran' },
  },
  gbc: {
    hue: '#047270',
    epic: { en: 'Pocket Rainbow', pl: 'Kieszonkowa Tęcza' },
  },
  gba: {
    hue: '#4f5e9f',
    epic: { en: 'Pocket Titan', pl: 'Kieszonkowy Tytan' },
  },
  pico8: {
    hue: '#8f4869',
    epic: { en: 'The Fantasy Console', pl: 'Fantazyjna Konsola' },
  },
};

/** The hues of the shelves that are not a console (every game, favourites), and the wave's on
 * Settings and before any game. */
export const HUE_ALL = '#2f6f7a';
export const HUE_FAV = '#9a6a1f';
export const HUE_NONE = '#1f6f7a';

export const hue = (id: SystemId) => IDENTITY[id].hue;
export const epic = (id: SystemId) => IDENTITY[id].epic[lang()];
/** The console's picture (public/platforms, a transparent WebP); 'all' for every game's shelf. */
export const art = (id: SystemId | 'all') => `${import.meta.env.BASE_URL}platforms/${id}.webp`;
