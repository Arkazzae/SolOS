// Moving round the screens as on the console: a pad's d-pad and buttons, or the keyboard standing in for
// them, turned into the console's actions (A, B, X, Y, LB, RB, LT, RT, Menu and the four directions).
// The focus is the page's own (real focusable elements), so the mouse, touch and Tab work too; the
// directions move it to the nearest element that way, as a TV's spatial navigation does.

import { Pad } from '../engine/core.ts';

export type Dir = 'up' | 'down' | 'left' | 'right';
export type Action = Dir | 'a' | 'b' | 'x' | 'y' | 'lb' | 'rb' | 'lt' | 'rt' | 'menu';
/** What the player last used: the hints show its keys or buttons. */
export type Device = 'keys' | 'pad' | 'touch';

/** The keyboard in the menus (KeyboardEvent.code). In a game it is a pad instead (engine/input.ts). */
const KEYS: Readonly<Record<string, Action>> = {
  ArrowUp: 'up',
  ArrowDown: 'down',
  ArrowLeft: 'left',
  ArrowRight: 'right',
  Enter: 'a',
  NumpadEnter: 'a',
  Space: 'a',
  Escape: 'b',
  Backspace: 'b',
  KeyQ: 'lb',
  KeyE: 'rb',
  PageUp: 'lt',
  PageDown: 'rt',
  KeyF: 'x',
  KeyR: 'y',
  KeyI: 'menu',
};

/** In a text field only these are the menu's; the rest type. */
const FIELD_KEYS = new Set(['Enter', 'NumpadEnter', 'Escape', 'ArrowUp', 'ArrowDown']);

/** The key a hint shows for an action, and the pad's button. */
export const KEY_CAPS: Readonly<Record<Exclude<Action, Dir>, string>> = {
  a: 'Enter', b: 'Esc', x: 'F', y: 'R', lb: 'Q', rb: 'E', lt: 'PgUp', rt: 'PgDn', menu: 'I',
};
export const PAD_CAPS: Readonly<Record<Exclude<Action, Dir>, string>> = {
  a: 'A', b: 'B', x: 'X', y: 'Y', lb: 'LB', rb: 'RB', lt: 'LT', rt: 'RT', menu: '≡',
};

const PAD_ACTIONS: [number, Action][] = [
  [Pad.up, 'up'], [Pad.down, 'down'], [Pad.left, 'left'], [Pad.right, 'right'],
  [Pad.south, 'a'], [Pad.east, 'b'], [Pad.west, 'x'], [Pad.north, 'y'],
  [Pad.l1, 'lb'], [Pad.r1, 'rb'], [Pad.l2, 'lt'], [Pad.r2, 'rt'], [Pad.start, 'menu'], [Pad.home, 'menu'],
];
const DIRS = Pad.up | Pad.down | Pad.left | Pad.right;
/** A held direction repeats after this long, then this often (ms), as the console's menus. */
const REPEAT_AFTER = 380;
const REPEAT_EVERY = 110;

/** The keyboard's action for a key press, or null when it is not the menu's. */
export function keyAction(event: KeyboardEvent): Action | null {
  if (event.ctrlKey || event.metaKey || event.altKey) return null;
  const field = event.target instanceof HTMLElement && event.target.matches('input, textarea, select, [contenteditable]');
  if (field && !FIELD_KEYS.has(event.code)) return null;
  // A focused button or link takes Enter and Space itself: the click is its action.
  return KEYS[event.code] ?? null;
}

/** Turns a pad's buttons into actions, with held directions repeating. */
export class PadRepeat {
  private held = 0;
  private since = 0;
  private last = 0;

  /** The actions for this poll: buttons that went down, and a direction held long enough. */
  actions(pressed: number, down: number, now: number): Action[] {
    const out: Action[] = [];
    for (const [bit, action] of PAD_ACTIONS) if (pressed & bit) out.push(action);
    const dirs = down & DIRS;
    if (dirs !== this.held) {
      this.held = dirs;
      this.since = this.last = now;
    } else if (dirs && now - this.since > REPEAT_AFTER && now - this.last > REPEAT_EVERY) {
      this.last = now;
      for (const [bit, action] of PAD_ACTIONS.slice(0, 4)) if (dirs & bit) out.push(action);
    }
    return out;
  }
}

/** The elements the directions move between: marked data-nav, shown and not disabled. */
function targets(scope: Element): HTMLElement[] {
  return [...scope.querySelectorAll<HTMLElement>('[data-nav]')].filter((el) => {
    if ((el as HTMLButtonElement).disabled || el.closest('[inert]')) return false;
    const box = el.getBoundingClientRect();
    return box.width > 0 && box.height > 0;
  });
}

/** Moves the focus to the nearest element in a direction within scope; false when there is none. A
 * candidate must lie that way; nearer along the direction wins, and off the line counts double. Elements
 * sharing a data-row prefer each other for left and right, so a rail's cards go along the rail. */
export function move(scope: Element, dir: Dir): boolean {
  const all = targets(scope);
  const from = document.activeElement instanceof HTMLElement && all.includes(document.activeElement) ? document.activeElement : null;
  if (!from) return focusFirst(scope);
  const a = from.getBoundingClientRect();
  const ax = a.left + a.width / 2;
  const ay = a.top + a.height / 2;
  const row = from.dataset.row;
  let best: HTMLElement | null = null;
  let bestScore = Infinity;
  for (const el of all) {
    if (el === from) continue;
    const b = el.getBoundingClientRect();
    const bx = b.left + b.width / 2;
    const by = b.top + b.height / 2;
    let along: number;
    let across: number;
    switch (dir) {
      case 'left': along = a.left - b.right; across = Math.abs(by - ay); if (bx >= ax - 1) continue; break;
      case 'right': along = b.left - a.right; across = Math.abs(by - ay); if (bx <= ax + 1) continue; break;
      case 'up': along = a.top - b.bottom; across = Math.abs(bx - ax); if (by >= ay - 1) continue; break;
      case 'down': along = b.top - a.bottom; across = Math.abs(bx - ax); if (by <= ay + 1) continue; break;
    }
    let score = Math.max(0, along) + across * 2;
    if ((dir === 'left' || dir === 'right') && row !== undefined) score += el.dataset.row === row ? 0 : 100_000;
    if (score < bestScore) {
      bestScore = score;
      best = el;
    }
  }
  if (!best) return false;
  focus(best);
  return true;
}

/** Focuses the element marked data-first within scope, else the first one. */
export function focusFirst(scope: Element): boolean {
  const all = targets(scope);
  const first = all.find((el) => el.dataset.first !== undefined) ?? all[0];
  if (!first) return false;
  focus(first);
  return true;
}

/** Focuses an element and brings it into view, as the TV's rails slide to it. */
export function focus(el: HTMLElement): void {
  el.focus({ preventScroll: true });
  el.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
}

/** A hint along the bottom: the buttons (the last one is what a click does) and what they do. */
export interface Hint {
  actions: Exclude<Action, Dir>[];
  label: string;
}
