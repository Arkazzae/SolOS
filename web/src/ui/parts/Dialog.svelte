<script lang="ts">
  // A window over the screen, as the console's: a question with the safe answer in focus, a name to type,
  // or tables of keys. B (Esc) is always the safe way out.
  import { NAME_MAX } from '../../engine/store.ts';
  import { t } from '../../lib/text.svelte.ts';
  import { layer } from '../layers.ts';
  import type { Action } from '../nav.ts';
  import type { Dialog } from '../state/notify.svelte.ts';
  import Hints from './Hints.svelte';
  import Icon from './Icon.svelte';

  let { dialog }: { dialog: Dialog } = $props();
  // svelte-ignore state_referenced_locally
  let name = $state(dialog.kind === 'name' ? dialog.value : '');
  let field: HTMLInputElement | undefined = $state();
  let safe: HTMLButtonElement | undefined = $state();
  let groups: HTMLElement | undefined = $state();

  $effect(() => {
    if (field) field.focus();
    else safe?.focus();
  });

  function close(): void {
    if (dialog.kind === 'ask') dialog.done(false);
    else if (dialog.kind === 'name') dialog.done(null);
    else dialog.done();
  }

  function handle(action: Action): boolean {
    if (action === 'b') {
      close();
      return true;
    }
    if (action === 'a' && dialog.kind === 'name' && document.activeElement === field) {
      dialog.done(name);
      return true;
    }
    // The keys' tables are longer than the window: up and down scroll them.
    if (groups && (action === 'up' || action === 'down')) {
      groups.scrollBy({ top: (action === 'up' ? -1 : 1) * groups.clientHeight * 0.6, behavior: 'smooth' });
      return true;
    }
    return false;
  }
</script>

<div class="veil" role="presentation" onclick={close}></div>
<div class="modal" role="dialog" aria-modal="true" aria-labelledby="dialog-title" use:layer={handle}>
  <span class="ic ic-lg" class:tone-danger={dialog.kind === 'ask' && dialog.danger}>
    <Icon name={dialog.kind === 'ask' ? (dialog.danger ? 'trash' : 'restart') : dialog.kind === 'name' ? 'user' : 'padLine'} />
  </span>
  <div class="modal-body">
    <h2 class="h-m" id="dialog-title">{dialog.title}</h2>
    {#if dialog.kind === 'ask'}
      {@const { done, confirm, danger } = dialog}
      <p class="lead">{dialog.line}</p>
      <div class="modal-acts">
        <button class="btn" data-nav bind:this={safe} onclick={() => done(false)}>{t.cancel}</button>
        <button class="btn" class:btn-danger={danger} data-nav onclick={() => done(true)}>{confirm}</button>
      </div>
    {:else if dialog.kind === 'name'}
      {@const { done, confirm } = dialog}
      <input class="field" data-nav bind:this={field} bind:value={name} maxlength={NAME_MAX} placeholder={t.namePlaceholder} autocomplete="off" spellcheck="false" />
      <div class="modal-acts">
        <button class="btn" data-nav onclick={close}>{t.cancel}</button>
        <button class="btn btn-ink" data-nav onclick={() => done(name)}>{confirm}</button>
      </div>
    {:else}
      <div class="key-groups" bind:this={groups}>
        {#each dialog.groups as group, i (i)}
          <section class="key-group">
            {#if group.title}<p class="eyebrow">{group.title}</p>{/if}
            <div class="keys keys-wide">
              {#each group.rows as [key, what] (key + what)}<div><kbd>{key}</kbd><span>{what}</span></div>{/each}
            </div>
          </section>
        {/each}
      </div>
      <div class="modal-acts"><button class="btn btn-ink" data-nav bind:this={safe} onclick={close}>{t.back}</button></div>
    {/if}
    <Hints hints={[{ actions: ['a'], label: t.select }, { actions: ['b'], label: t.cancel }]} />
  </div>
</div>
