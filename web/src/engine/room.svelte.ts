// Browser transport for sol-netplay v2. A socket carries one seat. A reconnect
// reclaims it and starts a new epoch from the host's current emulated frame.
import wasmUrl from '../../../target/wasm32-unknown-unknown/release/solweb.wasm?url';
import timelineUrl from '../../../target/wasm32-unknown-unknown/release/sol_netplay.wasm?url';
import type { NetStep } from './protocol.ts';
import { Pad } from './core.ts';

type Pads = [number, number];
type Start = { type: 'create'; game: string; name: string } | { type: 'join'; code: string };
type Event =
  | { type: 'seat'; code: string; seat: number; token: string; game: string; name: string }
  | { type: 'pong'; seat: number }
  | { type: 'performance'; epoch: number; seat: number; slow: boolean }
  | { type: 'wait' }
  | { type: 'sync'; epoch: number }
  | { type: 'snapshot'; epoch: number; frame: number; data: string }
  | { type: 'go'; epoch: number; frame: number }
  | { type: 'frame'; frame: number; pads: Pads }
  | { type: 'error'; reason: string };

export interface RoomMachine {
  reset(frame: number): void;
  snapshot(frame: number, rewind: boolean): Promise<Uint8Array<ArrayBuffer>>;
  restore(state: Uint8Array): Promise<void>;
}

interface Timeline extends WebAssembly.Exports {
  frame(): number;
  stable(): number;
  reserve(lead: number, pad: number): number;
  confirm(frame: number, p1: number, p2: number): number;
  advance(seat: number, predict: boolean): number;
  rewind(): number;
  retire(): number;
  reset(frame: number): void;
  check_interval(): number;
}

async function timeline(): Promise<Timeline> {
  const response = await fetch(timelineUrl);
  if (!response.ok) throw new Error('netplay');
  const { instance } = await WebAssembly.instantiate(await response.arrayBuffer());
  return instance.exports as Timeline;
}

let build: Promise<string> | undefined;

async function hash(bytes: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
}

function coreBuild(): Promise<string> {
  return build ??= fetch(wasmUrl).then(async response => {
    if (!response.ok) throw new Error('core');
    return hash(await response.arrayBuffer());
  }).catch(error => { build = undefined; throw error; });
}

// NES wire order. Browser gamepad bits also contain triggers and system buttons.
const buttons = [Pad.east, Pad.south, Pad.select, Pad.start, Pad.up, Pad.down, Pad.left, Pad.right];
const encode = (bits: number) => buttons.reduce((pad, button, index) => pad | ((bits & button ? 1 : 0) << index), 0);
const decode = (pad: number) => buttons.reduce((bits, button, index) => bits | (pad & (1 << index) ? button : 0), 0);
function toBase64(bytes: Uint8Array): string {
  let text = '';
  for (let i = 0; i < bytes.length; i += 8192) text += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return btoa(text);
}

export class Room {
  phase = $state<'connecting' | 'waiting' | 'sync' | 'playing' | 'reconnecting' | 'error'>('connecting');
  reason = $state('');
  code = $state('');
  game = $state('');
  name = $state('');
  seat = $state(0);
  private token = '';
  private ws: WebSocket | null = null;
  private timer = 0;
  private deadline = 0;
  private closed = false;
  private machine: RoomMachine | null = null;
  private ready = false;
  private epoch = 0;
  private timeline!: Timeline;
  private times = new Map<number, number>();
  private lead = 2;
  private nextCheck = 0;
  private lastFrameAt = 0;
  private lastMessageAt = 0;
  private lastPing = 0;
  private measuredAt = 0;
  private work = 0;
  private completed = 0;
  localSlow = $state(false);
  peerSlow = $state(false);
  networkWait = $state(false);

  constructor(request: Start) {
    void Promise.all([coreBuild(), timeline()]).then(([core, client]) => {
      this.timeline = client;
      this.connect({ ...request, version: 2, core });
    }).catch(() => this.fail('unavailable'));
  }

  private connect(hello: (Start & { version: number; core: string }) | { type: 'resume'; version: number; code: string; token: string }): void {
    if (this.closed) return;
    const url = new URL(import.meta.env.BASE_URL + 'netplay', location.origin);
    if (hello.type !== 'create') url.searchParams.set('room', hello.code);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = this.ws = new WebSocket(url);
    const timeout = window.setTimeout(() => ws.close(), 10000);
    ws.onopen = () => ws.send(JSON.stringify(hello));
    ws.onmessage = ({ data }: MessageEvent<string>) => {
      if (this.closed || this.ws !== ws) return;
      clearTimeout(timeout);
      this.lastMessageAt = performance.now();
      try { this.receive(JSON.parse(data) as Event); } catch { this.fail('protocol'); }
    };
    ws.onclose = () => {
      clearTimeout(timeout);
      if (this.closed || this.ws !== ws) return;
      this.ws = null;
      this.epoch = 0;
      if (!this.token) return this.fail('unavailable');
      this.deadline ||= Date.now() + 60000;
      if (Date.now() >= this.deadline) return this.fail('ended');
      this.phase = 'reconnecting';
      this.timer = window.setTimeout(() => this.connect({ type: 'resume', version: 2, code: this.code, token: this.token }), 800);
    };
  }

  private send(msg: object): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  private receive(event: Event): void {
    switch (event.type) {
      case 'seat':
        this.code = event.code;
        this.game = event.game;
        this.name = event.name;
        this.seat = event.seat;
        this.token = event.token;
        this.deadline = 0;
        this.phase = 'waiting';
        if (this.ready) this.send({ type: 'ready', game: this.game });
        return;
      case 'pong': return;
      case 'performance':
        if (this.epoch === event.epoch && this.seat !== event.seat) this.peerSlow = event.slow;
        return;
      case 'wait':
        this.epoch = 0;
        this.phase = 'waiting';
        return;
      case 'sync':
        this.phase = 'sync';
        this.epoch = event.epoch;
        if (this.seat === 0 && this.machine) {
          const frame = this.timeline.stable() >>> 0;
          void this.machine.snapshot(frame, true).then(bytes => {
            if (this.epoch === event.epoch) this.send({ type: 'snapshot', epoch: event.epoch, frame, data: toBase64(bytes) });
          }).catch(() => this.fail('snapshot'));
        }
        return;
      case 'snapshot':
        if (this.machine && this.epoch === event.epoch) {
          const bytes = Uint8Array.from(atob(event.data), c => c.charCodeAt(0));
          void this.machine.restore(bytes).then(() => {
            if (this.epoch === event.epoch) this.send({ type: 'loaded', epoch: event.epoch });
          }).catch(() => this.fail('snapshot'));
        }
        return;
      case 'go':
        if (event.epoch !== this.epoch) return;
        this.timeline.reset(event.frame);
        this.machine?.reset(event.frame);
        const interval = this.timeline.check_interval();
        this.nextCheck = event.frame - event.frame % interval + interval;
        this.lead = 2;
        this.times.clear();
        this.lastMessageAt = this.lastFrameAt = this.measuredAt = performance.now();
        this.work = this.completed = 0;
        this.localSlow = this.peerSlow = this.networkWait = false;
        this.phase = 'playing';
        return;
      case 'frame':
        if (this.phase !== 'playing') return;
        if (!this.timeline.confirm(event.frame, ...event.pads)) return this.fail('protocol');
        this.lastFrameAt = performance.now();
        this.networkWait = false;
        const sent = this.times.get(event.frame);
        this.times.delete(event.frame);
        if (sent !== undefined) this.lead = Math.min(8, Math.max(2, Math.ceil((this.lastFrameAt - sent) / 16.64)));
        return;
      case 'error': return this.fail(event.reason);
      default: return this.fail('protocol');
    }
  }

  attach(machine: RoomMachine, game: string): void {
    if (game !== this.game) throw new Error('room game mismatch');
    this.machine = machine;
    this.setReady(true);
  }

  setReady(ready: boolean): void {
    this.ready = ready;
    if (!ready && this.phase === 'playing') this.phase = 'waiting';
    if (this.machine && this.token && !this.closed) this.send(ready ? { type: 'ready', game: this.game } : { type: 'pause' });
  }

  private submit(bits: number): void {
    const pad = encode(bits);
    for (let n = this.timeline.reserve(this.lead, pad); n >= 0; n = this.timeline.reserve(this.lead, pad)) {
      this.times.set(n, performance.now());
      this.send({ type: 'input', epoch: this.epoch, frame: n, pad });
    }
  }

  /** The worker runs corrections in order before presenting the newest frame. */
  step(bits: number, due: number): NetStep | null {
    if (this.phase !== 'playing' || !this.ready) return null;
    const t = this.timeline;
    const oldHead = t.frame() >>> 0;
    const oldStable = t.stable() >>> 0;
    const rewind = t.rewind() >= 0;
    const start = t.frame() >>> 0;
    const goal = Math.min(this.nextCheck, oldHead + due);
    const pads: Pads[] = [];
    this.submit(bits);
    while ((t.frame() >>> 0) < goal) {
      const replay = (t.frame() >>> 0) < oldHead;
      const pair = t.advance(this.seat, replay || !this.localSlow);
      if (pair < 0) break;
      pads.push([decode(pair & 255), decode(pair >>> 8)]);
      // A 30 Hz screen consumes two 60 Hz frames on a tick. Keep the wire
      // window supplied during the batch instead of waiting for the next tick.
      this.submit(bits);
    }
    const stable = t.retire() >>> 0;
    if (!pads.length && stable === oldStable && !rewind) return null;
    return { start, rewind, pads, stable, fresh: (t.frame() >>> 0) - oldHead };
  }

  check(stable: number): void {
    if (stable !== this.nextCheck || !this.machine) return;
    const epoch = this.epoch;
    this.nextCheck += this.timeline.check_interval();
    void this.machine.snapshot(stable, false).then(bytes => hash(bytes.buffer)).then(digest => {
      if (this.epoch === epoch) this.send({ type: 'check', epoch, frame: stable, hash: digest });
    }).catch(() => { if (this.epoch === epoch) this.fail('snapshot'); });
  }

  monitor(now: number): void {
    if (this.phase !== 'playing') return;
    this.networkWait = now - this.lastFrameAt > 250;
    if (!this.networkWait) return;
    // A live relay answers even when the other player is behind. Reconnect a
    // silent transport independently of the browser's TCP timeout.
    if (now - this.lastMessageAt > 8000) this.ws?.close();
    else if (now - this.lastPing > 1500) {
      this.lastPing = now;
      this.send({ type: 'ping' });
    }
  }

  workload(frames: number, elapsed: number): void {
    if (this.phase !== 'playing') return;
    this.work += elapsed;
    this.completed += frames;
    const now = performance.now();
    if (now - this.measuredAt < 850 || this.completed < 4) return;
    const perFrame = this.work / this.completed;
    const slow = perFrame > (this.localSlow ? 14 : 17);
    this.work = this.completed = 0;
    this.measuredAt = now;
    if (this.localSlow === slow) return;
    this.localSlow = slow;
    this.send({ type: 'performance', epoch: this.epoch, slow });
  }

  private fail(reason: string): void {
    if (this.closed) return;
    this.reason = reason;
    this.phase = 'error';
    this.close();
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.epoch = 0;
    clearTimeout(this.timer);
    this.send({ type: 'leave' });
    this.ws?.close();
    this.machine = null;
  }
}
