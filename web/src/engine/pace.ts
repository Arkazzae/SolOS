// How many of the console's frames each of the display's refreshes runs.
//
// When the display refreshes within 1.2 % of the console's frame rate (a 60 Hz screen and the NES's
// 60.1, the Game Boys' 59.7 or the Mega Drive's 59.9), each refresh runs one frame, as the console's game
// host does on a TV, and the sound plays that much faster or slower: the motion stays even. Otherwise (a
// 120 or 144 Hz screen, a European Mega Drive game's 50) frames run as the time that passed holds, at the
// console's own rate. Far behind (a hidden tab, a slow machine), the time is dropped rather than caught
// up.

/** How near the refresh must be to the console's rate for a frame each. */
const LOCK = 0.012;
/** Refreshes measured before the rate is known, and the spacing that counts as one (ms). */
const SAMPLES = 30;
const SHORTEST = 4;
const LONGEST = 50;

export class Pace {
  /** The display's refreshes a second, from the time between animation frames; 0 until known. */
  refresh = 0;
  /** The sound's speed against the console's: the refresh over the frame rate when locked. */
  speed = 1;
  private due = 0;
  private readonly intervals: number[] = [];
  readonly frameHz: number;

  constructor(frameHz: number) {
    this.frameHz = frameHz;
  }

  /** Whether each refresh runs a frame. */
  get locked(): boolean {
    return this.refresh > 0 && Math.abs(this.refresh / this.frameHz - 1) < LOCK;
  }

  /** The frames due after a refresh dt ms after the last, `times` as many to fast forward. */
  frames(dt: number, times: number): number {
    this.measure(dt);
    if (this.locked) {
      this.speed = this.refresh / this.frameHz;
      this.due += (dt > 0 ? Math.min(3, Math.max(1, Math.round((dt * this.refresh) / 1000))) : 1) * times;
    } else {
      this.speed = 1;
      this.due += ((dt * this.frameHz) / 1000) * times;
    }
    if (this.due > 4 * times) this.due = times;
    const frames = Math.floor(this.due);
    this.due -= frames;
    return frames;
  }

  /** Frames that were due but not run (the machine still busy) wait for the next refresh. */
  defer(frames: number): void {
    this.due += frames;
  }

  /** Nothing due, as after a pause. */
  reset(): void {
    this.due = 0;
  }

  /** Keeps the display's refresh rate from the median of the last refreshes' spacing. */
  private measure(dt: number): void {
    if (dt < SHORTEST || dt > LONGEST) return;
    this.intervals.push(dt);
    if (this.intervals.length < SAMPLES) return;
    const sorted = this.intervals.sort((a, b) => a - b);
    this.refresh = 1000 / sorted[sorted.length >> 1]!;
    this.intervals.length = 0;
  }
}
