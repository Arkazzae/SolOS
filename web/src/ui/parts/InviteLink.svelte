<script lang="ts">
  import { t } from '../../lib/text.svelte.ts';
  import { inviteLink } from '../state/online.svelte.ts';
  import { toast } from '../state/notify.svelte.ts';

  let { code }: { code: string } = $props();
  let fallback = $state('');
  async function copy() {
    const link = inviteLink(code);
    try { await navigator.clipboard.writeText(link); toast(t.inviteCopied, 'check'); }
    catch { fallback = link; }
  }
</script>

<button class="btn" data-nav onclick={copy}>{t.copyInvite}</button>
{#if fallback}<input class="field" aria-label={t.copyInvite} readonly value={fallback} data-nav />{/if}
