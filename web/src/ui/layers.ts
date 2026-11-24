// Who gets the console's actions: the top layer (a window over a game's page over a screen). A layer may
// take an action itself; what it leaves goes the default way: the directions move the focus within it,
// A clicks what has the focus. When a layer goes, the focus goes back to where it was before it came.

import { type Action, type Dir, focus, focusFirst, move } from './nav.ts';

export type Handler = (action: Action) => boolean | void;

interface Layer {
  el: HTMLElement;
  handle: () => Handler | undefined;
  before: Element | null;
}

const stack: Layer[] = [];
const DIRS = new Set<Action>(['up', 'down', 'left', 'right']);

export function dispatch(action: Action): void {
  const top = stack.at(-1);
  if (!top) return;
  if (top.handle()?.(action) === true) return;
  if (DIRS.has(action)) {
    move(top.el, action as Dir);
  } else if (action === 'a') {
    const el = document.activeElement;
    if (el instanceof HTMLElement && top.el.contains(el) && el !== top.el) el.click();
    else focusFirst(top.el);
  }
}

/** The top layer's element, for what must stay inside it. */
export function topLayer(): HTMLElement | null {
  return stack.at(-1)?.el ?? null;
}

/** A Svelte action: the element is a layer for as long as it is on the page. */
export function layer(el: HTMLElement, handle?: Handler) {
  let current = handle;
  const entry: Layer = { el, handle: () => current, before: document.activeElement };
  stack.push(entry);
  return {
    update(next?: Handler) {
      current = next;
    },
    destroy() {
      const at = stack.indexOf(entry);
      if (at >= 0) stack.splice(at, 1);
      const before = entry.before;
      if (at === stack.length && before instanceof HTMLElement && before.isConnected && stack.at(-1)?.el.contains(before)) {
        focus(before);
      }
    },
  };
}
