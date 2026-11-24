// The messages between the page and the machine's worker (worker.ts).

import type { ShaderMode } from './shaders.ts';

/** What the page asks; a request with an id gets a reply with the same id. */
export type ToWorker =
  | { kind: 'init'; canvas: OffscreenCanvas; sound: MessagePort; shader: ShaderMode }
  | { kind: 'open'; id: number; system: string; rom: Uint8Array; sram: Uint8Array | null; state: Uint8Array | null }
  /** Frames to run. With a budget (ms, fast forward), a frame after the first runs only while one as
   * long as the last still ends within it, and only the last frame's sound is sent. */
  | { kind: 'run'; frames: number; pads: number[]; speed: number; budget: number }
  | { kind: 'resize'; width: number; height: number }
  | { kind: 'shader'; shader: ShaderMode }
  | { kind: 'save'; id: number; state: boolean; shot: boolean }
  /** Confirms that these cartridge bytes reached IndexedDB, rather than merely the page. */
  | { kind: 'sram-kept'; sram: Uint8Array }
  | { kind: 'load'; id: number; state: Uint8Array }
  | { kind: 'reset'; id: number }
  | { kind: 'close'; id: number };

/** The requests that get a reply. */
export type Request = Extract<ToWorker, { id: number }>;

/** A game opened: its pace, and whether it went on from the state it was given. */
export interface Opened {
  frameHz: number;
  resumed: boolean;
}

/** A snapshot to keep. The cartridge's save comes again until the page confirms a successful database
 * commit with sram-kept, so a failed write can be retried with the same bytes. */
export interface Saved {
  sram: Uint8Array | null;
  state: Uint8Array | null;
  shot: Blob | null;
}

export type FromWorker =
  | { kind: 'reply'; id: number; ok: true; value: unknown }
  | { kind: 'reply'; id: number; ok: false; error: string }
  /** The frames of the last run are done: how many ran. */
  | { kind: 'ran'; frames: number }
  /** The machine stopped on an error of its own (a core's panic). */
  | { kind: 'failed'; error: string };

/** What the worker sends the sound worklet: a run's samples, at a rate that takes the run's speed in. */
export interface SoundChunk {
  samples: Int16Array;
  stereo: boolean;
  rate: number;
}
