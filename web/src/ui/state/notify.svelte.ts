// Notifications and windows, as the console's: a toast for 3.5 s, three at most, top right; a window
// over everything that asks, takes a name or shows the keys, and answers through a promise.

import type { IconName } from '../../lib/icons.ts';

export interface Toast {
  id: number;
  icon: IconName;
  title: string;
  line?: string;
  tone?: 'accent' | 'danger';
}

/** The keys a window shows, in groups (a console's buttons, say) each under its title. */
export interface KeyGroup {
  title?: string;
  rows: (readonly [key: string, what: string])[];
}

export type Dialog =
  | { kind: 'ask'; title: string; line: string; confirm: string; danger: boolean; done(yes: boolean): void }
  | { kind: 'name'; title: string; value: string; confirm: string; done(name: string | null): void }
  | { kind: 'keys'; title: string; groups: KeyGroup[]; done(): void };

export const notices = $state({ toasts: [] as Toast[], dialog: null as Dialog | null });

const TOAST_MS = 3500;
let toastId = 0;

export function toast(title: string, icon: IconName = 'info', line?: string, tone?: Toast['tone']): void {
  const id = ++toastId;
  notices.toasts = [...notices.toasts.slice(-2), { id, icon, title, line, tone }];
  setTimeout(() => {
    notices.toasts = notices.toasts.filter((x) => x.id !== id);
  }, TOAST_MS);
}

/** Asks a question; true for the confirming answer. The safe one has the focus. */
export function ask(title: string, line: string, confirm: string, danger = true): Promise<boolean> {
  return new Promise((resolve) => {
    notices.dialog = { kind: 'ask', title, line, confirm, danger, done: (yes) => { notices.dialog = null; resolve(yes); } };
  });
}

/** Asks for a name; null when the player backs out. */
export function askName(title: string, value: string, confirm: string): Promise<string | null> {
  return new Promise((resolve) => {
    notices.dialog = { kind: 'name', title, value, confirm, done: (name) => { notices.dialog = null; resolve(name); } };
  });
}

export function showKeys(title: string, groups: KeyGroup[]): void {
  notices.dialog = { kind: 'keys', title, groups, done: () => { notices.dialog = null; } };
}
