<script lang="ts">
  // A game's page, over the section it was opened from (Menu, ≡): its picture and facts, and what can be
  // done with it here. B closes it.
  import { SYSTEMS } from '../../engine/systems.ts';
  import type { IconName } from '../../lib/icons.ts';
  import { t } from '../../lib/text.svelte.ts';
  import { epic, hue } from '../consoles.ts';
  import { layer } from '../layers.ts';
  import { type Action, focusFirst } from '../nav.ts';
  import Cover from '../parts/Cover.svelte';
  import Hints from '../parts/Hints.svelte';
  import Icon from '../parts/Icon.svelte';
  import { daysAgo, downloadSave, gameById, removeGame, toggleFav, uploadSave } from '../state/library.svelte.ts';
  import { startGame, startOver } from '../state/play.svelte.ts';
  import { prefs } from '../state/prefs.svelte.ts';
  import { view } from '../state/view.svelte.ts';

  interface Act { key: string; icon: IconName; label: string; line?: string; danger?: boolean; run(): void }

  const game = $derived(gameById(view.sheet));
  let root: HTMLElement | undefined = $state();
  let help = $state('');

  $effect(() => {
    if (root) focusFirst(root);
  });
  // The game removed from under its page closes it.
  $effect(() => {
    if (!game) view.sheet = null;
  });

  const close = () => (view.sheet = null);

  function handle(action: Action): boolean {
    if (action !== 'b' && action !== 'menu') return false;
    close();
    return true;
  }

  const acts = $derived.by<Act[]>(() => {
    if (!game) return [];
    const { id } = game;
    const resume = game.resume && prefs.quickResume;
    return [
      { key: 'play', icon: 'play', label: resume ? t.resume : t.play, run: () => void startGame(id) },
      ...(resume ? [{ key: 'over', icon: 'restart', label: t.startOver, line: t.startOverLine, run: () => void startOver(id) } satisfies Act] : []),
      { key: 'fav', icon: game.fav ? 'star' : 'starLine', label: t.favourite, line: t.favouritesLine, run: () => void toggleFav(id) },
      { key: 'down', icon: 'download', label: t.downloadSave, line: t.downloadSaveLine, run: () => void downloadSave(id) },
      { key: 'up', icon: 'upload', label: t.uploadSave, line: t.uploadSaveLine, run: () => void uploadSave(id) },
      { key: 'remove', icon: 'trash', label: t.removeGame, line: t.removeGameLine, danger: true, run: () => void removeGame(id) },
    ];
  });
</script>

{#if game}
  <div class="veil" role="presentation" onclick={close}></div>
  <div class="sheet" role="dialog" aria-modal="true" aria-label={game.name} style:--hue={hue(game.system)} bind:this={root} use:layer={handle}>
    <div class="sheet-art"><Cover {game} large /></div>
    <div class="sheet-body">
      <p class="eyebrow">{SYSTEMS[game.system].full} · {epic(game.system)}</p>
      <h2 class="h-l">{game.name}{#if game.fav}<span class="sheet-star"><Icon name="star" /></span>{/if}</h2>
      <p class="hero-meta">{[game.tags, t.size(game.size), t.addedOn(t.date(game.added))].filter(Boolean).join(' · ')}</p>
      <p class="lib-played">{game.played ? t.playedAgo(t.ago(daysAgo(game.played))) : t.notPlayed}</p>
      <div class="group">
        {#each acts as act (act.key)}
          <button class="row" class:is-danger={act.danger} data-nav data-first={act.key === 'play' ? '' : undefined}
            onfocus={() => (help = act.line ?? '')} onclick={act.run}>
            <span class="row-ic"><Icon name={act.icon} /></span><span class="row-k">{act.label}</span>
          </button>
        {/each}
      </div>
      <p class="set-help">{help}</p>
      <Hints hints={[{ actions: ['a'], label: t.select }, { actions: ['b'], label: t.back }]} />
    </div>
  </div>
{/if}
