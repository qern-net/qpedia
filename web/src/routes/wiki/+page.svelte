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

  // Group pages by top-level directory. System files (root-level .md) go
  // into a pinned "system" bucket rendered separately.
  const SYSTEM_PAGES = ['index.md', 'log.md', 'QPEDIA.md'];

  const grouped = $derived.by(() => {
    const out: Record<string, string[]> = {};
    for (const p of pages) {
      if (SYSTEM_PAGES.includes(p)) continue; // handled separately
      const slash = p.indexOf('/');
      const bucket = slash === -1 ? '(root)' : p.slice(0, slash);
      (out[bucket] ??= []).push(p);
    }
    for (const k of Object.keys(out)) out[k].sort();
    return out;
  });

  const systemPages = $derived(pages.filter((p) => SYSTEM_PAGES.includes(p)));
</script>

<h1>Wiki</h1>

{#if error}
  <div class="card" style="border-color: var(--err); color: var(--err);">{error}</div>
{:else if pages.length === 0}
  <div class="card muted">Wiki is empty — ingest a source to see the LLM populate it.</div>
{:else}
  <div class="card">
    <div class="tree">
      <!-- Pinned system pages -->
      {#if systemPages.length > 0}
        <div class="dir" style="margin-bottom: 4px;">system</div>
        {#each SYSTEM_PAGES.filter((p) => systemPages.includes(p)) as path}
          <a href={`/wiki/${path}`} style="padding-left: 16px;">
            {path === 'log.md' ? '📋 ' : path === 'index.md' ? '📑 ' : '📖 '}{path}
          </a>
        {/each}
        <div style="border-top: 1px solid var(--border); margin: 8px 0;"></div>
      {/if}

      <!-- Content pages grouped by directory -->
      {#each Object.keys(grouped).sort() as bucket}
        <div class="dir">{bucket}/</div>
        {#each grouped[bucket] as path}
          <a href={`/wiki/${path}`} style="padding-left: 16px;">{path}</a>
        {/each}
      {/each}
    </div>
  </div>
{/if}
