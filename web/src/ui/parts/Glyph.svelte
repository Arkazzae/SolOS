<script lang="ts">
  // A button's glyph in a hint: the pad's (A, LB, ≡) after a pad was used, else the keyboard's key.
  import { type Action, type Dir, KEY_CAPS, PAD_CAPS } from '../nav.ts';
  import { view } from '../state/view.svelte.ts';
  import Icon from './Icon.svelte';

  let { action }: { action: Exclude<Action, Dir> } = $props();
  const pad = $derived(view.device === 'pad');
  const cap = $derived(pad ? PAD_CAPS[action] : KEY_CAPS[action]);
</script>

{#if pad && action === 'menu'}
  <span class="g g-ico"><Icon name="menu" /></span>
{:else}
  <span class="g" class:g-pill={cap.length > 1} class:g-key={!pad}>{cap}</span>
{/if}
