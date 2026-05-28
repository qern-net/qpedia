<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    createFolder,
    deleteFolder,
    deleteSource,
    listFolders,
    listSources,
    moveSource,
    setFolderPinned,
    sourceOriginalUrl,
    TERMINAL,
    type Folder,
    type Source
  } from '$lib/api';
  import StatusChip from '$lib/components/StatusChip.svelte';
  import UploadPanel from '$lib/components/UploadPanel.svelte';
  import FolderTree from '$lib/components/FolderTree.svelte';

  let sources = $state<Source[]>([]);
  let folders = $state<Folder[]>([]);
  let selected = $state('/');
  let loadError = $state<string | null>(null);
  let timer: ReturnType<typeof setInterval> | null = null;

  // Files shown in the right pane: those directly in the selected folder.
  const visible = $derived(sources.filter((s) => (s.folder_path || '/') === selected));

  async function refresh() {
    try {
      // folder='/' returns every source (prefix match), so we can build the
      // whole tree client-side and filter the pane by selection.
      const [srcs, fld] = await Promise.all([listSources('/', 1000), listFolders()]);
      sources = srcs;
      folders = fld.items;
      loadError = null;
    } catch (e: any) {
      loadError = String(e?.message ?? e);
    }
  }

  onMount(() => {
    refresh();
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
  let actionError = $state<string | null>(null);

  async function onDelete(s: Source) {
    if (!confirm(`Delete "${s.filename}" and any wiki pages derived from it?`)) return;
    const next = new Set(pendingDelete);
    next.add(s.id);
    pendingDelete = next;
    try {
      await deleteSource(s.id);
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

  // ── tree callbacks ──
  async function onCreateFolder(path: string) {
    actionError = null;
    try {
      const f = await createFolder(path); // pinned by default (manual action)
      await refresh();
      selected = f.path;
    } catch (e: any) { actionError = String(e?.message ?? e); }
  }

  async function onDeleteFolder(path: string) {
    if (!confirm(`Delete folder ${path}? (Only empty folders can be deleted.)`)) return;
    actionError = null;
    try {
      await deleteFolder(path);
      if (selected === path) selected = '/';
      await refresh();
    } catch (e: any) { actionError = String(e?.message ?? e); }
  }

  async function onTogglePin(path: string, pinned: boolean) {
    actionError = null;
    try {
      await setFolderPinned(path, pinned);
      await refresh();
    } catch (e: any) { actionError = String(e?.message ?? e); }
  }

  async function onMoveSource(id: string, folderPath: string) {
    actionError = null;
    try {
      await moveSource(id, folderPath);
      await refresh();
    } catch (e: any) { actionError = String(e?.message ?? e); }
  }

  function onDragStart(e: DragEvent, s: Source) {
    if (!e.dataTransfer) return;
    e.dataTransfer.setData('text/source-id', s.id);
    e.dataTransfer.setData('text/plain', s.id); // fallback for strict browsers
    e.dataTransfer.effectAllowed = 'move';
  }
</script>

<h1>Sources</h1>

<div class="col" style="gap: 20px;">
  <UploadPanel bind:folderPath={selected} onUploaded={refresh} />

  {#if loadError}
    <div class="card" style="border-color: var(--err); color: var(--err);">{loadError}</div>
  {/if}
  {#if actionError}
    <div class="card" style="border-color: var(--err); color: var(--err);">{actionError}</div>
  {/if}

  <div class="explorer">
    <!-- ── Left: folder tree ── -->
    <aside class="pane-left">
      <div class="row" style="margin-bottom: 8px;">
        <h2 style="margin: 0; font-size: 15px;">Folders</h2>
        <span class="spacer"></span>
        <button onclick={refresh} title="Refresh" style="font-size: 12px; padding: 3px 8px;">⟳</button>
      </div>
      <FolderTree
        {folders}
        {sources}
        bind:selected
        manage
        droppable
        onSelect={(p) => (selected = p)}
        {onCreateFolder}
        {onDeleteFolder}
        {onTogglePin}
        {onMoveSource}
      />
      <p class="muted" style="font-size: 11px; margin-top: 8px;">
        Drag a file onto a folder to move it. 🔒 folders are kept out of AI auto-organization.
      </p>
    </aside>

    <!-- ── Right: files in the selected folder ── -->
    <section class="pane-right">
      <div class="row" style="margin-bottom: 12px;">
        <h2 style="margin: 0; font-size: 15px;">Files in <span class="mono">{selected}</span></h2>
      </div>

      {#if visible.length === 0}
        <div class="card muted">No files in this folder. Upload above, or drag files here from another folder.</div>
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
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each visible as src (src.id)}
                <tr draggable="true" ondragstart={(e) => onDragStart(e, src)} title="Drag to a folder to move">
                  <td>
                    <div>{src.filename}</div>
                    <div class="mono muted" style="font-size: 11px;">{src.id}</div>
                  </td>
                  <td class="mono muted">{src.mime}</td>
                  <td>{fmtSize(src.size_bytes)}</td>
                  <td><StatusChip status={src.status} /></td>
                  <td>{src.classification?.doc_type ?? '—'}</td>
                  <td>{src.language ?? '—'}</td>
                  <td style="white-space: nowrap;">
                    <a
                      href={sourceOriginalUrl(src.id)}
                      download={src.filename}
                      draggable="false"
                      title="Download original file"
                      style="font-size: 12px; padding: 4px 10px; background: var(--bg-2); border: 1px solid var(--border); border-radius: 6px; color: var(--fg); text-decoration: none; margin-right: 4px;"
                    >↓</a>
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
    </section>
  </div>
</div>

<style>
  .explorer {
    display: grid;
    grid-template-columns: minmax(240px, 320px) 1fr;
    gap: 20px;
    align-items: start;
  }
  @media (max-width: 720px) {
    .explorer { grid-template-columns: 1fr; }
  }
  tbody tr[draggable='true'] { cursor: grab; }
</style>
