<script lang="ts">
  // An icon in the text's colour (lib/icons.ts).
  import { type IconName, PH } from '../../lib/icons.ts';

  let { name }: { name: IconName } = $props();
</script>

<svg viewBox="0 0 256 256" aria-hidden="true"><path fill="currentColor" d={PH[name]} /></svg>
