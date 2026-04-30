<script lang="ts">
  import { onMount } from 'svelte';
  import { listWikiPages } from '$lib/api';

  let pages = $state<string[]>([]);
  let error = $state<string | null>(null);

  onMount(async () => {
    try {
      pages = (await listWikiPages('')).pages;
    } catch (e: any) {
      error = String(e?.message ?? e);
    }
  });

  // Group pages by top-level directory for a quick visual tree.
  const grouped = $derived(() => {
    const out: Record<string, string[]> = {};
    for (const p of pages) {
      const slash = p.indexOf('/');
      const bucket = slash === -1 ? '(root)' : p.slice(0, slash);
      (out[bucket] ??= []).push(p);
    }
    for (const k of Object.keys(out)) out[k].sort();
    return out;
  });
</script>

<h1>Wiki</h1>

{#if error}
  <div class="card" style="border-color: var(--err); color: var(--err);">{error}</div>
{:else if pages.length === 0}
  <div class="card muted">Wiki is empty — ingest a source to see the LLM populate it.</div>
{:else}
  <div class="card">
    <div class="tree">
      {#each Object.keys(grouped()).sort() as bucket}
        <div class="dir">{bucket}/</div>
        {#each grouped()[bucket] as path}
          <a href={`/wiki/${path}`}>  {path}</a>
        {/each}
      {/each}
    </div>
  </div>
{/if}
