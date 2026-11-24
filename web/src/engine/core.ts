// The emulation cores' WebAssembly module (web/core), one machine per instance. The page's worker runs
// it, and so does tools/check.ts under Node, to compare it with the native build.

/** The module's functions, as web/core/src/lib.rs exports them. */
interface Exports {
  memory: WebAssembly.Memory;
  alloc(len: number): number;
  free(ptr: number, len: number): void;
  out(): number;
  out_len(): number;
  open(sys: number, sysLen: number, rom: number, romLen: number): number;
  close(): void;
  run_frame(pad1: number, pad2: number, pad3: number, pad4: number): void;
  width(): number;
  height(): number;
  pixel_aspect(): number;
  frame_hz(): number;
  picture(): number;
  audio(): number;
  audio_len(): number;
  audio_stereo(): number;
  sample_hz(): number;
  save_state(): number;
  load_state(ptr: number, len: number): number;
  sram(): number;
  load_sram(ptr: number, len: number): void;
  set_clock(utc: number, offset: number): void;
}

/** A pad's buttons: bit n is button n of the browser's standard gamepad, as web/core/src/lib.rs has them,
 * and the guide button (16), which only the page reads. */
export const Pad = {
  south: 1 << 0,
  east: 1 << 1,
  west: 1 << 2,
  north: 1 << 3,
  l1: 1 << 4,
  r1: 1 << 5,
  l2: 1 << 6,
  r2: 1 << 7,
  select: 1 << 8,
  start: 1 << 9,
  l3: 1 << 10,
  r3: 1 << 11,
  up: 1 << 12,
  down: 1 << 13,
  left: 1 << 14,
  right: 1 << 15,
  home: 1 << 16,
} as const;

const utf8 = new TextDecoder();
const encode = new TextEncoder();

export class Machine {
  private readonly x: Exports;

  private constructor(x: Exports) {
    this.x = x;
  }

  /** A fresh instance of the module, from its compiled form or its bytes. */
  static async load(module: WebAssembly.Module | BufferSource): Promise<Machine> {
    let memory: WebAssembly.Memory | undefined;
    const imports = {
      sol: {
        log(ptr: number, len: number) {
          if (memory) console.error(utf8.decode(new Uint8Array(memory.buffer, ptr, len)));
        },
      },
    };
    const instance =
      module instanceof WebAssembly.Module
        ? await WebAssembly.instantiate(module, imports)
        : (await WebAssembly.instantiate(module, imports)).instance;
    const x = instance.exports as unknown as Exports;
    memory = x.memory;
    return new Machine(x);
  }

  /** Powers on a game on a console (systems.ts's ids); throws why not. */
  open(system: string, rom: Uint8Array): void {
    const result = this.pass(encode.encode(system), (s, sLen) => this.pass(rom, (r, rLen) => this.x.open(s, sLen, r, rLen)));
    if (result !== 0) throw new Error(this.outText());
  }

  close(): void {
    this.x.close();
  }

  /** Runs a frame with each player's buttons held (bits of Pad). */
  runFrame(pads: readonly number[]): void {
    this.x.run_frame(pads[0] ?? 0, pads[1] ?? 0, pads[2] ?? 0, pads[3] ?? 0);
  }

  get width(): number {
    return this.x.width();
  }

  get height(): number {
    return this.x.height();
  }

  /** The console's pixels' width over their height, on the screen it was made for. */
  get pixelAspect(): number {
    return this.x.pixel_aspect();
  }

  get frameHz(): number {
    return this.x.frame_hz();
  }

  get sampleHz(): number {
    return this.x.sample_hz();
  }

  get stereo(): boolean {
    return this.x.audio_stereo() === 1;
  }

  /** The last frame as RGBA: a view of the module's memory, good until the next call. */
  picture(): Uint8ClampedArray<ArrayBuffer> {
    const ptr = this.x.picture();
    return new Uint8ClampedArray(this.x.memory.buffer, ptr, this.width * this.height * 4);
  }

  /** The last frame's sound: a view of the module's memory, good until the next call. */
  audio(): Int16Array<ArrayBuffer> {
    const ptr = this.x.audio();
    return new Int16Array(this.x.memory.buffer, ptr, this.x.audio_len());
  }

  /** The whole machine, for Quick Resume. */
  saveState(): Uint8Array<ArrayBuffer> {
    return this.outBytes(this.x.save_state());
  }

  /** Puts the machine back as a state has it; throws why not, leaving it as it was. */
  loadState(state: Uint8Array): void {
    if (this.pass(state, (p, len) => this.x.load_state(p, len)) !== 0) throw new Error(this.outText());
  }

  /** The cartridge's own save, or null for a game without one. */
  sram(): Uint8Array<ArrayBuffer> | null {
    const len = this.x.sram();
    return len < 0 ? null : this.outBytes(len);
  }

  loadSram(data: Uint8Array): void {
    this.pass(data, (p, len) => this.x.load_sram(p, len));
  }

  /** The time for a cartridge's clock: seconds since 1970 in UTC, and the player's time zone's offset
   * from UTC in seconds. By default the browser's time now. */
  setClock(utc = Math.floor(Date.now() / 1000), offset = -new Date(utc * 1000).getTimezoneOffset() * 60): void {
    this.x.set_clock(utc, offset);
  }

  /** Copies bytes into the module's memory for a call, and gives the room back after. */
  private pass<T>(data: Uint8Array, call: (ptr: number, len: number) => T): T {
    const ptr = this.x.alloc(data.length);
    new Uint8Array(this.x.memory.buffer, ptr, data.length).set(data);
    try {
      return call(ptr, data.length);
    } finally {
      this.x.free(ptr, data.length);
    }
  }

  private outBytes(len: number): Uint8Array<ArrayBuffer> {
    return new Uint8Array(this.x.memory.buffer, this.x.out(), len).slice();
  }

  private outText(): string {
    return utf8.decode(new Uint8Array(this.x.memory.buffer, this.x.out(), this.x.out_len()));
  }
}
