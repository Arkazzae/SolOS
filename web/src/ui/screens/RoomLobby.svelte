<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { t } from '../../lib/text.svelte.ts';
  import InviteLink from '../parts/InviteLink.svelte';
  import { layer } from '../layers.ts';
  import { focusFirst, type Action } from '../nav.ts';
  import { addFiles, gameById, pickFiles } from '../state/library.svelte.ts';
  import { toast } from '../state/notify.svelte.ts';
  import { closeRoom, joinRoom, online, roomStatus, validCode } from '../state/online.svelte.ts';
  import { play, startGame } from '../state/play.svelte.ts';

  let code = $state(online.initialCode);
  let room = $derived(online.room);
  let game = $derived(gameById(room?.game ?? null));
  let root: HTMLElement;
  let field = $state<HTMLInputElement>();
  onMount(() => {
    if (!online.room && validCode(online.initialCode)) joinRoom(online.initialCode);
  });
  let picking = $state(false);

  $effect(() => {
    const phase = room?.phase;
    void tick().then(() => {
      if (phase === undefined) field?.focus();
      else if (!root?.contains(document.activeElement)) focusFirst(root);
    });
  });

  function join() {
    if (validCode(code.trim().toUpperCase())) joinRoom(code);
  }

  function handle(action: Action) {
    if (action === 'b') { if (!play.opening) closeRoom(); return true; }
    if (action === 'a' && document.activeElement === field) { join(); return true; }
    return false;
  }

  async function pick() {
    const current = room;
    if (!current || picking) return;
    picking = true;
    try {
      const files = await pickFiles('.nes', false);
      if (files.length) {
        await addFiles(files);
        if (!gameById(current.game)) toast(t.roomWrongFile, 'warn');
      }
    } finally { picking = false; }
  }
</script>

<div class="veil"></div>
<div class="modal" role="dialog" aria-modal="true" aria-labelledby="room-title" use:layer={handle} bind:this={root}>
  <div class="modal-body">
    <h2 id="room-title" class="h-m">{room ? t.onlinePlay : t.joinRoom}</h2>
    {#if !room}
      <label>{t.roomCode}<input class="field" bind:this={field} bind:value={code} maxlength="6" autocomplete="off" autocapitalize="characters" spellcheck="false" data-nav /></label>
      <button class="btn" data-nav disabled={!validCode(code.trim().toUpperCase())} onclick={join}>{t.joinRoom}</button>
    {:else if room.phase === 'waiting' && room.code}
      <p class="lead">{room.name} · P{room.seat + 1}</p>
      <p>{t.roomCode} <strong class="room-code">{room.code}</strong></p>
      {#if room.seat === 0}
        <InviteLink code={room.code} />
      {/if}
      {#if game}
        <p>{t.roomTemporary}</p>
        {#if room.seat === 0}<label><input type="checkbox" data-nav bind:checked={online.hostProgress} /> {t.roomProgress}</label>{/if}
        <button class="btn" data-nav disabled={play.opening} onclick={() => game && startGame(game.id)}>{t.roomReady}</button>
      {:else}
        <p>{t.roomFile}</p>
        <button class="btn" data-nav disabled={picking} onclick={pick}>{t.chooseFiles}</button>
      {/if}
    {:else}
      <p class="lead" role="status">{roomStatus(room)}</p>
    {/if}
    <div class="modal-acts"><button class="btn" data-nav disabled={play.opening} onclick={closeRoom}>{t.back}</button></div>
  </div>
</div>

<style>
  .modal-body { display: grid; gap: 1.4rem; min-width: 0; }
  .modal-body > .btn { justify-self: start; }
  .room-code { letter-spacing: .14em; user-select: all; }
  label { display: block; }
</style>
