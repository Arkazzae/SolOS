<script lang="ts">
  // Start, as the console's: the headline column on the left with the hints, a rail of games (Quick
  // Resume and the last played first) and a rail of the library's consoles. With no games yet, Welcome:
  // where games come from, and the button that brings them.
  import type { Game } from '../../engine/store.ts';
  import { SYSTEMS } from '../../engine/systems.ts';
  import { t } from '../../lib/text.svelte.ts';
  import { art, epic, HUE_ALL, HUE_NONE, hue } from '../consoles.ts';
  import { layer } from '../layers.ts';
  import { type Action, focusFirst, type Hint } from '../nav.ts';
  import ConsoleList from '../parts/ConsoleList.svelte';
  import Cover from '../parts/Cover.svelte';
  import Glyph from '../parts/Glyph.svelte';
  import Hints from '../parts/Hints.svelte';
  import Icon from '../parts/Icon.svelte';
  import { consolesWithGames, currentProfile, daysAgo, library, pickGames, profileName, shelfGames } from '../state/library.svelte.ts';
  import { startGame, startOver } from '../state/play.svelte.ts';
  import { prefs } from '../state/prefs.svelte.ts';
  import { online } from '../state/online.svelte.ts';
  import { openShelf, view } from '../state/view.svelte.ts';

  let { tab }: { tab: (action: Action) => boolean } = $props();

  /** The rail's games: the last played or added, as many as fit a rail on the TV. */
  const recent = $derived(library.games.slice(0, 12));
  const consoles = $derived(consolesWithGames());
  /** The game in focus, by id: the library is read anew after every change. */
  let focusedId = $state<string | null>(null);
  const focused = $derived(library.games.find((g) => g.id === focusedId) ?? null);
  let root: HTMLElement | undefined = $state();

  $effect(() => {
    if (root && !root.contains(document.activeElement)) focusFirst(root);
  });

  const resumes = (game: Game) => game.resume && prefs.quickResume;

  function meta(game: Game): string {
    if (game.played > 0) return t.playedAgo(t.ago(daysAgo(game.played)));
    return `${SYSTEMS[game.system].short} · ${t.notPlayed}`;
  }

  function focusOn(game: Game | null, tone: string): void {
    focusedId = game?.id ?? null;
    view.hue = tone;
  }

  function handle(action: Action): boolean {
    if (tab(action)) return true;
    const game = focused;
    if (!game || !(document.activeElement as HTMLElement | null)?.dataset.game) return false;
    if (action === 'y' && resumes(game)) void startOver(game.id);
    else if (action === 'menu') view.sheet = game.id;
    else return false;
    return true;
  }

  const hints = $derived<Hint[]>(
    focused
      ? [
          { actions: ['a'], label: resumes(focused) ? t.resume : t.play },
          ...(resumes(focused) ? [{ actions: ['y'], label: t.startOver } satisfies Hint] : []),
          { actions: ['menu'], label: t.details },
          { actions: ['lb', 'rb'], label: t.sections },
        ]
      : [{ actions: ['a'], label: t.select }, { actions: ['lb', 'rb'], label: t.sections }],
  );
</script>

<div class="home" bind:this={root} use:layer={handle}>
  <div class="home-col">
    {#if library.games.length}
      <h1 class="h-xl">{t.keepPlaying}</h1>
      <p class="lead">{t.homeLead}</p>
    {:else}
      <h1 class="h-l home-welcome">{t.welcome(profileName(currentProfile()))}</h1>
      <p class="lead">{t.welcomeLead}</p>
    {/if}
    <Hints {hints} column />
    <button class="btn" data-nav onclick={() => (online.open = true)}>{t.joinRoom}</button>
  </div>

  {#if library.games.length}
    <div class="rows">
      <section class="rail" aria-label={t.keepPlaying}>
        <div class="track">
          {#each recent as game, i (game.id)}
            {@const resume = resumes(game)}
            {@const isFocused = focused?.id === game.id}
            <div class="card-wrap" class:is-focus={isFocused}>
              <button class="card" data-nav data-row="games" data-game={game.id} data-first={i === 0 ? '' : undefined}
                onfocus={() => focusOn(game, hue(game.system))} onclick={() => startGame(game.id)}>
                <span class="card-art"><Cover {game} /><span class="pill pill-sm chip">{SYSTEMS[game.system].short}</span></span>
                <span class="card-foot">
                  <span class="card-act">
                    {#if isFocused}
                      <span class="cta"><Glyph action="a" />{resume ? t.resume : t.play}</span>
                    {:else if resume}
                      <span class="pill"><Icon name="pause" />{t.quickResume}</span>
                    {/if}
                  </span>
                  <span class="card-line">{game.name}</span>
                  <span class="card-meta">{meta(game)}</span>
                </span>
              </button>
              <button class="card-more cta-2 is-round" tabindex="-1" aria-label={t.details} onclick={() => (view.sheet = game.id)}><Glyph action="menu" /></button>
            </div>
          {/each}
          <button class="card card-add" data-nav data-row="games" onfocus={() => focusOn(null, HUE_NONE)} onclick={pickGames}>
            <span class="card-art"><span class="ic ic-lg"><Icon name="upload" /></span></span>
            <span class="card-foot"><span class="card-act"></span><span class="card-line">{t.addGames}</span><span class="card-meta">{t.addGamesLine}</span></span>
          </button>
        </div>
      </section>

      <section class="rail rail-cons" aria-label={t.yourLibrary}>
        <h2 class="rail-head">{t.yourLibrary} <span class="rail-n">{library.games.length}</span></h2>
        <div class="track">
          {#each consoles as id (id)}
            <button class="con" data-nav data-row="consoles" style:--hue={hue(id)} onfocus={() => focusOn(null, hue(id))} onclick={() => openShelf(id)}>
              <span class="con-art"><img class="con-img" src={art(id)} alt="" /><span class="con-epic">{epic(id)}</span></span>
              <span class="con-foot"><span class="con-name">{SYSTEMS[id].full}</span><span class="con-n">{t.games(shelfGames(id).length)}</span></span>
            </button>
          {/each}
          <button class="con" data-nav data-row="consoles" style:--hue={HUE_ALL} onfocus={() => focusOn(null, HUE_ALL)} onclick={() => openShelf('all')}>
            <span class="con-art"><img class="con-img" src={art('all')} alt="" /><span class="con-epic">{t.allGamesEpic}</span></span>
            <span class="con-foot"><span class="con-name">{t.allGames}</span><span class="con-n">{t.games(library.games.length)}</span></span>
          </button>
        </div>
      </section>
    </div>
  {:else}
    <div class="welcome">
      <button class="press-a" data-nav data-first onfocus={() => (view.hue = HUE_NONE)} onclick={pickGames}>
        <span class="g g-xl g-ico"><Icon name="upload" /></span><span>{t.chooseFiles}</span>
      </button>
      <p class="foot">{t.orDrop}</p>
      <ConsoleList />
      <p class="tip"><Icon name="info" /><span>{t.privacy}</span></p>
    </div>
  {/if}
</div>
