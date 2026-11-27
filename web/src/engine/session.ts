// A game being played: the machine's worker, the pace of its frames, the buttons, and the saves, which
// go on as the console's do. The cartridge's own save is kept every few seconds when it changes; the
// whole machine (Quick Resume) every minute, when the tab is hidden, when the game menu opens and when
// the player leaves, and the game goes on from it next time.
//
// The session draws nothing but the game: the page's loop (ui/pump.ts) gives it each animation frame
// with the buttons held, and what the player should see (the fast forward badge, the counter, a fault)
// goes out as a Status whenever it changes. The game menu is the page's; it pauses and resumes the
// session and calls its actions.
//
// Fast forward, as the console's Przyspieszenie: the menu's speed (2×, 4×, no limit) for the rest of
// the session, or no limit while Tab or a pad's RT is held. A refresh then asks for that many times the
// frames, and the worker runs them while each still ends within most of a refresh (worker.ts), so a
// machine too slow for more goes on as it did.

import { Pad } from './core.ts';
import type { Input, Poll } from './input.ts';
import { Pace } from './pace.ts';
import type { Opened, Saved } from './protocol.ts';
import type { Room } from './room.svelte.ts';
import { Remote } from './remote.ts';
import type { ShaderMode } from './shaders.ts';
import type { Speaker } from './sound.ts';
import * as store from './store.ts';

/** How often the cartridge's save, and the whole machine, are kept while playing (ms). */
const SRAM_EVERY = 5_000;
const RESUME_EVERY = 60_000;
/** Fast forward's speeds, the menu's in turn: as many times the frames a refresh; no limit is at most
 * this many, as many of them as fit. */
export const SPEEDS = [1, 2, 4, 16] as const;
const NO_LIMIT = SPEEDS.length - 1;
/** The share of a refresh the worker may take for a fast refresh's frames. */
const FAST_BUDGET = 0.7;

/** The performance counter: a second's pictures, the game's speed against its console's, the display's
 * refresh and whether frames follow it, the worker's time for a run, the sound ring's level and how
 * often it ran dry. */
export interface Stats {
  fps: number;
  speed: number;
  refresh: number;
  locked: boolean;
  work: number;
  sound: number;
  dry: number;
}

/** What the player sees of the session besides the game. */
export interface Status {
  /** The machine stopped on an error of its own: only starting over or leaving is left. */
  failed: string | null;
  /** The fast forward running now, an index in SPEEDS (0: the game's own speed). */
  fast: number;
  /** The browser has not let the sound start yet. */
  silent: boolean;
  /** A state waits in the slot. */
  slot: boolean;
  /** The counter, once a second while it is shown. */
  stats: Stats | null;
}

export interface SessionEvents {
  /** What the player sees changed. */
  status(status: Status): void;
  /** The pad's guide button: the game menu. */
  menu(): void;
  /** A save did not reach the database; the game goes on and the next save tries again. */
  saveFailed(error: unknown): void;
  /** The machine stopped on an error of its own. */
  failed(error: string): void;
}

export interface SessionOptions {
  shader: ShaderMode;
  /** Whether the game goes on from where it was left, and is kept so when left. */
  quickResume: boolean;
  room?: Room | null;
  hostProgress?: boolean;
}

type Target = 'sram' | 'resume' | 'slot';

export class Session {
  private game: store.Game;
  private readonly remote: Remote;
  private readonly speaker: Speaker;
  private readonly input: Input;
  private readonly events: SessionEvents;
  private readonly quickResume: boolean;
  private readonly room: Room | null;
  private readonly cleanup: (() => void)[] = [];
  private pace = new Pace(60);

  private closed = false;
  private paused = false;
  private busy = false;
  private last = 0;
  private runStart = 0;
  /** The menu's fast forward, an index in SPEEDS: the game's own speed at each start. */
  private menuFast = 0;
  /** Buttons held when the game went on after the menu, kept from the game until let go: the button
   * that chose "Back to the game" is not the game's. */
  private masked = 0;
  private maskNext = false;
  /** Saves one after another. */
  private saving: Promise<boolean> = Promise.resolve(true);
  private counted = { since: 0, ran: 0, runs: 0, work: 0 };
  private showStats = false;
  private status: Status = { failed: null, fast: 0, silent: false, slot: false, stats: null };

  /** Whether the game went on from its Quick Resume state. */
  resumed = false;

  private constructor(canvas: HTMLCanvasElement, game: store.Game, speaker: Speaker, input: Input, options: SessionOptions, events: SessionEvents) {
    this.game = game;
    this.speaker = speaker;
    this.input = input;
    this.events = events;
    this.room = options.room ?? null;
    this.quickResume = options.quickResume;
    this.status = { ...this.status, slot: game.slot };
    this.remote = new Remote({
      ran: (frames) => this.ran(frames),
      failed: (error) => this.fault(error),
    });
    const offscreen = canvas.transferControlToOffscreen();
    const sound = speaker.port();
    this.remote.post({ kind: 'init', canvas: offscreen, sound, shader: options.shader }, [offscreen, sound]);
    const resizer = new ResizeObserver(([entry]) => {
      if (!entry) return;
      const box = entry.devicePixelContentBoxSize?.[0];
      const width = box ? box.inlineSize : Math.round(entry.contentRect.width * devicePixelRatio);
      const height = box ? box.blockSize : Math.round(entry.contentRect.height * devicePixelRatio);
      this.remote.post({ kind: 'resize', width, height });
    });
    resizer.observe(canvas);
    const hidden = () => {
      if (this.room) {
        this.room.setReady(!document.hidden && !this.paused);
        this.last = 0;
        this.pace.reset();
      } else if (document.hidden) {
        void this.keep(true);
      }
    };
    document.addEventListener('visibilitychange', hidden);
    this.cleanup.push(() => resizer.disconnect(), () => document.removeEventListener('visibilitychange', hidden));
  }

  /** Starts a game on the canvas (one that has not been given to a session before), from where the
   * player left it if a Quick Resume state waits. */
  static async open(
    canvas: HTMLCanvasElement,
    game: store.Game,
    speaker: Speaker,
    input: Input,
    options: SessionOptions,
    events: SessionEvents,
  ): Promise<Session> {
    const session = new Session(canvas, game, speaker, input, options, events);
    try {
      const localProgress = !session.room || (session.room.seat === 0 && options.hostProgress);
      const [rom, sram, state] = await Promise.all([
        store.rom(game.id),
        localProgress ? store.sram(game.id) : undefined,
        localProgress && options.quickResume ? store.resumeState(game.id) : undefined,
      ]);
      if (!rom) throw new Error('the file is gone');
      const opened = await session.remote.call<Opened>({ kind: 'open', system: game.system, rom, sram: sram ?? null, state: state ?? null });
      if (session.status.failed) throw new Error(session.status.failed);
      session.pace = new Pace(opened.frameHz);
      session.resumed = opened.resumed;
      session.game = { ...game, played: Date.now() };
      session.room?.attach({
        reset: frame => {
          session.remote.post({ kind: 'net-reset', frame });
          session.pace.reset();
          session.last = 0;
          session.maskNext = true;
          speaker.flush();
        },
        snapshot: (frame, rewind) => session.remote.call<Uint8Array<ArrayBuffer>>({ kind: 'snapshot', frame, rewind }),
        restore: async state => {
          await session.remote.call({ kind: 'load', state });
          speaker.flush();
        },
      }, game.id);
      if (document.hidden) session.room?.setReady(false);
      if (!session.room) void store.saveGame(session.game).catch((error: unknown) => events.saveFailed(error));
    } catch (error) {
      session.stop();
      throw error;
    }
    speaker.flush();
    const every = (ms: number, f: () => void) => {
      const timer = window.setInterval(f, ms);
      session.cleanup.push(() => clearInterval(timer));
    };
    if (!session.room) every(SRAM_EVERY, () => void session.keep(false));
    if (!session.room && session.quickResume) every(RESUME_EVERY, () => void session.keep(true));
    input.keyboard = true;
    return session;
  }

  /** An animation frame of the page while the game runs: frames due run with the buttons held. */
  frame(now: number, poll: Poll): void {
    if (this.closed || this.paused) return;
    if (poll.pressed & Pad.home) {
      this.events.menu();
      return;
    }
    const dt = this.last ? now - this.last : 0;
    this.last = now;
    const held = poll.pads.reduce((all, bits) => all | bits, 0);
    if (this.maskNext) {
      this.masked = held;
      this.maskNext = false;
    }
    this.masked &= held;
    const pads = poll.pads.map((bits) => bits & ~this.masked);
    const fast = this.room ? 0 : poll.fast ? NO_LIMIT : this.menuFast;
    this.update({ fast, silent: !this.speaker.running });
    if (!this.status.failed) {
      const times = SPEEDS[fast]!;
      const frames = this.pace.frames(dt, times, !!this.room);
      if (this.room) {
        this.room.monitor(performance.now());
        if (this.room.phase === 'playing') {
          const step = this.busy ? null : this.room.step(pads[0] ?? 0, frames);
          this.pace.defer(frames - (step?.fresh ?? 0));
          if (step) {
            this.busy = true;
            this.runStart = performance.now();
            this.remote.post({ kind: 'net-frames', ...step });
            this.room.check(step.stable);
          }
        }
      } else if (frames > 0 && !this.busy) {
        this.busy = true;
        this.runStart = performance.now();
        const budget = fast > 0 ? (FAST_BUDGET * 1000) / (this.pace.refresh || 60) : 0;
        this.remote.post({ kind: 'run', frames, pads, speed: this.pace.speed, budget });
      } else if (this.busy) {
        // The machine is still on the last run: what is due waits.
        this.pace.defer(frames);
      }
    }
    this.count(now);
  }

  /** The game menu opened: the game stops, the keyboard goes to the menu, and the save is kept. */
  pause(): void {
    if (this.paused || this.closed) return;
    this.paused = true;
    this.room?.setReady(false);
    this.input.keyboard = false;
    this.update({ fast: 0 });
    void this.keep(false);
  }

  /** Back to the game, the buttons held now kept from it until let go. */
  resume(): void {
    if (!this.paused || this.closed || this.status.failed) return;
    this.paused = false;
    this.room?.setReady(!document.hidden);
    this.maskNext = true;
    this.input.keyboard = true;
    this.last = 0;
    this.pace.reset();
  }

  /** The menu's fast forward, an index in SPEEDS, for the rest of the session. */
  get fast(): number {
    return this.menuFast;
  }

  set fast(index: number) {
    this.menuFast = ((index % SPEEDS.length) + SPEEDS.length) % SPEEDS.length;
  }

  setShader(shader: ShaderMode): void {
    this.remote.post({ kind: 'shader', shader });
  }

  /** Whether the counter goes out with the status, once a second. */
  set stats(on: boolean) {
    this.showStats = on;
    if (!on) this.update({ stats: null });
  }

  /** Keeps this moment in the slot, over the one before. */
  saveSlot(): Promise<boolean> {
    return this.save('slot');
  }

  /** Goes back to the moment in the slot; false when there is none. */
  async loadSlot(): Promise<boolean> {
    if (this.room) return false;
    const state = await store.slot(this.game.id);
    if (!state) return false;
    await this.remote.call({ kind: 'load', state });
    this.speaker.flush();
    return true;
  }

  /** Turns the console off and on, the cartridge's save kept; after a fault too. */
  async restart(): Promise<void> {
    if (this.room) return;
    await this.remote.call({ kind: 'reset' });
    this.update({ failed: null });
    this.speaker.flush();
  }

  /** Saves and stops; false, and the game stays open, when the save did not reach the database. */
  async leave(): Promise<boolean> {
    if (this.closed) return true;
    if (!this.status.failed && !(await this.keep(true))) return false;
    this.stop();
    return true;
  }

  /** Stops without saving. */
  stop(): void {
    if (this.closed) return;
    this.closed = true;
    this.room?.close();
    this.input.keyboard = false;
    for (const off of this.cleanup) off();
    this.remote.terminate();
    this.speaker.flush();
  }

  private ran(frames: number): void {
    this.busy = false;
    this.counted.ran += frames;
    this.counted.runs++;
    const elapsed = performance.now() - this.runStart;
    this.counted.work += elapsed;
    this.room?.workload(frames, elapsed);
  }

  private fault(error: string): void {
    this.busy = false;
    this.update({ failed: error });
    this.events.failed(error);
  }

  private update(change: Partial<Status>): void {
    const next = { ...this.status, ...change };
    const keys = Object.keys(change) as (keyof Status)[];
    if (keys.every((key) => next[key] === this.status[key])) return;
    this.status = next;
    this.events.status(next);
  }

  /** Once a second, the counter's numbers. */
  private count(now: number): void {
    const c = this.counted;
    if (now - c.since < 1000) return;
    if (this.showStats && c.since > 0) {
      const seconds = (now - c.since) / 1000;
      const sound = this.speaker.report;
      this.update({
        stats: {
          fps: c.runs / seconds,
          speed: (c.ran / seconds / this.pace.frameHz) * 100,
          refresh: this.pace.refresh,
          locked: this.pace.locked,
          work: c.runs > 0 ? c.work / c.runs : 0,
          sound: sound.level,
          dry: sound.dry,
        },
      });
    }
    this.counted = { since: now, ran: 0, runs: 0, work: 0 };
  }

  /** Keeps the cartridge's save if it changed, and with whole the machine and a picture for the card. */
  private keep(whole: boolean): Promise<boolean> {
    if (this.room) return Promise.resolve(true);
    return this.save(whole && this.quickResume ? 'resume' : 'sram');
  }

  /** Every save goes through one queue; only a committed transaction is acknowledged to the worker, so
   * a failed one is retried with the same bytes. */
  private save(target: Target): Promise<boolean> {
    if (this.room || this.closed || this.status.failed) return Promise.resolve(false);
    this.saving = this.saving.then(async () => {
      if (this.closed || this.status.failed) return false;
      try {
        const saved = await this.remote.call<Saved>({ kind: 'save', state: target !== 'sram', shot: target === 'resume' });
        const record =
          target === 'resume' ? { ...this.game, resume: true, played: Date.now(), shot: saved.shot ?? this.game.shot }
          : target === 'slot' ? { ...this.game, slot: true }
          : this.game;
        await store.checkpoint(record, saved, target === 'sram' ? null : target === 'slot' ? 'slots' : 'resume');
        this.game = record;
        if (saved.sram) this.remote.post({ kind: 'sram-kept', sram: saved.sram });
        if (target === 'slot') this.update({ slot: true });
        return true;
      } catch (error) {
        this.events.saveFailed(error);
        return false;
      }
    });
    return this.saving;
  }
}
