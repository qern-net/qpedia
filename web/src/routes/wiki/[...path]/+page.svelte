<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { getWikiPage } from '$lib/api';
  import { marked } from 'marked';

  let raw = $state('');
  let error = $state<string | null>(null);

  $effect(() => {
    const path = $page.params.path;
    if (!path) return;
    error = null;
    raw = '';
    getWikiPage(path).then((t) => (raw = t)).catch((e) => (error = String(e?.message ?? e)));
  });

  // Split frontmatter from body, render body as markdown with [[wikilinks]]
  // rewritten to /wiki/<path> hrefs.
  const parts = $derived.by(() => {
    if (!raw) return { frontmatter: '', body: '' };
    const trimmed = raw.replace(/^﻿/, '');
    if (trimmed.startsWith('---')) {
      const end = trimmed.indexOf('\n---', 3);
      if (end !== -1) {
        return {
          frontmatter: trimmed.slice(3, end).trim(),
          body: trimmed.slice(end + 4).replace(/^\s*/, '')
        };
      }
    }
    return { frontmatter: '', body: trimmed };
  });

  const html = $derived(() => {
    if (!parts.body) return '';
    // [[concepts/foo.md]] → standard markdown link to /wiki/concepts/foo.md
    const linked = parts.body.replace(/\[\[([^\]]+)\]\]/g, (_m, target) => {
      const t = String(target).trim();
      return `[${t}](/wiki/${t})`;
    });
    return marked.parse(linked, { async: false }) as string;
  });
</script>

<div class="row" style="margin-bottom: 16px;">
  <a href="/wiki" class="muted">← all pages</a>
  <span class="spacer"></span>
  <span class="mono muted">{$page.params.path}</span>
</div>

{#if error}
  <div class="card" style="border-color: var(--err); color: var(--err);">{error}</div>
{:else if !raw}
  <div class="card muted">Loading…</div>
{:else}
  {#if parts.frontmatter}
    <details class="frontmatter">
      <summary class="muted" style="cursor: pointer;">frontmatter</summary>
      <pre style="margin: 8px 0 0;">{parts.frontmatter}</pre>
    </details>
  {/if}
  <div class="markdown">{@html html()}</div>
{/if}
