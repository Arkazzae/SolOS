<script lang="ts">
  // Notifications, top right under the status, as the console's.
  import { notices } from '../state/notify.svelte.ts';
  import Icon from './Icon.svelte';
</script>

<div class="toasts" role="status" aria-live="polite">
  {#each notices.toasts as toast (toast.id)}
    <div class="toast" class:is-accent={toast.tone === 'accent'} class:is-danger={toast.tone === 'danger'}>
      <Icon name={toast.icon} />
      <div><b>{toast.title}</b>{#if toast.line}<span>{toast.line}</span>{/if}</div>
    </div>
  {/each}
</div>
