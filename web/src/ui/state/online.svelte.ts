import { Room } from '../../engine/room.svelte.ts';
import type { Game } from '../../engine/store.ts';
import { t } from '../../lib/text.svelte.ts';

const invite = new URL(location.href).searchParams.get('room')?.trim().toUpperCase();
export const validCode = (code: string) => /^[A-HJ-NP-Z2-9]{6}$/.test(code);

export const online = $state({
  open: !!invite,
  initialCode: invite ?? '',
  room: null as Room | null,
  hostProgress: false,
});

export function hostRoom(game: Game): void {
  closeRoom();
  online.hostProgress = false;
  online.room = new Room({ type: 'create', game: game.id, name: game.name.slice(0, 120) });
  online.open = true;
}

export function joinRoom(code: string): void {
  online.room?.close();
  online.room = new Room({ type: 'join', code: code.trim().toUpperCase() });
}

export function closeRoom(): void {
  online.room?.close();
  online.room = null;
  online.open = false;
  online.initialCode = '';
  const url = new URL(location.href);
  url.searchParams.delete('room');
  if (url.href !== location.href) history.replaceState(history.state, '', url);
}

export function inviteLink(code: string): string {
  const url = new URL(location.href);
  url.searchParams.set('room', code);
  return url.href;
}

export function roomStatus(room: Room): string {
  if (room.phase === 'error') {
    switch (room.reason) {
      case 'version': return t.roomVersion;
      case 'full': return t.roomFull;
      case 'missing': return t.roomMissing;
      case 'ended': return t.roomEnded;
      case 'unavailable': return t.roomUnavailable;
      default: return t.roomFailed;
    }
  }
  if (room.phase === 'connecting') return t.roomConnecting;
  if (room.phase === 'reconnecting') return t.roomReconnect;
  if (room.phase === 'sync') return t.roomSync;
  return t.roomWait;
}
