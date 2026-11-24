<script lang="ts">
  // Sol's mark, Piksel: an 8×8 sun of pixels in the accent, and the lowercase wordmark beside it.
  const SUN = ['..####..', '.######.', '########', '########', '########', '########', '.######.', '..####..'];
  const pixels = SUN.flatMap((row, r) => [...row].flatMap((c, k) => (c === '#' ? [{ x: 2 + k * 5.5, y: 2 + r * 5.5 }] : [])));
</script>

<span class="brand">
  <svg viewBox="0 0 48 48" aria-hidden="true">
    {#each pixels as { x, y } (`${x},${y}`)}<rect {x} {y} width="5" height="5" rx=".8" fill="var(--accent)" />{/each}
  </svg>
  <span>sol</span>
</span>
