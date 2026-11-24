<script lang="ts">
  // The consoles the page plays, each with its picture and its games' extensions: where games come from,
  // on the first run and on an empty Start.
  import { SYSTEM_IDS, SYSTEMS } from '../../engine/systems.ts';
  import { art, hue } from '../consoles.ts';
</script>

<ul class="found">
  {#each SYSTEM_IDS as id (id)}
    <li style:--hue={hue(id)}>
      <img src={art(id)} alt="" />
      <b>{SYSTEMS[id].name}</b>
      <span>{SYSTEMS[id].extensions.map((ext) => `.${ext}`).join(', ')}</span>
    </li>
  {/each}
</ul>
