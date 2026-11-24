<script lang="ts">
  // The hints along the bottom, as the console's: what each button does here. A click does it too; touch
  // has no buttons to hint at.
  import { dispatch } from '../layers.ts';
  import type { Hint } from '../nav.ts';
  import { view } from '../state/view.svelte.ts';
  import Glyph from './Glyph.svelte';

  let { hints, column = false }: { hints: Hint[]; column?: boolean } = $props();
</script>

{#if view.device !== 'touch'}
  <div class="hints" class:hints-col={column}>
    {#each hints as hint (hint.label)}
      <button class="hint" tabindex="-1" onclick={() => dispatch(hint.actions.at(-1)!)}>
        {#each hint.actions as action (action)}<Glyph {action} />{/each}
        <span>{hint.label}</span>
      </button>
    {/each}
  </div>
{/if}
