// The page's one loop: every animation frame the pads and the keyboard are read once, and go either to
// the game, while one runs with its menu shut, or to the menus as the console's actions (layers.ts), with
// held directions repeating.

import { Input, type Poll } from '../engine/input.ts';
import { dispatch } from './layers.ts';
import { PadRepeat } from './nav.ts';
import { view } from './state/view.svelte.ts';

export const input = new Input();

type Game = (now: number, poll: Poll) => void;
let game: Game | null = null;

/** Gives the pads and the keyboard to a game, or back to the menus with null. */
export function giveTo(next: Game | null): void {
  game = next;
}

/** Whether a game has the buttons (its menu shut). */
export const gameHasInput = () => game !== null;

export function startPump(): () => void {
  const repeat = new PadRepeat();
  let frame = 0;
  const loop = (now: number) => {
    const poll = input.poll();
    if (poll.connected !== view.pads) view.pads = poll.connected;
    if (game) {
      game(now, poll);
    } else {
      const actions = repeat.actions(poll.pressed, poll.held, now);
      if (actions.length) {
        view.device = 'pad';
        for (const action of actions) dispatch(action);
      }
    }
    frame = requestAnimationFrame(loop);
  };
  frame = requestAnimationFrame(loop);
  return () => cancelAnimationFrame(frame);
}
