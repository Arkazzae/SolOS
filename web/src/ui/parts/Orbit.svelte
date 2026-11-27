<script lang="ts">
  let { moving = false }: { moving?: boolean } = $props();
</script>

<span class="orbit-scene" class:moving aria-hidden="true">
  <span class="system">
    <span class="plane">
      <span class="ring outer"></span>
      <span class="ring middle"></span>
      <span class="ring inner"></span>
    </span>
    <span class="planet"></span>
  </span>
</span>

<style>
  .orbit-scene {
    position: relative;
    display: block;
    width: 100%;
    height: 100%;
    container-type: size;
    overflow: hidden;
    border-radius: inherit;
    background: radial-gradient(ellipse at 76% 58%, #222039, #101320 39%, #090b10 77%);
  }
  .system {
    position: absolute;
    left: 73%;
    top: 57%;
    width: max(84cqw, 102cqh);
    aspect-ratio: 1;
    transform: translate(-50%, -50%);
  }
  .plane {
    position: absolute;
    inset: 0;
    transform: rotate(-23deg) scaleY(.43);
  }
  .ring {
    position: absolute;
    border-radius: 50%;
    border: max(.6px, .08cqw) solid color-mix(in srgb, var(--light) 18%, transparent);
  }
  .outer  { inset: 0;   --light: #729bcc; --period: 46s; --phase: 242deg; }
  .middle { inset: 9%;  --light: var(--accent); --period: 32s; --phase: 104deg; }
  .inner  { inset: 19%; --light: #e0c6a8; --period: 21s; --phase: 322deg; }
  .ring::before {
    content: '';
    position: absolute;
    inset: -1px;
    border-radius: 50%;
    background: conic-gradient(transparent 0 276deg, var(--light) 352deg, #fff1da 359deg, transparent 360deg);
    mask-image: radial-gradient(closest-side, transparent calc(100% - max(1.5px, .24cqw)), black 99.9%);
    transform: rotate(var(--phase));
  }
  .planet {
    position: absolute;
    left: 50%;
    top: 50%;
    width: 25%;
    aspect-ratio: 1;
    transform: translate(-50%, -50%);
    border-radius: 50%;
    background: radial-gradient(circle at 40% 59%, #090b12 46%, #151827 59%, #443446 71%, var(--accent) 78%, #ffe3c4 81%);
    box-shadow: .4cqw -.35cqw 2.5cqw -.7cqw color-mix(in srgb, var(--accent) 38%, transparent);
  }
  @media (prefers-reduced-motion: no-preference) {
    :global(.menu-background) .ring::before {
      animation: revolve var(--period) linear infinite paused;
    }
    :global(.sect:not([inert])) .orbit-scene.moving .ring::before {
      animation-play-state: running;
    }
  }
  @keyframes revolve {
    from { transform: rotate(var(--phase)); }
    to { transform: rotate(calc(var(--phase) + 1turn)); }
  }
</style>
