// The players' buttons: pads through the Gamepad API (its "standard" mapping is the Xbox pad's layout,
// which is how web/core takes them) and the keyboard as the first player's pad. One poll a frame, by
// the page's one loop (ui/pump.ts), for the game and the menus alike.

import { Pad } from './core.ts';

/** The keyboard's keys (KeyboardEvent.code) and the pad's buttons they are, in a game. */
const KEYS: Readonly<Record<string, number>> = {
  ArrowUp: Pad.up,
  ArrowDown: Pad.down,
  ArrowLeft: Pad.left,
  ArrowRight: Pad.right,
  KeyZ: Pad.south,
  KeyX: Pad.east,
  KeyA: Pad.west,
  KeyS: Pad.north,
  KeyQ: Pad.l1,
  KeyW: Pad.r1,
  Enter: Pad.start,
  ShiftLeft: Pad.select,
  ShiftRight: Pad.select,
};

/** The key a player presses for a pad's button in a game, as a key cap says it. */
export function keyFor(bit: number): string {
  const code = Object.keys(KEYS).find((key) => KEYS[key] === bit);
  return code ? code.replace(/^Key/, '').replace(/(Left|Right)$/, '') : '';
}

/** Fast forward while held: Tab, or a pad's RT, which none of the page's consoles has. */
const FAST_KEY = 'Tab';
/** How far a stick goes before it is a direction. */
const STICK = 0.5;

/** A pad's buttons held, as bits (bit n is button n), with its left stick as the d-pad. */
function padBits(pad: Gamepad): number {
  let bits = 0;
  pad.buttons.forEach((button, i) => {
    if (i < 17 && (button.pressed || button.value > 0.5)) bits |= 1 << i;
  });
  const [x = 0, y = 0] = pad.axes;
  if (x < -STICK) bits |= Pad.left;
  if (x > STICK) bits |= Pad.right;
  if (y < -STICK) bits |= Pad.up;
  if (y > STICK) bits |= Pad.down;
  return bits;
}

export interface Poll {
  /** Each player's buttons held, without fast forward's RT; the keyboard in the first when it plays. */
  pads: number[];
  /** Fast forward held: Tab, or RT on any pad. */
  fast: boolean;
  /** Buttons that went down since the last poll on any pad (not the keyboard): for menus. */
  pressed: number;
  /** Buttons held on any pad (not the keyboard). */
  held: number;
  /** How many pads are connected. */
  connected: number;
}

export class Input {
  /** Whether the keyboard plays: only while a game runs, never while a menu has it. */
  private playing = false;
  private keys = 0;
  private fastKey = false;
  private before = 0;

  constructor(target: EventTarget = window) {
    target.addEventListener('keydown', (event) => this.down(event as KeyboardEvent));
    target.addEventListener('keyup', (event) => this.up(event as KeyboardEvent));
    target.addEventListener('blur', () => this.release());
  }

  /** The keyboard is the first player's pad, or the menus'. */
  set keyboard(playing: boolean) {
    this.playing = playing;
    if (!playing) this.release();
  }

  get keyboard(): boolean {
    return this.playing;
  }

  /** Lets go of every key, as when a menu takes the keyboard. */
  release(): void {
    this.keys = 0;
    this.fastKey = false;
  }

  private down(event: KeyboardEvent): void {
    if (!this.playing || event.ctrlKey || event.metaKey || event.altKey) return;
    if (event.code === FAST_KEY) {
      this.fastKey = true;
    } else {
      const bit = KEYS[event.code];
      if (bit === undefined) return;
      this.keys |= bit;
    }
    // Not to the page: not Tab to the next button, nor Enter to a focused one.
    event.preventDefault();
  }

  private up(event: KeyboardEvent): void {
    if (event.code === FAST_KEY) this.fastKey = false;
    const bit = KEYS[event.code];
    if (bit !== undefined) this.keys &= ~bit;
  }

  poll(): Poll {
    const pads = navigator.getGamepads().filter((pad): pad is Gamepad => pad !== null && pad.connected);
    const held = pads.map(padBits);
    const any = held.reduce((all, bits) => all | bits, 0);
    const pressed = any & ~this.before;
    this.before = any;
    const players = [0, 1, 2, 3].map((i) => (held[i] ?? 0) & ~Pad.r2);
    if (this.playing) players[0]! |= this.keys;
    const fast = (this.playing && this.fastKey) || (any & Pad.r2) !== 0;
    return { pads: players, fast, pressed, held: any, connected: pads.length };
  }
}
