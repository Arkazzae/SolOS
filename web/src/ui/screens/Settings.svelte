<script lang="ts">
  // Settings, as the console's two-column page: categories on the left, the chosen one's rows on the
  // right (a choice with ◂ ▸, a switch, an action, information), the focused row's line under them. Only
  // what the page really does in a browser: no TV, Wi-Fi, pairing, sleep or power.
  import { SHADERS } from '../../engine/shaders.ts';
  import * as store from '../../engine/store.ts';
  import { SYSTEM_IDS, SYSTEMS } from '../../engine/systems.ts';
  import type { IconName } from '../../lib/icons.ts';
  import { LANG_NAMES, LANGS, t } from '../../lib/text.svelte.ts';
  import { HUE_NONE } from '../consoles.ts';
  import { allGameKeys, menuKeys } from '../keys.ts';
  import { layer } from '../layers.ts';
  import { type Action, focus, type Hint, move } from '../nav.ts';
  import Hints from '../parts/Hints.svelte';
  import Icon from '../parts/Icon.svelte';
  import {
    addProfile, clearAll, library, pickGames, profileName, removeAll, removeProfile, renameProfile, switchProfile,
  } from '../state/library.svelte.ts';
  import { showKeys, toast } from '../state/notify.svelte.ts';
  import { prefs, setPrefs } from '../state/prefs.svelte.ts';
  import { install, pwa } from '../state/pwa.svelte.ts';
  import { view } from '../state/view.svelte.ts';

  type Row =
    | { kind: 'choice'; key: string; label: string; line?: string; values: string[]; index: number; set(i: number): void }
    | { kind: 'toggle'; key: string; label: string; line?: string; on: boolean; set(on: boolean): void }
    | { kind: 'action'; key: string; label: string; line?: string; value?: string; danger?: boolean; run(): void }
    | { kind: 'info'; key: string; label: string; line?: string; value: string; warn?: boolean };
  interface Category { id: string; name: string; icon: IconName; lead: string; rows: Row[] }

  let { tab }: { tab: (action: Action) => boolean } = $props();

  let space = $state<{ used: number; quota: number } | null>(null);
  let kept = $state(false);
  $effect(() => {
    void library.games.length;
    void store.space().then((s) => (space = s));
    void store.persisted().then((p) => (kept = p));
  });

  async function keep(): Promise<void> {
    kept = await store.persist();
    if (!kept) toast(t.keepRefused, 'info');
  }

  async function installApp(): Promise<void> {
    if (!(await install())) toast(t.installAppLine, 'download');
  }

  async function toggleFullscreen(): Promise<void> {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else await document.documentElement.requestFullscreen({ navigationUI: 'hide' });
    } catch {
      // Not allowed here (an iPhone, a frame).
    }
  }

  const offline = $derived(
    pwa.offline === 'ready' ? t.offlineReady : pwa.offline === 'loading' ? t.offlineLoading : isSecureContext ? '—' : t.offlineNo,
  );

  const categories = $derived<Category[]>([
    {
      id: 'picture', name: t.picture, icon: 'frame', lead: t.pictureLead, rows: [
        { kind: 'choice', key: 'shader', label: t.pictureLook, line: t.pictureLookLine, values: SHADERS.map((s) => t.shader(s)),
          index: SHADERS.indexOf(prefs.shader), set: (i) => setPrefs({ shader: SHADERS[i] }) },
        { kind: 'toggle', key: 'perf', label: t.perfCounter, line: t.perfCounterLine, on: prefs.perf, set: (on) => setPrefs({ perf: on }) },
        { kind: 'toggle', key: 'full', label: t.fullScreen, line: t.fullScreenLine, on: view.fullscreen, set: () => void toggleFullscreen() },
      ],
    },
    {
      id: 'controls', name: t.controls, icon: 'pad', lead: t.controlsLead, rows: [
        { kind: 'info', key: 'pads', label: t.padsConnected, line: t.padsConnectedLine, value: view.pads ? t.pads(view.pads) : t.noPads },
        { kind: 'action', key: 'game-keys', label: t.keysInGames, line: t.keysInGamesLine, run: () => showKeys(t.keysInGames, allGameKeys()) },
        { kind: 'action', key: 'menu-keys', label: t.keysInMenus, line: t.keysInMenusLine, run: () => showKeys(t.keysInMenus, menuKeys()) },
      ],
    },
    {
      id: 'profiles', name: t.profiles, icon: 'user', lead: t.profilesLead, rows: [
        ...library.profiles.map((p): Row => ({
          kind: 'action', key: `profile-${p.id}`, label: profileName(p), line: p.id === prefs.profile ? undefined : t.switchTo,
          value: p.id === prefs.profile ? t.active : undefined, run: () => void switchProfile(p.id),
        })),
        ...(library.profiles.length < store.PROFILES_MAX
          ? [{ kind: 'action', key: 'add-profile', label: t.addProfile, line: t.addProfileLine, run: () => void addProfile() } satisfies Row]
          : []),
        { kind: 'action', key: 'rename', label: t.rename, line: t.renameLine, run: () => void renameProfile() },
        ...(library.profiles.length > 1
          ? [{ kind: 'action', key: 'remove-profile', label: t.removeProfile, line: t.removeProfileLine, danger: true, run: () => void removeProfile() } satisfies Row]
          : []),
      ],
    },
    {
      id: 'games', name: t.gamesSection, icon: 'cards', lead: t.gamesLead, rows: [
        { kind: 'action', key: 'add', label: t.addGames, line: t.addGamesLine2, run: () => void pickGames() },
        { kind: 'info', key: 'consoles', label: t.supportedConsoles, line: t.supported, value: SYSTEM_IDS.map((id) => SYSTEMS[id].short).join(' · ') },
        { kind: 'info', key: 'space', label: t.space, line: t.spaceLine, value: space ? t.spaceValue(space.used, space.quota) : '—' },
        kept
          ? { kind: 'info', key: 'keep', label: t.keepData, line: t.keepDataLine, value: t.kept }
          : { kind: 'action', key: 'keep', label: t.keepData, line: t.keepDataLine, value: t.askBrowser, run: () => void keep() },
        { kind: 'action', key: 'remove-all', label: t.removeAll, line: t.removeAllLine, danger: true, run: () => void removeAll() },
      ],
    },
    {
      id: 'saves', name: t.savesSection, icon: 'save', lead: t.savesLead, rows: [
        { kind: 'toggle', key: 'qr', label: t.quickResume, line: t.quickResumeLine, on: prefs.quickResume, set: (on) => setPrefs({ quickResume: on }) },
        { kind: 'info', key: 'where', label: t.savesWhere, line: t.savesWhereLine, value: t.savesWhereValue },
      ],
    },
    {
      id: 'system', name: t.system, icon: 'gear', lead: t.systemLead, rows: [
        { kind: 'choice', key: 'lang', label: t.language, values: LANGS.map((l) => LANG_NAMES[l]), index: LANGS.indexOf(prefs.lang),
          set: (i) => setPrefs({ lang: LANGS[i] }) },
        pwa.installed
          ? { kind: 'info', key: 'install', label: t.installApp, line: t.installAppLine, value: t.installed }
          : { kind: 'action', key: 'install', label: t.installApp, line: t.installAppLine, value: pwa.installable ? t.install : t.installFromMenu, run: () => void installApp() },
        { kind: 'info', key: 'offline', label: t.offline, line: t.offlineLine, warn: pwa.offline === 'none' && !isSecureContext, value: offline },
        { kind: 'info', key: 'version', label: t.version, value: __SOL_VERSION__ },
        { kind: 'info', key: 'licences', label: t.licences, line: t.licencesLine, value: t.licencesValue },
        { kind: 'action', key: 'clear', label: t.clearAll, line: t.clearAllLine, danger: true, run: () => void clearAll() },
      ],
    },
  ]);

  let active = $state('picture');
  const category = $derived(categories.find((c) => c.id === active) ?? categories[0]!);
  let focusedKey = $state('');
  const focusedRow = $derived(category.rows.find((r) => r.key === focusedKey));
  let root: HTMLElement | undefined = $state();
  let cats: HTMLElement | undefined = $state();
  let page: HTMLElement | undefined = $state();
  const inRows = () => !!page?.contains(document.activeElement);

  const toCategory = () => {
    const current = cats?.querySelector<HTMLElement>(`[data-cat="${active}"]`);
    if (current) focus(current);
  };
  const toRows = () => {
    const first = page?.querySelector<HTMLElement>('[data-nav]');
    if (first) focus(first);
  };

  $effect(() => {
    view.hue = HUE_NONE;
    if (!root?.contains(document.activeElement)) toCategory();
  });

  function turn(row: Row, by: number): void {
    if (row.kind === 'choice') row.set((row.index + by + row.values.length) % row.values.length);
    else if (row.kind === 'toggle') row.set(by > 0);
  }

  function choose(row: Row): void {
    if (row.kind === 'choice') turn(row, 1);
    else if (row.kind === 'toggle') row.set(!row.on);
    else if (row.kind === 'action') row.run();
  }

  function handle(action: Action): boolean {
    if (tab(action)) return true;
    // The two columns side by side: the stylesheet's TV layout (sol.css).
    const wide = matchMedia('(min-width: 1000px) and (min-height: 600px)').matches;
    if (inRows()) {
      const row = focusedRow;
      switch (action) {
        case 'up': case 'down': move(page!, action); return true;
        case 'left': case 'right':
          if (row && (row.kind === 'choice' || row.kind === 'toggle')) turn(row, action === 'left' ? -1 : 1);
          else if (action === 'left') toCategory();
          return true;
        case 'b': toCategory(); return true;
        case 'a': if (row) choose(row); return true;
        default: return false;
      }
    }
    switch (action) {
      case 'up': case 'down':
        if (wide) move(cats!, action);
        else if (action === 'down') toRows();
        return true;
      case 'left': case 'right':
        if (!wide) move(cats!, action);
        else if (action === 'right') toRows();
        return true;
      case 'a': toRows(); return true;
      case 'b': return tab('lb');
      default: return false;
    }
  }

  const hints = $derived<Hint[]>([
    focusedRow?.kind === 'choice' ? { actions: ['a'], label: t.change }
    : focusedRow?.kind === 'toggle' ? { actions: ['a'], label: focusedRow.on ? t.turnOff : t.turnOn }
    : { actions: ['a'], label: t.open },
    { actions: ['b'], label: focusedRow ? t.back : t.home },
    { actions: ['lb', 'rb'], label: t.sections },
  ]);
</script>

<div class="set" bind:this={root} use:layer={handle}>
  <nav class="set-cats" bind:this={cats} aria-label={t.settings}>
    {#each categories as c (c.id)}
      <button class="set-cat" class:is-on={c.id === active} data-nav data-cat={c.id}
        onfocus={() => { active = c.id; focusedKey = ''; }} onclick={() => (active = c.id)}>
        <Icon name={c.icon} /><span>{c.name}</span>
      </button>
    {/each}
  </nav>

  <section class="set-page" bind:this={page}>
    <header class="set-head">
      <h1 class="h-l">{category.name}</h1>
      <p class="lead">{category.lead}</p>
    </header>
    <div class="set-list">
      {#each category.rows as row (row.key)}
        <button class="set-row" class:is-danger={row.kind === 'action' && row.danger} class:is-warn={row.kind === 'info' && row.warn}
          class:is-info={row.kind === 'info'} data-nav onfocus={() => (focusedKey = row.key)} onclick={() => choose(row)}>
          <span class="set-k">{row.label}</span>
          {#if row.kind === 'choice'}
            <span class="set-v"><i class="arr" aria-hidden="true">◂</i>{row.values[row.index]}<i class="arr" aria-hidden="true">▸</i></span>
          {:else if row.kind === 'toggle'}
            <span class="switch" class:is-on={row.on} role="switch" aria-checked={row.on}></span>
          {:else if row.kind === 'action'}
            {#if row.value}<span class="set-v">{row.value}</span>{/if}
            <span class="set-chev"><Icon name="next" /></span>
          {:else}
            <span class="set-v">{row.value}</span>
          {/if}
        </button>
      {/each}
    </div>
    <p class="set-help">{focusedRow?.line ?? ''}</p>
  </section>

  <div class="set-hints"><Hints {hints} /></div>
</div>
