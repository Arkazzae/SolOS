<script lang="ts">
  // The bar on top of Start, the Library and Settings: the mark, the sections with LB and RB round them,
  // and the status pill (the profile when there are two or more, the pads, the clock).
  import { t } from '../../lib/text.svelte.ts';
  import { dispatch } from '../layers.ts';
  import { currentProfile, library, profileName } from '../state/library.svelte.ts';
  import { go, type Section, view } from '../state/view.svelte.ts';
  import Brand from './Brand.svelte';
  import Glyph from './Glyph.svelte';
  import Icon from './Icon.svelte';

  const sections = $derived<[Section, string][]>([['home', t.home], ['library', t.library], ['settings', t.settings]]);
  let now = $state(new Date());
  $effect(() => {
    const timer = setInterval(() => (now = new Date()), 10_000);
    return () => clearInterval(timer);
  });
</script>

<header class="topbar">
  <button class="brand-btn" tabindex="-1" aria-label={t.home} onclick={() => go('home')}><Brand /></button>
  <nav class="tabs" aria-label="Sol">
    {#if view.device !== 'touch'}<button class="tab-g" tabindex="-1" aria-hidden="true" onclick={() => dispatch('lb')}><Glyph action="lb" /></button>{/if}
    {#each sections as [id, label] (id)}
      <button class="tab" class:is-on={view.section === id} aria-current={view.section === id ? 'page' : undefined} onclick={() => go(id)}>{label}</button>
    {/each}
    {#if view.device !== 'touch'}<button class="tab-g" tabindex="-1" aria-hidden="true" onclick={() => dispatch('rb')}><Glyph action="rb" /></button>{/if}
  </nav>
  <div class="status">
    {#if library.profiles.length > 1}<span class="status-user"><Icon name="user" /><span>{profileName(currentProfile())}</span></span>{/if}
    {#if view.pads > 0}<span class="status-pads" title={t.pads(view.pads)}><Icon name="pad" /><span>{view.pads}</span></span>{/if}
    <span class="status-clock">{t.clock(now)}</span>
  </div>
</header>
