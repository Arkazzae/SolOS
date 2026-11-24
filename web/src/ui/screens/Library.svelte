<script lang="ts">
  // The Library, as the console's game browser: the shelves (every game, favourites, each console with
  // games) under LB and RB, the focused game's card on the left, the grid on the right with the letter
  // bar (LT and RT jump a letter), X for a star, Y to start over, Menu for the game's page.
  import { untrack } from 'svelte';
  import type { Game } from '../../engine/store.ts';
  import { SYSTEMS } from '../../engine/systems.ts';
  import { t } from '../../lib/text.svelte.ts';
  import { epic, HUE_ALL, HUE_FAV, hue } from '../consoles.ts';
  import { layer } from '../layers.ts';
  import { type Action, focus, focusFirst, type Hint } from '../nav.ts';
  import Cover from '../parts/Cover.svelte';
  import Glyph from '../parts/Glyph.svelte';
  import Hints from '../parts/Hints.svelte';
  import Icon from '../parts/Icon.svelte';
  import { daysAgo, pickGames, shelfGames, shelves, toggleFav } from '../state/library.svelte.ts';
  import { startGame, startOver } from '../state/play.svelte.ts';
  import { prefs } from '../state/prefs.svelte.ts';
  import { go, type Shelf, view } from '../state/view.svelte.ts';

  const list = $derived(shelves());
  const shelf = $derived(list.includes(view.shelf) ? view.shelf : 'all');
  const games = $derived(shelfGames(shelf));
  let focusedId = $state<string | null>(null);
  const focused = $derived(games.find((g) => g.id === focusedId) ?? null);
  const resume = $derived(!!focused?.resume && prefs.quickResume);
  let grid: HTMLElement | undefined = $state();
  let root: HTMLElement | undefined = $state();

  const letterOf = (game: Game) => {
    const c = game.name[0]?.toLocaleUpperCase() ?? '#';
    return /\p{L}/u.test(c) ? c : '#';
  };
  const letters = $derived([...new Set(games.map(letterOf))]);
  const letter = $derived(focused ? letterOf(focused) : (letters[0] ?? ''));
  const near = $derived(letters.slice(Math.max(0, letters.indexOf(letter) - 1), letters.indexOf(letter) + 2));

  const shelfName = (s: Shelf) => (s === 'all' ? t.allGames : s === 'fav' ? t.favourites : SYSTEMS[s].short);
  const shelfEpic = (s: Shelf) => (s === 'all' ? t.allGamesEpic : s === 'fav' ? t.favouritesEpic : epic(s));
  const shelfHue = (s: Shelf) => (s === 'all' ? HUE_ALL : s === 'fav' ? HUE_FAV : hue(s));

  $effect(() => {
    view.hue = focused ? hue(focused.system) : shelfHue(shelf);
  });

  // A new shelf, or the focused game gone from it, puts the focus on the shelf's first game. A game on
  // both shelves keeps its tile, and the tile its focus without a new focus event: the game in focus is
  // read back from the page.
  $effect(() => {
    void shelf;
    const tiles = grid;
    if (!tiles) return;
    untrack(() => {
      const known = focusedId && tiles.querySelector<HTMLElement>(`[data-game="${focusedId}"]`);
      if (known) {
        if (!root?.contains(document.activeElement)) focus(known);
        return;
      }
      focusFirst(tiles);
      focusedId = (document.activeElement as HTMLElement | null)?.dataset.game ?? null;
    });
  });

  function chooseShelf(next: Shelf): void {
    view.shelf = next;
    focusedId = null;
  }

  /** LB and RB along the shelves; past the first or the last they go on to the next section, so the
   * bumpers walk Start, every shelf and Settings in one line. */
  function turnShelf(by: number): void {
    const next = list[list.indexOf(shelf) + by];
    if (next === undefined) go(by < 0 ? 'home' : 'settings');
    else chooseShelf(next);
  }

  function jumpLetter(by: number): void {
    const next = letters[Math.max(0, Math.min(letters.length - 1, letters.indexOf(letter) + by))];
    const game = games.find((g) => letterOf(g) === next);
    const el = game && grid?.querySelector<HTMLElement>(`[data-game="${game.id}"]`);
    if (el) focus(el);
  }

  function handle(action: Action): boolean {
    switch (action) {
      case 'lb': turnShelf(-1); return true;
      case 'rb': turnShelf(1); return true;
      case 'lt': jumpLetter(-1); return true;
      case 'rt': jumpLetter(1); return true;
      case 'b': go('home'); return true;
      case 'x': if (focused) void toggleFav(focused.id); return true;
      case 'y': if (focused && resume) void startOver(focused.id); return true;
      case 'menu': if (focused) view.sheet = focused.id; return true;
      default: return false;
    }
  }

  function facts(game: Game): string {
    return [SYSTEMS[game.system].name, game.tags, t.size(game.size)].filter(Boolean).join(' · ');
  }

  const hints = $derived<Hint[]>(
    games.length === 0
      ? [{ actions: ['a'], label: t.select }, { actions: ['lb', 'rb'], label: t.console }, { actions: ['b'], label: t.back }]
      : [
          { actions: ['a'], label: resume ? t.resume : t.play },
          { actions: ['x'], label: t.favourite },
          { actions: ['menu'], label: t.details },
          { actions: ['lt', 'rt'], label: t.letter },
          { actions: ['lb', 'rb'], label: t.console },
          { actions: ['b'], label: t.back },
        ],
  );
</script>

<div class="lib" bind:this={root} use:layer={handle}>
  <aside class="lib-side">
    {#if focused}
      {@const game = focused}
      <p class="eyebrow">{SYSTEMS[game.system].name} · {epic(game.system)}</p>
      <h1 class="h-l">{game.name}</h1>
      <p class="hero-meta">{facts(game)}</p>
      <p class="lib-played">{game.played ? t.playedAgo(t.ago(daysAgo(game.played))) : t.notPlayed}</p>
      <div class="cta-row">
        <button class="cta" tabindex="-1" onclick={() => startGame(game.id)}><Glyph action="a" />{resume ? t.resume : t.play}</button>
        <button class="cta-2" tabindex="-1" onclick={() => (view.sheet = game.id)}><Glyph action="menu" />{t.details}</button>
      </div>
    {:else}
      <p class="eyebrow">{shelfName(shelf)}</p>
      <h1 class="h-l">{shelfEpic(shelf)}</h1>
    {/if}
  </aside>

  <div class="lib-main">
    <div class="lib-bar">
      <nav class="sysnav" aria-label={t.library}>
        {#if view.device !== 'touch'}<button class="tab-g" tabindex="-1" onclick={() => turnShelf(-1)}><Glyph action="lb" /></button>{/if}
        {#each list as s (s)}
          <button class="sys-tab" class:is-on={s === shelf} onclick={() => chooseShelf(s)}>
            {#if s === 'fav'}<Icon name="star" />{/if}{shelfName(s)}
          </button>
        {/each}
        {#if view.device !== 'touch'}<button class="tab-g" tabindex="-1" onclick={() => turnShelf(1)}><Glyph action="rb" /></button>{/if}
      </nav>
      {#if letters.length > 1}
        <div class="letters" aria-hidden="true">
          <button class="tab-g" tabindex="-1" onclick={() => jumpLetter(-1)}><Glyph action="lt" /></button>
          {#each near as l (l)}{#if l === letter}<b>{l}</b>{:else}<span>{l}</span>{/if}{/each}
          <button class="tab-g" tabindex="-1" onclick={() => jumpLetter(1)}><Glyph action="rt" /></button>
        </div>
      {/if}
    </div>
    <p class="crumb">{shelfName(shelf)} · {t.games(games.length)}</p>

    {#if games.length}
      <div class="gb-grid" bind:this={grid}>
        {#each games as game (game.id)}
          <button class="gt" data-nav data-game={game.id} aria-label={game.name}
            onfocus={() => (focusedId = game.id)} onclick={() => startGame(game.id)}>
            <Cover {game} />
            {#if game.fav}<span class="gt-star"><Icon name="star" /></span>{/if}
            {#if game.resume && prefs.quickResume}<span class="gt-qr"><Icon name="pause" /></span>{/if}
            <span class="gt-name">{game.name}</span>
          </button>
        {/each}
      </div>
    {:else if shelf === 'fav'}
      <p class="tip" bind:this={grid}><Icon name="starLine" /><span>{t.noFavourites}</span></p>
    {:else}
      <div class="lib-empty" bind:this={grid}>
        <p class="lead">{t.noGames}</p>
        <button class="press-a" data-nav data-first onclick={pickGames}><span class="g g-xl g-ico"><Icon name="upload" /></span><span>{t.chooseFiles}</span></button>
        <p class="foot">{t.orDrop}</p>
      </div>
    {/if}
  </div>

  <div class="lib-hints"><Hints {hints} /></div>
</div>
