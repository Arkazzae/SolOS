<script lang="ts">
  // The first run, as the console's setup: a progress line on top, one question a screen. The language
  // (English first), the profile's name, then the first games.
  import { NAME_MAX } from '../../engine/store.ts';
  import { LANG_NAMES, LANGS, type Lang, t } from '../../lib/text.svelte.ts';
  import { HUE_NONE } from '../consoles.ts';
  import { layer } from '../layers.ts';
  import { type Action, focusFirst } from '../nav.ts';
  import Brand from '../parts/Brand.svelte';
  import ConsoleList from '../parts/ConsoleList.svelte';
  import Glyph from '../parts/Glyph.svelte';
  import Hints from '../parts/Hints.svelte';
  import Icon from '../parts/Icon.svelte';
  import { library, makeFirstProfile, pickGames } from '../state/library.svelte.ts';
  import { prefs, setPrefs } from '../state/prefs.svelte.ts';
  import { type SetupStep, view } from '../state/view.svelte.ts';

  const STEPS: SetupStep[] = ['lang', 'profile', 'games'];
  const step = $derived(view.setup ?? 'lang');
  const at = $derived(STEPS.indexOf(step));
  const stepName = (s: SetupStep) => (s === 'lang' ? t.stepLanguage : s === 'profile' ? t.stepProfile : t.stepGames);
  let name = $state('');
  let root: HTMLElement | undefined = $state();
  let field: HTMLInputElement | undefined = $state();

  // Each step starts with its first choice, or its field, in focus.
  $effect(() => {
    void step;
    view.hue = HUE_NONE;
    if (field) field.focus();
    else if (root) focusFirst(root);
  });

  function done(): void {
    view.setup = null;
    view.section = 'home';
  }

  // Games added on the last step (picked, or dropped on the page) end the first run.
  $effect(() => {
    if (step === 'games' && library.games.length > 0) done();
  });

  function chooseLang(lang: Lang): void {
    setPrefs({ lang });
    view.setup = 'profile';
  }

  async function makeProfile(): Promise<void> {
    await makeFirstProfile(name);
    view.setup = 'games';
  }

  function handle(action: Action): boolean {
    if (action === 'b') {
      if (step === 'profile') view.setup = 'lang';
      return true;
    }
    if (action === 'a' && step === 'profile' && document.activeElement === field) {
      void makeProfile();
      return true;
    }
    return false;
  }
</script>

<div class="setup" bind:this={root} use:layer={handle}>
  <header class="setup-top">
    <Brand />
    <ol class="progress">
      {#each STEPS as s, i (s)}
        <li class:is-on={i === at} class:is-done={i < at}>
          <span class="step-n">{#if i < at}<Icon name="check" />{:else}{i + 1}{/if}</span>
          <span>{stepName(s)}</span>
        </li>
      {/each}
    </ol>
  </header>

  <div class="setup-body">
    {#if step === 'lang'}
      <h1 class="h-l">{t.chooseLanguage}</h1>
      <p class="lead">{t.chooseLanguageLead}</p>
      <div class="group opts">
        {#each LANGS as lang (lang)}
          <button class="row" data-nav data-first={lang === prefs.lang ? '' : undefined} {lang} onclick={() => chooseLang(lang)}>
            <span class="row-ic"><Icon name="globe" /></span><span class="row-k">{LANG_NAMES[lang]}</span>
            {#if lang === prefs.lang}<span class="set-v"><Icon name="check" /></span>{/if}
          </button>
        {/each}
      </div>
    {:else if step === 'profile'}
      <h1 class="h-l">{t.whoPlays}</h1>
      <p class="lead">{t.whoPlaysLead}</p>
      <label class="field-row">
        <span class="eyebrow">{t.yourName}</span>
        <input class="field" data-nav bind:this={field} bind:value={name} maxlength={NAME_MAX} placeholder={t.namePlaceholder} autocomplete="nickname" spellcheck="false" />
      </label>
      <button class="press-a" data-nav onclick={makeProfile}><Glyph action="a" /><span>{t.next}</span></button>
    {:else}
      <h1 class="h-l">{t.firstGames}</h1>
      <p class="lead">{t.firstGamesLead}</p>
      <div class="cta-row setup-acts">
        <button class="press-a" data-nav data-first onclick={pickGames}><span class="g g-xl g-ico"><Icon name="upload" /></span><span>{t.chooseFiles}</span></button>
        <button class="cta-2 setup-later" data-nav onclick={done}>{t.later}</button>
      </div>
      <ConsoleList />
      <p class="tip"><Icon name="info" /><span>{t.privacy}</span></p>
    {/if}
  </div>

  <Hints hints={step === 'profile'
    ? [{ actions: ['a'], label: t.next }, { actions: ['b'], label: t.back }]
    : [{ actions: ['a'], label: t.select }]} />
</div>
