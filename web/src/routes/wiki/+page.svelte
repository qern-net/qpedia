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

  // Pagination: cap each bucket, expandable on demand (buckets can be long
  // once the taxonomy deepens).
  const BUCKET_CAP = 50;
  let expanded = $state<Set<string>>(new Set());
  function toggleBucket(b: string) {
    const next = new Set(expanded);
    next.has(b) ? next.delete(b) : next.add(b);
    expanded = next;
  }
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
        <div class="dir">{bucket}/ <span class="muted" style="font-weight: 400;">({grouped[bucket].length})</span></div>
        {#each (expanded.has(bucket) ? grouped[bucket] : grouped[bucket].slice(0, BUCKET_CAP)) as path}
          <a href={`/wiki/${path}`} style="padding-left: 16px;">{path}</a>
        {/each}
        {#if grouped[bucket].length > BUCKET_CAP}
          <button class="more" onclick={() => toggleBucket(bucket)}>
            {expanded.has(bucket)
              ? '▴ show less'
              : `▾ show all ${grouped[bucket].length}`}
          </button>
        {/if}
      {/each}
    </div>
  </div>
{/if}

<style>
  .more {
    margin: 2px 0 8px 16px;
    padding: 2px 8px;
    font-size: 12px;
    background: none;
    border: none;
    color: var(--fg-dim);
    cursor: pointer;
  }
  .more:hover { color: var(--fg); background: var(--bg-3); border-radius: 4px; }
</style>
