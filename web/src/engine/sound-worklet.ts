// The game's sound, on the audio thread: the worker's chunks go into a ring, and the worklet plays it
// out at the output's rate, resampling linearly. The page and the sound card keep their own clocks, so
// the ring drifts: as the game host does on the console, the worklet plays a little faster or slower,
// up to 0.5 % (a twelfth of a semitone), to hold the ring at about 50 ms. When it runs dry (the game
// paused, the tab hidden) it goes quiet and waits for the ring to fill again before it plays on.

import type { SoundChunk } from './protocol.ts';

// The AudioWorkletGlobalScope's own names, which the DOM's types do not have.
declare abstract class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor();
}
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;
declare const sampleRate: number;

/** Room in the ring, in frames (left and right): more than a second at 48 kHz. */
const SIZE = 1 << 16;
/** The ring's level to hold, and the most it may hold before old sound is let go, in seconds. */
const TARGET = 0.05;
const MOST = 0.2;
/** How far the pace may move to hold it. */
const DRIFT = 0.005;

class Sound extends AudioWorkletProcessor {
  private readonly left = new Float32Array(SIZE);
  private readonly right = new Float32Array(SIZE);
  /** Frames written in all, and where playing is, in frames with a fraction. */
  private written = 0;
  private at = 0;
  /** The chunks' rate: frames a second of the game's sound. */
  private rate = 48000;
  private playing = false;
  /** For the page's stats: the times the ring ran dry, and output frames since the last report. */
  private dry = 0;
  private sinceReport = 0;

  constructor() {
    super();
    this.port.onmessage = ({ data }: MessageEvent<{ port?: MessagePort; flush?: boolean }>) => {
      if (data.port) data.port.onmessage = ({ data: chunk }: MessageEvent<SoundChunk>) => this.take(chunk);
      if (data.flush) this.flush();
    };
  }

  private flush(): void {
    this.at = this.written;
    this.playing = false;
  }

  private take({ samples, stereo, rate }: SoundChunk): void {
    this.rate = rate;
    const step = stereo ? 2 : 1;
    for (let i = 0; i + step - 1 < samples.length; i += step) {
      const slot = this.written & (SIZE - 1);
      const l = samples[i]! / 32768;
      this.left[slot] = l;
      this.right[slot] = stereo ? samples[i + 1]! / 32768 : l;
      this.written++;
    }
    // After a hitch, play from the newest sound, not seconds late.
    if (this.written - this.at > MOST * rate) this.at = this.written - TARGET * rate;
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const [outLeft, outRight] = outputs[0] ?? [];
    if (!outLeft) return true;
    this.sinceReport += outLeft.length;
    if (this.sinceReport >= sampleRate / 2) {
      this.sinceReport = 0;
      this.port.postMessage({ level: ((this.written - this.at) / this.rate) * 1000, dry: this.dry });
    }
    const target = TARGET * this.rate;
    let level = this.written - this.at;
    if (!this.playing && level >= target) this.playing = true;
    if (!this.playing) {
      outLeft.fill(0);
      outRight?.fill(0);
      return true;
    }
    const pace = 1 + Math.max(-1, Math.min(1, (level - target) / target)) * DRIFT;
    const step = (this.rate / sampleRate) * pace;
    for (let i = 0; i < outLeft.length; i++) {
      level = this.written - this.at;
      if (level < 2) {
        // Dry: silence until the ring fills again.
        this.playing = false;
        this.dry++;
        outLeft.fill(0, i);
        outRight?.fill(0, i);
        break;
      }
      const whole = Math.floor(this.at);
      const part = this.at - whole;
      const a = whole & (SIZE - 1);
      const b = (whole + 1) & (SIZE - 1);
      outLeft[i] = this.left[a]! + (this.left[b]! - this.left[a]!) * part;
      if (outRight) outRight[i] = this.right[a]! + (this.right[b]! - this.right[a]!) * part;
      this.at += step;
    }
    return true;
  }
}

registerProcessor('sol-sound', Sound);
