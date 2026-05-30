<script lang="ts">
  import { searchWiki, type SearchHit } from '$lib/api';

  let q = $state('');
  let mode = $state<'hybrid' | 'filesystem' | null>(null);
  let hits = $state<SearchHit[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function go() {
    if (!q.trim()) return;
    busy = true; error = null;
    try {
      const r = await searchWiki(q, 20);
      hits = r.hits;
      mode = r.mode;
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = false;
    }
  }
</script>

<h1>Search</h1>

<form onsubmit={(e) => { e.preventDefault(); go(); }} class="row" style="margin-bottom: 16px;">
  <input
    type="search"
    bind:value={q}
    placeholder="search the wiki"
    style="flex: 1; max-width: 600px;"
  />
  <button class="primary" type="submit" disabled={busy || !q.trim()}>Search</button>
  {#if mode}
    <span class="muted">mode: <strong>{mode}</strong></span>
  {/if}
</form>

{#if error}
  <div class="card" style="border-color: var(--err); color: var(--err);">{error}</div>
{:else if busy}
  <div class="muted">Searching…</div>
{:else if hits.length === 0 && mode}
  <div class="muted">No hits.</div>
{:else}
  {#each hits as h}
    <div class="search-hit">
      <div class="title" dir="auto"><a href={`/wiki/${h.path}`}>{h.title || h.path}</a></div>
      <div class="path">{h.path}</div>
      <div class="snippet" dir="auto">{h.snippet}</div>
    </div>
  {/each}
{/if}
