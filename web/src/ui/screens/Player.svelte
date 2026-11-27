<script lang="ts">
  // A game on the whole page: its picture, a bar that fades while the mouse rests, the fast forward badge
  // and the counter in a corner, and the game menu as the console's (.ig-row): back to the game, save and
  // load the state, start over; the picture look and fast forward (◂ ▸), the counter and full screen
  // (switches); leaving. The session (engine/session.ts) runs the game; this is everything around it.
  import { tick, untrack } from 'svelte';
  import { Session, type SessionEvents, type Status } from '../../engine/session.ts';
  import { SHADERS } from '../../engine/shaders.ts';
  import type { Speaker } from '../../engine/sound.ts';
  import type { Game } from '../../engine/store.ts';
  import { SYSTEMS } from '../../engine/systems.ts';
  import type { IconName } from '../../lib/icons.ts';
  import { t } from '../../lib/text.svelte.ts';
  import { buttonKeys, commonGameKeys } from '../keys.ts';
  import { layer } from '../layers.ts';
  import { focusFirst } from '../nav.ts';
  import type { Action } from '../nav.ts';
  import Hints from '../parts/Hints.svelte';
  import InviteLink from '../parts/InviteLink.svelte';
  import Icon from '../parts/Icon.svelte';
  import { online, roomStatus } from '../state/online.svelte.ts';
  import { endGame, opened, sound } from '../state/play.svelte.ts';
  import { toast } from '../state/notify.svelte.ts';
  import { prefs, setPrefs } from '../state/prefs.svelte.ts';
  import { giveTo, input } from '../pump.ts';

  type Act = 'back' | 'save' | 'load' | 'restart' | 'shader' | 'speed' | 'perf' | 'fullscreen' | 'exit';
  /** The menu's rows: a group, then the next after a gap. */
  const ROWS: { act: Act; icon: IconName; gap?: boolean }[] = [
    { act: 'back', icon: 'play' },
    { act: 'save', icon: 'save' },
    { act: 'load', icon: 'history' },
    { act: 'restart', icon: 'restart' },
    { act: 'shader', icon: 'frame', gap: true },
    { act: 'speed', icon: 'bolt' },
    { act: 'perf', icon: 'info' },
    { act: 'fullscreen', icon: 'frame' },
    { act: 'exit', icon: 'close', gap: true },
  ];
  /** How long the bar stays after the mouse stops (ms). */
  const IDLE_AFTER = 2_500;

  let { game }: { game: Game } = $props();
  const system = $derived(SYSTEMS[game.system]);

  let root: HTMLElement | undefined = $state();
  let canvas: HTMLCanvasElement | undefined = $state();
  let menuEl: HTMLElement | undefined = $state();
  let session = $state.raw<Session | null>(null);
  let overlay = $state<HTMLElement>();
  let speaker: Speaker | null = null;
  let status = $state<Status>({ failed: null, fast: 0, silent: false, slot: false, stats: null });
  let menu = $state(false);
  let fast = $state(0);
  let stats = $state(prefs.perf || new URLSearchParams(location.search).has('stats'));
  let full = $state(false);
  let muted = $state(false);
  let idle = $state(false);
  let focusedAct = $state<Act | null>(null);

  const events: SessionEvents = {
    status: (next) => (status = next),
    menu: () => openMenu(),
    saveFailed: (error) => {
      console.error('saving failed:', error);
      toast(t.saveFailed, 'warn', undefined, 'danger');
    },
    failed: (error) => {
      online.room?.close();
      toast(t.failed(error), 'warn', undefined, 'danger');
      openMenu();
    },
  };

  const play = (now: number, poll: Parameters<Session['frame']>[1]) => session?.frame(now, poll);

  // The game starts when the canvas is there, and stops with the Player.
  $effect(() => {
    const target = canvas;
    if (!target) return;
    let gone = false;
    let started: Session | null = null;
    untrack(() => {
      const options = { shader: prefs.shader, quickResume: prefs.quickResume, room: online.room, hostProgress: online.hostProgress };
      void (async () => {
        try {
          speaker = await sound();
          started = await Session.open(target, game, speaker, input, options, events);
          if (gone) return started.stop();
          session = started;
          session.stats = stats;
          if (session.resumed) toast(t.resumed, 'play');
          opened();
        } catch (error) {
          // A game left while it was starting has nothing more to say.
          if (gone) return;
          toast(t.openFailed(error instanceof Error ? error.message : String(error)), 'warn', undefined, 'danger');
          void endGame();
        }
      })();
    });
    return () => {
      gone = true;
      giveTo(null);
      started?.stop();
    };
  });

  // While waiting, the same keyboard and pad that drive the library navigate
  // the room actions. Only a running game owns gameplay input.
  $effect(() => {
    if (!session) return;
    const running = !menu && (!online.room || online.room.phase === 'playing');
    input.keyboard = running;
    giveTo(running ? play : null);
    if (!menu && overlay) focusFirst(overlay);
  });

  function roomAction(action: Action): boolean {
    if (action === 'b') { void leave(); return true; }
    return false;
  }

  // Moving the mouse brings the bar back; it fades when the mouse rests.
  $effect(() => {
    if (idle) return;
    const timer = setTimeout(() => (idle = true), IDLE_AFTER);
    return () => clearTimeout(timer);
  });

  function row(act: Act): HTMLButtonElement | null {
    return menuEl?.querySelector<HTMLButtonElement>(`.ig-row[data-act="${act}"]`) ?? null;
  }

  async function openMenu(): Promise<void> {
    if (menu || !session) return;
    menu = true;
    giveTo(null);
    session.pause();
    await tick();
    row(status.failed ? (online.room ? 'exit' : 'restart') : 'back')?.focus();
  }

  function closeMenu(): void {
    if (!menu || !session || status.failed) return;
    menu = false;
    (document.activeElement as HTMLElement | null)?.blur();
    session.resume();
  }

  async function leave(): Promise<void> {
    // Still starting: nothing to save, and the session stops with the Player.
    if (!session) return void endGame();
    if (!(await session.leave())) return;
    giveTo(null);
    // Full screen the game asked for ends with it; the page's own (Settings) stays.
    if (document.fullscreenElement === root) void document.exitFullscreen();
    void endGame();
  }

  function fullscreen(on = document.fullscreenElement !== root): void {
    if (!on && document.fullscreenElement) void document.exitFullscreen();
    else if (on) void root?.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
  }

  function setStats(on: boolean): void {
    stats = on;
    if (session) session.stats = on;
    setPrefs({ perf: on });
  }

  function toggleMute(): void {
    if (!speaker) return;
    speaker.muted = !speaker.muted;
    muted = speaker.muted;
  }

  /** A row's value turned a step left (-1) or right (1), as ◂ ▸ on the console. */
  function turn(act: Act, step: number): void {
    switch (act) {
      case 'shader': {
        const shader = SHADERS[(SHADERS.indexOf(prefs.shader) + step + SHADERS.length) % SHADERS.length]!;
        setPrefs({ shader });
        session?.setShader(shader);
        return;
      }
      case 'speed':
        if (!session) return;
        session.fast += step;
        fast = session.fast;
        return;
      case 'perf':
        return setStats(step > 0);
      case 'fullscreen':
        return fullscreen(step > 0);
    }
  }

  async function choose(act: Act): Promise<void> {
    speaker?.resume();
    switch (act) {
      case 'back':
        return closeMenu();
      case 'save':
        if (await session?.saveSlot()) {
          toast(t.saved, 'save');
          closeMenu();
        }
        return;
      case 'load':
        if (await session?.loadSlot()) {
          toast(t.loaded, 'history');
          closeMenu();
        }
        return;
      case 'restart':
        await session?.restart();
        return closeMenu();
      case 'shader':
      case 'speed':
        return turn(act, 1);
      case 'perf':
        return setStats(!stats);
      case 'fullscreen':
        return fullscreen();
      case 'exit':
        return leave();
    }
  }

  /** Up and down go round the rows that can be chosen. */
  function step(by: number): void {
    const rows = [...(menuEl?.querySelectorAll<HTMLButtonElement>('.ig-row:not(:disabled)') ?? [])];
    const at = rows.indexOf(document.activeElement as HTMLButtonElement);
    rows[(at + by + rows.length) % rows.length]?.focus();
  }

  /** The menus' actions while the game menu is open; before that, nothing gets past the game. */
  function handle(action: Action): boolean {
    if (!menu) return true;
    switch (action) {
      case 'up': step(-1); return true;
      case 'down': step(1); return true;
      case 'left': case 'right':
        if (focusedAct) turn(focusedAct, action === 'left' ? -1 : 1);
        return true;
      case 'b': case 'menu': closeMenu(); return true;
      // A clicks the row in focus (layers.ts).
      case 'a': return false;
      default: return true;
    }
  }

  /** The keys of a game that runs: the menu, full screen and the counter; the rest are the pad's
   * (engine/input.ts). While the menu is open the keyboard drives it as the menus' (App.svelte), and a
   * key the menu took (Esc closing it) is not the game's too. */
  function onkeydown(event: KeyboardEvent): void {
    speaker?.resume();
    if (menu || overlay || !session || event.defaultPrevented) return;
    if (event.code === 'Escape') {
      event.preventDefault();
      void openMenu();
    } else if (event.code === 'KeyF' && !event.repeat) {
      event.preventDefault();
      fullscreen();
    } else if (event.code === 'Backquote' && !event.repeat) {
      event.preventDefault();
      setStats(!stats);
    }
  }

  const label = (act: Act): string =>
    ({
      back: t.backToGame, save: t.saveState, load: t.loadState, restart: t.restart, shader: t.pictureLook,
      speed: t.speedName, perf: t.perfCounter, fullscreen: t.fullScreen, exit: t.exit,
    })[act];

  const help = $derived.by(() => {
    switch (focusedAct) {
      case 'save': return t.saveStateLine;
      case 'load': return status.slot ? t.loadStateLine : t.noState;
      case 'restart': return t.restartLine;
      case 'shader': return t.shaderLine;
      case 'speed': return t.speedLine;
      case 'perf': return t.perfLine;
      case 'fullscreen': return t.fullscreenLine;
      case 'exit': return t.exitLine;
      default: return '';
    }
  });

  const onlineHidden: Act[] = ['save', 'load', 'restart', 'speed'];
  const rows = $derived(online.room ? ROWS.filter(row => !onlineHidden.includes(row.act)) : ROWS);
  const disabled = (act: Act) => (act === 'load' && !status.slot) || (!!status.failed && (act === 'back' || act === 'save' || act === 'load'));

  const statsLine = $derived.by(() => {
    const s = status.stats;
    if (!s) return '';
    return `${s.fps.toFixed(1)} fps · ${s.speed.toFixed(0)} % · ${s.refresh.toFixed(1)} Hz ${s.locked ? 'vsync' : 'clock'} · ` +
      `${s.work.toFixed(1)} ms/run · sound ${s.sound.toFixed(0)} ms, ${s.dry} dry`;
  });
  const keys = $derived([commonGameKeys()[0]!, ...buttonKeys(system), ...commonGameKeys().slice(1)]);
</script>

<svelte:window {onkeydown} onpointerdown={() => speaker?.resume()} />
<svelte:document onfullscreenchange={() => (full = document.fullscreenElement === root)} />

<section class="player" class:idle={idle && !menu} class:is-menu={menu} role="application" aria-label={game.name}
  bind:this={root} use:layer={handle} onpointermove={() => (idle = false)}>
  <canvas class="screen" bind:this={canvas}></canvas>
  {#if online.room && online.room.phase !== 'playing' && !menu}
    <div class="room-overlay" role="dialog" aria-modal="true" aria-label={t.onlinePlay} bind:this={overlay} use:layer={roomAction}><b role="status">{roomStatus(online.room)}</b>
      {#if online.room.code}<span>{t.roomCode} · {online.room.code} · P{online.room.seat + 1}</span>{/if}
      {#if online.room.phase === 'waiting' && online.room.seat === 0}<InviteLink code={online.room.code} />{/if}
      <button class="btn" data-nav onclick={leave}>{t.exit}</button>
    </div>
  {/if}
  {#if online.room?.phase === 'playing' && !menu}
    {@const note = online.room.localSlow ? t.roomLocalSlow : online.room.peerSlow ? t.roomPeerSlow : online.room.networkWait ? t.roomNetworkWait : ''}
    {#if note}<div class="room-health" role="status">{note}</div>{/if}
  {/if}
  <header class="bar">
    <button class="round" title={t.exit} aria-label={t.exit} onclick={leave}><Icon name="close" /></button>
    <div class="bar-title"><b>{game.name}</b><span>{system.short}</span></div>
    <div class="bar-tools">
      <button class="round" title={muted ? t.unmute : t.mute} aria-label={muted ? t.unmute : t.mute} onclick={toggleMute}>
        <Icon name={muted ? 'speakerOff' : 'speaker'} />
      </button>
      <button class="round" title={t.fullScreen} aria-label={t.fullScreen} onclick={() => fullscreen()}><Icon name="frame" /></button>
      <button class="round" title={t.menu} aria-label={t.menu} onclick={openMenu}><Icon name="pause" /></button>
    </div>
  </header>
  {#if status.silent && !menu}<div class="notice">{t.soundOff}</div>{/if}
  <div class="play-corner">
    {#if status.fast > 0}<div class="fast"><Icon name="bolt" /><span>{t.speed(status.fast)}</span></div>{/if}
    {#if stats && statsLine}<div class="stats">{statsLine}</div>{/if}
  </div>

  {#if menu}
    <div class="ig" bind:this={menuEl}>
      <div class="ig-veil"></div><div class="ig-solid"></div><div class="ig-fade"></div>
      <div class="ig-panel" role="dialog" aria-modal="true" aria-label={t.menu}>
        <p class="eyebrow">{system.full}</p>
        <h2 class="h-l ig-title">{game.name}</h2>
        <div class="ig-list">
          {#each rows as { act, icon, gap } (act)}
            <button class="ig-row" class:ig-gap={gap} data-act={act} disabled={disabled(act)}
              onfocus={() => (focusedAct = act)} onclick={() => choose(act)}>
              <span class="ig-ic"><Icon name={icon} /></span><span class="ig-k">{label(act)}</span>
              <span class="ig-v">
                {#if act === 'shader'}
                  <i class="arr">◂</i><span>{t.shader(prefs.shader)}</span><i class="arr">▸</i>
                {:else if act === 'speed'}
                  <i class="arr">◂</i><span>{t.speed(fast)}</span><i class="arr">▸</i>
                {:else if act === 'perf'}
                  <span class="switch" class:is-on={stats}></span>
                {:else if act === 'fullscreen'}
                  <span class="switch" class:is-on={full}></span>
                {/if}
              </span>
            </button>
          {/each}
        </div>
        <p class="ig-help">{help}</p>
        <div class="ig-hints"><Hints hints={[{ actions: ['a'], label: t.select }, { actions: ['b'], label: t.backToGame }]} /></div>
      </div>
      <aside class="ig-keys">
        <p class="eyebrow">{t.keyboard} · {system.name}</p>
        <div class="keys">{#each keys as [key, what] (key)}<div><kbd>{key}</kbd><span>{what}</span></div>{/each}</div>
      </aside>
    </div>
  {/if}
</section>

<style>
  .room-health { position: absolute; left: 50%; bottom: 6%; transform: translateX(-50%); background: #111e; color: white; padding: .65em 1em; text-align: center; pointer-events: none; }
  .room-overlay { position: absolute; inset: 0; z-index: 2; display: grid; place-content: center;
    justify-items: center; gap: 1.6rem; padding: 2rem; background: #111d; color: white; text-align: center; }
</style>
