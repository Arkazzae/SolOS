// Starting a game and coming back from it. The game itself, its session and its menu, is the Player
// (ui/screens/Player.svelte), shown while `play.game` is set.

import { Speaker } from '../../engine/sound.ts';
import * as store from '../../engine/store.ts';
import { t } from '../../lib/text.svelte.ts';
import { focus, focusFirst } from '../nav.ts';
import { topLayer } from '../layers.ts';
import { gameById, refresh } from './library.svelte.ts';
import { ask, toast } from './notify.svelte.ts';
import { closeRoom, online } from './online.svelte.ts';
import { view } from './view.svelte.ts';

export const play = $state({
  /** The game being played, or being started. */
  game: null as store.Game | null,
  /** A game is being started: what the player presses waits. */
  opening: false,
});

/** The sound, made at the first game: a page may only start sound in the turn of a click or a key. */
let speaker: Promise<Speaker> | null = null;
/** What had the focus when the game started: it gets it back when the game ends. */
let before: Element | null = null;

/** Starts a game: from its Quick Resume when one waits and Settings keep them. Called from a click or a
 * key, whose turn the sound must start in. A second call while one is starting does nothing. */
export async function startGame(id: string): Promise<void> {
  if (play.game || play.opening) return;
  play.opening = true;
  before = document.activeElement;
  try {
    const sound = (speaker ??= Speaker.start().catch((error: unknown) => {
      speaker = null;
      throw error;
    }));
    const [game] = await Promise.all([store.game(id), sound]);
    if (game) {
      view.sheet = null;
      // The Player takes it from here, and says when it is running (opened).
      play.game = game;
      return;
    }
  } catch (error) {
    toast(t.openFailed(error instanceof Error ? error.message : String(error)), 'warn', undefined, 'danger');
  }
  play.opening = false;
}

/** The sound the Player plays through (startGame made it). */
export function sound(): Promise<Speaker> {
  return speaker ?? Promise.reject(new Error('no sound'));
}

/** The Player has the game running, or could not start it. */
export function opened(): void {
  play.opening = false;
}

/** The game ended (left, or never started): the library comes back, its card in focus. */
export async function endGame(): Promise<void> {
  const id = play.game?.id;
  if (online.room) closeRoom();
  play.game = null;
  play.opening = false;
  await refresh();
  // The card the game was started from, found again by its game (the list was drawn anew).
  const again = id ? document.querySelector<HTMLElement>(`[data-game="${id}"]`) : null;
  const target = again ?? (before instanceof HTMLElement && before.isConnected ? before : null);
  const scope = topLayer();
  if (target && scope?.contains(target)) focus(target);
  else if (scope) focusFirst(scope);
}

/** Starts a game over: its Quick Resume goes first, the cartridge's save stays. */
export async function startOver(id: string): Promise<void> {
  const game = gameById(id);
  if (!game) return;
  if (game.resume && !(await ask(t.startOverAsk(game.name), t.startOverLine, t.startOver, false))) return;
  await store.dropResumeState(id);
  await refresh();
  await startGame(id);
}
