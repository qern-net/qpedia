<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { getSource, getWikiPage, sourceOriginalUrl } from '$lib/api';
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

  // Split frontmatter from body.
  const parts = $derived.by(() => {
    if (!raw) return { frontmatter: '', body: '' };
    const trimmed = raw.replace(/^/, '');
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

  // Extract source_ids from frontmatter for download links.
  const sourceIds = $derived.by(() => {
    const fm = parts.frontmatter;
    if (!fm) return [] as string[];
    const match = fm.match(/source_ids:\s*\[([^\]]*)\]/);
    if (!match) return [] as string[];
    return match[1]
      .split(',')
      .map((s) => s.trim().replace(/^["']|["']$/g, ''))
      .filter(Boolean);
  });

  // Inline provenance citations: the wiki agent emits `[^src:<id>]` markers
  // tying a fact to the source it came from. Number them in first-appearance
  // order so the body shows compact superscripts and the page foots a list.
  const CITE_RE = /\[\^src:([^\]]+)\]/g;
  const citations = $derived.by(() => {
    const order: string[] = [];
    const num = new Map<string, number>();
    for (const m of parts.body.matchAll(CITE_RE)) {
      const id = m[1].trim();
      if (!num.has(id)) {
        num.set(id, order.length + 1);
        order.push(id);
      }
    }
    return { order, num };
  });

  // Resolve cited source ids → readable filenames for the footer list.
  // Best-effort: a missing/removed source falls back to its slug.
  let citeNames = $state<Record<string, string>>({});
  const requested = new Set<string>();
  $effect(() => {
    for (const id of citations.order) {
      if (requested.has(id)) continue;
      requested.add(id);
      getSource(id)
        .then((s) => (citeNames = { ...citeNames, [id]: s.filename }))
        .catch(() => {});
    }
  });

  const html = $derived(() => {
    if (!parts.body) return '';
    // [[concepts/foo.md]] → standard markdown link to /wiki/concepts/foo.md
    let s = parts.body.replace(/\[\[([^\]]+)\]\]/g, (_m, target) => {
      const t = String(target).trim();
      return `[${t}](/wiki/${t})`;
    });
    // [^src:<id>] → superscript number linking to the footer entry.
    s = s.replace(CITE_RE, (_m, id) => {
      const sid = String(id).trim();
      const n = citations.num.get(sid);
      if (!n) return '';
      return `<sup class="cite-ref"><a href="#cite-${n}" title="Source ${n}">${n}</a></sup>`;
    });
    return marked.parse(s, { async: false }) as string;
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

  {#if sourceIds.length > 0}
    <div class="row" style="margin-bottom: 12px; flex-wrap: wrap; gap: 6px;">
      <span class="muted" style="font-size: 12px;">Sources:</span>
      {#each sourceIds as sid}
        <a
          href={sourceOriginalUrl(sid)}
          download
          class="cite"
          title="Download original file for source {sid}"
          style="font-size: 12px;"
        >↓ {sid.slice(0, 10)}…</a>
      {/each}
    </div>
  {/if}

  <div class="markdown">{@html html()}</div>

  {#if citations.order.length > 0}
    <section class="citations">
      <h2>Sources cited</h2>
      <ol>
        {#each citations.order as sid, i (sid)}
          <li id="cite-{i + 1}">
            <a href={sourceOriginalUrl(sid)} download title="Download original file">
              {citeNames[sid] ?? sid}
            </a>
          </li>
        {/each}
      </ol>
    </section>
  {/if}
{/if}

<style>
  .cite {
    background: var(--bg-2);
    border: 1px solid var(--border);
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 12px;
    text-decoration: none;
    color: var(--fg);
  }
  .cite:hover {
    background: var(--bg-3);
  }

  /* Inline citation superscripts live inside {@html}, so target them
     with :global (Svelte can't see into rendered HTML to scope them). */
  .markdown :global(sup.cite-ref) { font-size: 0.7em; line-height: 0; }
  .markdown :global(sup.cite-ref a) {
    color: var(--accent);
    text-decoration: none;
    padding: 0 1px;
  }
  .markdown :global(sup.cite-ref a:hover) { text-decoration: underline; }

  .citations {
    margin-top: 32px;
    border-top: 1px solid var(--border);
    padding-top: 12px;
  }
  .citations h2 {
    font-size: 13px; margin: 0 0 8px;
    color: var(--fg-dim); text-transform: uppercase; letter-spacing: 0.06em;
  }
  .citations ol { padding-left: 22px; margin: 0; }
  .citations li { margin: 3px 0; font-size: 13px; }
  /* Highlight the entry briefly when jumped to via a superscript. */
  .citations li:target { background: var(--bg-2); border-radius: 4px; }
</style>
