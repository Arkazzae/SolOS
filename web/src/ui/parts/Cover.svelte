<script lang="ts" module>
  /** The pictures' addresses, by game and kind, kept while the picture is the same one: every card of a
   * game shows the same address, and an address goes when its game's picture changes. */
  const urls = new Map<string, { blob: Blob; url: string }>();

  function urlOf(key: string, blob: Blob): string {
    const known = urls.get(key);
    if (known?.blob === blob) return known.url;
    if (known) URL.revokeObjectURL(known.url);
    const url = URL.createObjectURL(blob);
    urls.set(key, { blob, url });
    return url;
  }
</script>

<script lang="ts">
  // A game's picture: the last frame its Quick Resume kept, else its own (a PICO-8 cartridge's label), else
  // a cover drawn in its console's hue with its name, as the console draws one for a game without box art.
  import type { Game } from '../../engine/store.ts';
  import { SYSTEMS } from '../../engine/systems.ts';
  import { hue } from '../consoles.ts';

  let { game, large = false }: { game: Game; large?: boolean } = $props();

  const url = $derived(
    game.shot ? urlOf(`${game.id}/shot`, game.shot) : game.cover ? urlOf(`${game.id}/cover`, game.cover) : null,
  );
</script>

{#if url}
  <span class="cover is-real" class:is-label={!game.shot}><img src={url} alt="" /></span>
{:else}
  <span class="cover" class:cover-lg={large} style:--hue={hue(game.system)}>
    <span class="cover-sys">{SYSTEMS[game.system].short}</span>
    <span class="cover-title">{game.name}</span>
  </span>
{/if}
