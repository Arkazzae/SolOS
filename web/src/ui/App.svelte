<script lang="ts">
  // The page: the first run or the three sections (Start, Library, Settings) under the top bar, a game's
  // page and windows over them, and the game itself when one is played. The keyboard and the pads drive
  // it all as the console's pad does (nav.ts, pump.ts); a game takes them over while it runs.
  import { t } from '../lib/text.svelte.ts';
  import { HUE_NONE } from './consoles.ts';
  import { dispatch, topLayer } from './layers.ts';
  import { type Action, focusFirst, keyAction } from './nav.ts';
  import Dialog from './parts/Dialog.svelte';
  import Icon from './parts/Icon.svelte';
  import Orbit from './parts/Orbit.svelte';
  import Toasts from './parts/Toasts.svelte';
  import TopBar from './parts/TopBar.svelte';
  import { gameHasInput, startPump } from './pump.ts';
  import GameSheet from './screens/GameSheet.svelte';
  import Home from './screens/Home.svelte';
  import Library from './screens/Library.svelte';
  import Player from './screens/Player.svelte';
  import Settings from './screens/Settings.svelte';
  import Setup from './screens/Setup.svelte';
  import { addFiles, library, shelves, start } from './state/library.svelte.ts';
  import { notices, toast } from './state/notify.svelte.ts';
  import { play } from './state/play.svelte.ts';
  import { prefs } from './state/prefs.svelte.ts';
  import { pwa, registerWorker, watchInstall } from './state/pwa.svelte.ts';
  import { go, SECTIONS, view } from './state/view.svelte.ts';
  import { ACCEPT } from '../engine/systems.ts';

  $effect(() => {
    const root = document.documentElement;
    root.dataset.accent = prefs.accent;
    root.dataset.font = prefs.uiFont;
    root.dataset.scale = String(prefs.uiScale);
  });

  $effect(() => {
    void start();
    watchInstall();
    void registerWorker();
    return startPump();
  });

  // In full screen the browser takes Esc to leave it; where the page may keep the key (Keyboard Lock,
  // Chrome and Edge) a short press stays Back and the game menu, and holding it leaves full screen.
  $effect(() => {
    type Lock = { lock?(keys: string[]): Promise<void>; unlock?(): void };
    const keyboard = (navigator as Navigator & { keyboard?: Lock }).keyboard;
    const changed = () => {
      view.fullscreen = document.fullscreenElement !== null;
      if (view.fullscreen) void keyboard?.lock?.(['Escape']).catch(() => {});
      else keyboard?.unlock?.();
    };
    document.addEventListener('fullscreenchange', changed);
    return () => document.removeEventListener('fullscreenchange', changed);
  });

  $effect(() => {
    if (pwa.update) toast(t.updateReady, 'download', t.updateReadyLine);
  });

  /** LB and RB between the sections, as the console's tabs. */
  function tab(action: Action): boolean {
    if (action !== 'lb' && action !== 'rb') return false;
    const at = SECTIONS.indexOf(view.section);
    const next = SECTIONS[Math.max(0, Math.min(SECTIONS.length - 1, at + (action === 'lb' ? -1 : 1)))];
    // The Library is entered at its near end: its first shelf from Start, its last from Settings.
    if (next === 'library') view.shelf = action === 'rb' ? 'all' : (shelves().at(-1) ?? 'all');
    if (next && next !== view.section) go(next);
    return true;
  }

  function onkeydown(event: KeyboardEvent): void {
    // A game that runs has the keyboard (Player.svelte, engine/input.ts).
    if (gameHasInput() || event.defaultPrevented) return;
    if (event.code === 'Tab') view.device = 'keys';
    const action = keyAction(event);
    if (!action) return;
    // Enter and Space on a focused button would click it twice: the action clicks.
    event.preventDefault();
    view.device = 'keys';
    dispatch(action);
  }

  // A file picker or another window gives the focus back to nothing: the top layer takes it again.
  function onfocus(): void {
    setTimeout(() => {
      const scope = topLayer();
      if (!play.game && scope && !scope.contains(document.activeElement)) focusFirst(scope);
    });
  }

  function onpointerdown(event: PointerEvent): void {
    if (event.pointerType === 'touch' || event.pointerType === 'pen') view.device = 'touch';
    else if (view.device === 'touch') view.device = 'keys';
  }

  // Files dropped anywhere on the page, once there is a profile to add them for.
  let dragging = $state(0);
  const takesFiles = (event: DragEvent) => (event.dataTransfer?.types.includes('Files') ?? false) && !play.game && library.profiles.length > 0;
  function ondragenter(event: DragEvent): void {
    if (takesFiles(event)) dragging++;
  }
  function ondragleave(): void {
    dragging = Math.max(0, dragging - 1);
  }
  function ondragover(event: DragEvent): void {
    if (event.dataTransfer?.types.includes('Files')) event.preventDefault();
  }
  function ondrop(event: DragEvent): void {
    event.preventDefault();
    const files = takesFiles(event) ? [...event.dataTransfer!.files] : [];
    dragging = 0;
    if (files.length) void addFiles(files);
  }

  // Games opened from the system: an installed app handles their files (manifest.webmanifest). They wait
  // until there is a profile: the library is read, or the first run has made one.
  type LaunchQueue = { setConsumer(f: (params: { files: FileSystemFileHandle[] }) => void): void };
  let launched = $state<File[]>([]);
  $effect(() => {
    (window as Window & { launchQueue?: LaunchQueue }).launchQueue?.setConsumer(async ({ files }) => {
      if (files.length) launched = [...launched, ...(await Promise.all(files.map((handle) => handle.getFile())))];
    });
  });
  $effect(() => {
    if (!library.ready || !library.profiles.length || !launched.length) return;
    const files = launched;
    launched = [];
    void addFiles(files);
  });
</script>

<svelte:window {onkeydown} {onpointerdown} {onfocus} {ondragenter} {ondragleave} {ondragover} {ondrop} />

{#if !isSecureContext}
  <div class="insecure"><span class="ic ic-lg tone-accent"><Icon name="warn" /></span><p class="h-m">{t.secureOnly}</p></div>
{:else if library.ready}
  <div class="sect" inert={!!play.game || notices.dialog !== null || view.sheet !== null} style:--hue={view.hue || HUE_NONE}>
    <div class="menu-background" class:moving={prefs.animateWallpaper} aria-hidden="true">
      {#if prefs.wallpaper === 'wave'}
        <svg class="wave" viewBox="0 0 1920 1080" preserveAspectRatio="none">
          <defs>
            <linearGradient id="wave-fill" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" style="stop-color:var(--wave)" /><stop offset=".75" style="stop-color:var(--bg)" />
            </linearGradient>
          </defs>
          <path fill="url(#wave-fill)" d="M0 600C380 420 900 150 1920 90V1080H0z" />
          <path class="wave-edge" fill="none" d="M0 600C380 420 900 150 1920 90" />
        </svg>
      {:else if prefs.wallpaper === 'orbit'}
        <Orbit moving={prefs.animateOrbit} />
      {:else}
        <div class="wallpaper" data-scene={prefs.wallpaper}></div>
      {/if}
    </div>
    {#if view.setup}
      <Setup />
    {:else}
      <TopBar />
      <main class="screen">
        {#if view.section === 'home'}
          <Home {tab} />
        {:else if view.section === 'library'}
          <Library />
        {:else}
          <Settings {tab} />
        {/if}
      </main>
    {/if}
  </div>
  {#if view.sheet && !play.game}<GameSheet />{/if}
  {#if play.game}
    {#key play.game.id}<Player game={play.game} />{/key}
  {/if}
{/if}

{#if notices.dialog}<Dialog dialog={notices.dialog} />{/if}
<Toasts />

{#if dragging > 0}
  <div class="drop">
    <div><Icon name="upload" /><b>{t.dropHere}</b><span>{t.supported}</span><small>{ACCEPT.split(',').join(' ')}</small></div>
  </div>
{/if}
