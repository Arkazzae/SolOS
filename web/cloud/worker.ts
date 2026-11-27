// One Durable Object per invite code. Only seats and the synchronization epoch
// survive hibernation. The host supplies a fresh machine snapshot after wake.
import { DurableObject } from 'cloudflare:workers';
import relayModule from './generated/sol_relay_bg.wasm';
import { initSync, hello, RoomEngine } from './generated/sol_relay.js';

initSync({ module: relayModule });

type Seat = 0 | 1;
type Handshake = { type: 'create'; core: string; game: string; name: string }
  | { type: 'join'; core: string; code: string }
  | { type: 'resume'; code: string; token: string };
type Attachment = { seat: Seat } | { code: string; create: boolean; until: number };
interface State {
  version: number; code: string; game: string; core: string; name: string;
  tokens: [string, string | null]; expires: [number, number]; epoch: number; ready: number;
}

const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789';
function random(length: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(length)), n => alphabet[n & 31]).join('');
}

function error(socket: WebSocket, reason: string): void {
  try { socket.send(JSON.stringify({ type: 'error', reason })); } finally { socket.close(1000); }
}

function rejected(reason: string): Response {
  const pair = new WebSocketPair();
  pair[1].accept();
  error(pair[1], reason);
  return new Response(null, { status: 101, webSocket: pair[0] });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== '/netplay') return env.ASSETS.fetch(request);
    if (request.headers.get('Upgrade')?.toLowerCase() !== 'websocket') return new Response(null, { status: 426 });
    const room = url.searchParams.get('room');
    if (room !== null && !/^[A-HJ-NP-Z2-9]{6}$/.test(room)) return rejected('missing');
    // Only the entry Worker can allocate a code. Retry a random collision.
    for (let attempt = 0; attempt < 4; attempt++) {
      const code = room ?? random(6);
      const stub = env.ROOMS.getByName(code);
      const route = new URL(request.url);
      route.pathname = room === null ? '/create' : '/join';
      route.search = '';
      route.searchParams.set('code', code);
      const response = await stub.fetch(route, { headers: { Upgrade: 'websocket' } });
      if (response.status !== 409) return response;
    }
    return rejected('full');
  },
} satisfies ExportedHandler<Env>;

export class SolRoom extends DurableObject<Env> {
  private state?: State;
  private game?: RoomEngine;
  private seats: [WebSocket | null, WebSocket | null] = [null, null];
  private pending = new Map<WebSocket, Extract<Attachment, { code: string }>>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"type":"ping"}', '{"type":"pong"}'));
    for (const socket of ctx.getWebSockets()) {
      const attachment: Attachment | null = socket.deserializeAttachment();
      if (attachment && 'seat' in attachment) this.seats[attachment.seat] = socket;
      else if (attachment) this.pending.set(socket, attachment);
    }
    this.state = ctx.storage.kv.get<State>('room');
    if (this.state && this.state.version !== RoomEngine.version()) {
      this.end('version');
      return;
    }
    if (this.state) {
      let lostSeat = false;
      for (const seat of [0, 1] as const) {
        if (this.state.tokens[seat] && !this.seats[seat] && !this.state.expires[seat]) {
          this.state.ready &= ~(1 << seat);
          this.state.expires[seat] = Date.now() + 60000;
          lostSeat = true;
        }
      }
      const { game, epoch, ready } = this.state;
      this.game = new RoomEngine(game, epoch, ready);
      if (lostSeat) { this.save(); this.schedule(); }
      const sync = this.game.wake();
      if (sync) { this.save(); this.deliver(sync); }
      else if (lostSeat) this.deliver('{"type":"wait"}');
    }
  }

  override fetch(request: Request): Response {
    this.expire();
    const url = new URL(request.url);
    const create = url.pathname === '/create';
    if (create && (this.state || this.pending.size)) return new Response(null, { status: 409 });
    if (!create && !this.state) return rejected('missing');
    if (this.pending.size >= 4) return rejected('full');
    const pair = new WebSocketPair();
    const attachment = { code: url.searchParams.get('code')!, create, until: Date.now() + 10000 };
    this.ctx.acceptWebSocket(pair[1]);
    pair[1].serializeAttachment(attachment);
    this.pending.set(pair[1], attachment);
    this.schedule();
    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  private enter(socket: WebSocket, request: Handshake, connection: { code: string; create: boolean }): void {
    let seat: Seat;
    if (request.type === 'create') {
      if (!connection.create || this.state) throw 'missing';
      this.state = { version: RoomEngine.version(), code: connection.code, game: request.game, core: request.core, name: request.name,
        tokens: [random(32), null], expires: [0, 0], epoch: 0, ready: 0 };
      this.game = new RoomEngine(request.game, 0, 0);
      seat = 0;
    } else {
      if (connection.create || !this.state || this.state.code !== request.code) throw 'missing';
      if (request.type === 'join') {
        if (request.core !== this.state.core) throw 'version';
        if (this.state.tokens[1]) throw 'full';
        this.state.tokens[1] = random(32);
        seat = 1;
      } else {
        const encoder = new TextEncoder();
        const given = encoder.encode(request.token);
        const index = this.state.tokens.findIndex(token => token !== null && given.length === 32 &&
          crypto.subtle.timingSafeEqual(given, encoder.encode(token)));
        if (index !== 0 && index !== 1) throw 'missing';
        seat = index;
        this.seats[seat]?.close(1000);
        this.command(seat, '{"type":"pause"}');
      }
    }
    const state = this.state;
    state.expires[seat] = 0;
    this.seats[seat] = socket;
    socket.serializeAttachment({ seat });
    this.save();
    socket.send(JSON.stringify({ type: 'seat', code: state.code, seat, token: state.tokens[seat], game: state.game, name: state.name }));
  }

  override webSocketMessage(socket: WebSocket, message: string | ArrayBuffer): void {
    const pending = this.pending.get(socket);
    if (pending) {
      this.pending.delete(socket);
      try {
        if (typeof message !== 'string' || message.length > 2048 || pending.until <= Date.now()) throw 'protocol';
        this.expire();
        this.enter(socket, JSON.parse(hello(message)) as Handshake, pending);
      } catch (reason) {
        error(socket, typeof reason === 'string' ? reason : 'protocol');
      }
      this.schedule();
      return;
    }
    const seat = this.seats.indexOf(socket);
    if (seat !== 0 && seat !== 1) return;
    try {
      if (typeof message !== 'string' || message.length > 512 * 1024) throw 'protocol';
      this.command(seat, message);
    } catch (reason) {
      this.end(typeof reason === 'string' ? reason : 'protocol');
    }
  }

  private command(seat: Seat, message: string): void {
    const game = this.game!;
    const reply = game.command(seat, message);
    if (game.epoch() !== this.state!.epoch || game.ready() !== this.state!.ready) this.save();
    if (reply) this.deliver(reply);
  }

  private deliver(reply: string): void {
    const event = JSON.parse(reply);
    const target = event.type === 'snapshot' ? 1 : event.type === 'pong' ? event.seat : null;
    this.seats.forEach((socket, seat) => {
      if (!socket || (target !== null && seat !== target) || socket.readyState !== WebSocket.OPEN) return;
      try { socket.send(reply); } catch { this.webSocketError(socket); }
    });
  }

  private save(): void {
    const state = this.state!;
    state.ready = this.game!.ready();
    state.epoch = this.game!.epoch();
    this.ctx.storage.kv.put('room', state);
  }

  override webSocketClose(socket: WebSocket): void {
    this.pending.delete(socket);
    const seat = this.seats.indexOf(socket);
    if ((seat === 0 || seat === 1) && this.state) {
      this.seats[seat] = null;
      this.state.expires[seat] = Date.now() + 60000;
      this.command(seat, '{"type":"pause"}');
      this.save();
    }
    this.schedule();
  }

  override webSocketError(socket: WebSocket): void {
    socket.close(1011);
    this.webSocketClose(socket);
  }

  private end(reason: string): void {
    this.state = undefined;
    this.game?.free();
    this.game = undefined;
    this.seats = [null, null];
    this.pending.clear();
    for (const socket of this.ctx.getWebSockets()) {
      if (socket.readyState === WebSocket.OPEN) error(socket, reason);
    }
    this.schedule();
  }

  private expire(): void {
    const now = Date.now();
    for (const [socket, { until }] of this.pending) {
      if (until <= now) { this.pending.delete(socket); socket.close(1000); }
    }
    if (this.state?.expires.some(time => time > 0 && time <= now)) this.end('ended');
  }

  private schedule(): void {
    if (!this.state && this.pending.size === 0) {
      // This compatibility date also removes the alarm. Reclaim the SQLite database,
      // including metadata left by a handshake that expired before a room was created.
      this.ctx.waitUntil(this.ctx.storage.deleteAll());
      return;
    }
    const deadlines = [...(this.state?.expires ?? []), ...Array.from(this.pending.values(), p => p.until)].filter(t => t > 0);
    this.ctx.waitUntil(deadlines.length ? this.ctx.storage.setAlarm(Math.min(...deadlines)) : this.ctx.storage.deleteAlarm());
  }

  override alarm(): void {
    this.expire();
    this.schedule();
  }
}
