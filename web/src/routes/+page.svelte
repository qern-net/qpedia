<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { deleteSource, listSources, TERMINAL, type Source } from '$lib/api';
  import StatusChip from '$lib/components/StatusChip.svelte';
  import UploadPanel from '$lib/components/UploadPanel.svelte';

  let sources = $state<Source[]>([]);
  let folder = $state('/');
  let loadError = $state<string | null>(null);
  let timer: ReturnType<typeof setInterval> | null = null;

  async function refresh() {
    try {
      sources = await listSources(folder, 200);
      loadError = null;
    } catch (e: any) {
      loadError = String(e?.message ?? e);
    }
  }

  onMount(() => {
    refresh();
    // Poll while any source is non-terminal so the chip updates live.
    timer = setInterval(() => {
      if (sources.some((s) => !TERMINAL.has(s.status))) refresh();
    }, 2000);
  });

  onDestroy(() => { if (timer) clearInterval(timer); });

  function fmtSize(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  let pendingDelete = $state<Set<string>>(new Set());

  async function onDelete(s: Source) {
    if (!confirm(`Delete "${s.filename}" and any wiki pages derived from it?`)) return;
    const next = new Set(pendingDelete);
    next.add(s.id);
    pendingDelete = next;
    try {
      await deleteSource(s.id);
      // Trigger faster polling so the row disappears once the worker finishes.
      setTimeout(refresh, 500);
      setTimeout(refresh, 2000);
      setTimeout(refresh, 5000);
    } catch (e: any) {
      const cleared = new Set(pendingDelete);
      cleared.delete(s.id);
      pendingDelete = cleared;
      alert(`delete failed: ${e?.message ?? e}`);
    }
  }
</script>

<h1>Sources</h1>

<div class="col" style="gap: 24px;">
  <UploadPanel bind:folderPath={folder} onUploaded={refresh} />

  <div>
    <div class="row" style="margin-bottom: 12px;">
      <h2 style="margin: 0;">Folder: <span class="mono">{folder}</span></h2>
      <span class="spacer"></span>
      <button onclick={refresh}>Refresh</button>
    </div>

    {#if loadError}
      <div class="card" style="border-color: var(--err); color: var(--err);">{loadError}</div>
    {:else if sources.length === 0}
      <div class="card muted">No sources in this folder yet — upload one above.</div>
    {:else}
      <div class="card" style="padding: 0; overflow: hidden;">
        <table>
          <thead>
            <tr>
              <th>Filename</th>
              <th>Type</th>
              <th>Size</th>
              <th>Status</th>
              <th>Doc type</th>
              <th>Lang</th>
              <th>Hints</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each sources as src (src.id)}
              <tr>
                <td>
                  <div>{src.filename}</div>
                  <div class="mono muted" style="font-size: 11px;">{src.id}</div>
                </td>
                <td class="mono muted">{src.mime}</td>
                <td>{fmtSize(src.size_bytes)}</td>
                <td><StatusChip status={src.status} /></td>
                <td>{src.classification?.doc_type ?? '—'}</td>
                <td>{src.language ?? '—'}</td>
                <td class="muted" style="max-width: 280px;">
                  {(src.classification?.hints ?? []).join(', ') || '—'}
                </td>
                <td>
                  <button
                    onclick={() => onDelete(src)}
                    disabled={pendingDelete.has(src.id)}
                    title="Delete this source and any wiki pages derived from it"
                    style="font-size: 12px; padding: 4px 10px;"
                  >
                    {pendingDelete.has(src.id) ? 'removing…' : 'delete'}
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </div>
</div>
