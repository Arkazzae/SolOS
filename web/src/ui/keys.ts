// The keyboard's keys as the page lists them: in a game, where each console's buttons are (the keyboard
// is the first player's pad, engine/input.ts), and in the menus, standing in for a pad (nav.ts).

import { keyFor } from '../engine/input.ts';
import { type System, SYSTEM_IDS, SYSTEMS } from '../engine/systems.ts';
import { t } from '../lib/text.svelte.ts';
import { KEY_CAPS } from './nav.ts';
import type { KeyGroup } from './state/notify.svelte.ts';

type Row = readonly [key: string, what: string];

/** A console's buttons and the keys that press them. */
export function buttonKeys(system: System): Row[] {
  return system.buttons.map(([bit, label]) => [keyFor(bit), label] as const);
}

/** The keys every game has: the d-pad, the game menu, fast forward, full screen and the counter. */
export function commonGameKeys(): Row[] {
  return [
    ['← ↑ → ↓', t.dpad],
    ['Esc', t.menuKey],
    ['Tab', t.fastKey],
    ['F', t.fullscreenKey],
    ['`', t.statsKey],
  ];
}

/** Every console's keys, for Settings: the common ones, then the consoles' buttons, the consoles whose
 * buttons are the same keys under one title. */
export function allGameKeys(): KeyGroup[] {
  const groups = new Map<string, { names: string[]; rows: Row[] }>();
  for (const id of SYSTEM_IDS) {
    const rows = buttonKeys(SYSTEMS[id]);
    const key = JSON.stringify(rows);
    const group = groups.get(key) ?? { names: [], rows };
    group.names.push(SYSTEMS[id].name);
    groups.set(key, group);
  }
  return [
    { title: t.everyConsole, rows: commonGameKeys() },
    ...[...groups.values()].map(({ names, rows }) => ({ title: names.join(' · '), rows })),
  ];
}

/** The keys in the menus. */
export function menuKeys(): KeyGroup[] {
  return [{
    rows: [
      ['← ↑ → ↓', t.move],
      [`${KEY_CAPS.a} / Space`, t.confirmKey],
      [`${KEY_CAPS.b} / Backspace`, t.backKey],
      [`${KEY_CAPS.lb} ${KEY_CAPS.rb}`, t.sectionsKey],
      [`${KEY_CAPS.lt} ${KEY_CAPS.rt}`, t.letterKey],
      [KEY_CAPS.x, t.favouriteKey],
      [KEY_CAPS.y, t.startOverKey],
      [KEY_CAPS.menu, t.detailsKey],
    ],
  }];
}
