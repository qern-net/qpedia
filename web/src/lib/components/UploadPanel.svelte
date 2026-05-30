<script lang="ts">
  import { onMount } from 'svelte';
  import { createFolder, uploadSource } from '$lib/api';

  let { folderPath = $bindable('/'), onUploaded }: {
    folderPath?: string;
    onUploaded?: () => void;
  } = $props();

  let busy = $state(false);
  let progress = $state<string | null>(null);
  let pct = $state(0); // 0..1 upload fraction, drives the bar
  let error = $state<string | null>(null);
  let fileInput: HTMLInputElement | undefined = $state();
  let folderInput: HTMLInputElement | undefined = $state();
  let dragHot = $state(false);
  /** Tracks which folder mode the next folder-pick should use, since one
   *  hidden <input webkitdirectory> serves both buttons. */
  let folderMode: 'mirror' | 'ai' = 'mirror';

  onMount(() => {
    // `webkitdirectory` isn't in the TS DOM types; set it imperatively
    // to avoid lint noise. All evergreen browsers support it.
    if (folderInput) (folderInput as any).webkitdirectory = true;
  });

  function note(msg: string | null) {
    progress = msg;
    if (msg) setTimeout(() => { if (progress === msg) progress = null; }, 4000);
  }

  // ── walking dragged folders via webkitGetAsEntry ────────────────────
  type Item = { relPath: string; file: File };
  async function readAllEntries(reader: any): Promise<any[]> {
    const all: any[] = [];
    while (true) {
      const batch: any[] = await new Promise((res, rej) => reader.readEntries(res, rej));
      if (batch.length === 0) break;
      all.push(...batch);
    }
    return all;
  }
  async function walkEntry(entry: any, prefix: string): Promise<Item[]> {
    if (entry.isFile) {
      const file = await new Promise<File>((res, rej) => entry.file(res, rej));
      return [{ relPath: prefix + entry.name, file }];
    }
    if (entry.isDirectory) {
      const reader = entry.createReader();
      const out: Item[] = [];
      const entries = await readAllEntries(reader);
      for (const child of entries) {
        const sub = await walkEntry(child, prefix + entry.name + '/');
        out.push(...sub);
      }
      return out;
    }
    return [];
  }

  // ── upload primitives ───────────────────────────────────────────────

  /** Flat upload: every file lands in `target` (defaults to selected
   *  folder). Use target='/' to engage the AI auto-organize path —
   *  classify.rs moves each root-/ file into /{doc_type}. */
  async function flatUpload(files: File[], target: string = folderPath) {
    if (files.length === 0) return;
    busy = true; error = null; pct = 0;
    let done = 0;
    progress = `Uploading 0 / ${files.length}…`;
    try {
      for (const f of files) {
        await uploadSource(target, f);
        done++;
        pct = done / files.length;
        progress = `Uploading ${done} / ${files.length}…`;
        if (done % 10 === 0) onUploaded?.(); // refresh the tree periodically
      }
      note(
        `Uploaded ${files.length} file${files.length === 1 ? '' : 's'}` +
        (target === '/' ? ' — AI will auto-organize by doc type as they classify.' : ` to ${target}. Watch each folder's progress bar as they ingest.`)
      );
      onUploaded?.();
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = false;
    }
  }

  /**
   * Mirror upload: replicate the OS folder structure under the selected
   * `folderPath` as pinned qpedia folders, then upload each file to its
   * mirrored location.
   *
   * The server slugifies folder paths, so we use the path returned by
   * createFolder (not the raw OS name) when uploading — keeps the
   * mapping correct for "Q4 Reports" → "q4-reports".
   */
  async function mirrorUpload(items: Item[]) {
    if (items.length === 0) return;
    busy = true; error = null; pct = 0;

    // 1. Collect every distinct parent folder we'll need.
    const folderSet = new Set<string>();
    for (const it of items) {
      const dir = it.relPath.includes('/')
        ? it.relPath.slice(0, it.relPath.lastIndexOf('/'))
        : '';
      if (!dir) continue;
      const parts = dir.split('/').filter(Boolean);
      let acc = folderPath === '/' ? '' : folderPath;
      for (const p of parts) {
        acc = acc + '/' + p;
        folderSet.add(acc);
      }
    }
    progress = `Creating ${folderSet.size} folder${folderSet.size === 1 ? '' : 's'}…`;

    // 2. Create parents first (sorted by depth) so the slug mapping is
    //    complete before we start uploading.
    const folders = Array.from(folderSet).sort((a, b) => a.length - b.length);
    const rawToSlug = new Map<string, string>();
    try {
      for (const raw of folders) {
        const r = await createFolder(raw, true);
        rawToSlug.set(raw, r.path);
      }
    } catch (e: any) {
      error = `Folder creation failed: ${e?.message ?? e}`;
      busy = false;
      return;
    }

    // 3. Upload each file to its slugified target folder.
    let done = 0;
    progress = `Uploading 0 / ${items.length}…`;
    try {
      for (const it of items) {
        const dir = it.relPath.includes('/')
          ? it.relPath.slice(0, it.relPath.lastIndexOf('/'))
          : '';
        const raw = dir
          ? (folderPath === '/' ? '/' + dir : folderPath + '/' + dir)
          : folderPath;
        const target = rawToSlug.get(raw) ?? raw;
        await uploadSource(target, it.file);
        done++;
        pct = done / items.length;
        progress = `Uploading ${done} / ${items.length}…`;
        if (done % 10 === 0) onUploaded?.();
      }
      note(
        `Uploaded ${items.length} file${items.length === 1 ? '' : 's'} into ` +
        `${folders.length} new 🔒 locked folder${folders.length === 1 ? '' : 's'} ` +
        `(mirroring your structure; the AI won't reorganize them). Watch each folder's progress bar as they ingest.`
      );
      onUploaded?.();
    } catch (e: any) {
      error = `Upload failed at ${done}/${items.length}: ${e?.message ?? e}`;
    } finally {
      busy = false;
    }
  }

  // ── input + drag handlers ───────────────────────────────────────────

  async function onFiles(files: FileList | null) {
    if (!files || files.length === 0) return;
    await flatUpload(Array.from(files));
    if (fileInput) fileInput.value = '';
  }

  function openFolderPicker(mode: 'mirror' | 'ai') {
    folderMode = mode;
    folderInput?.click();
  }

  async function onFolderPick(files: FileList | null) {
    if (!files || files.length === 0) return;
    const items: Item[] = Array.from(files).map((f) => ({
      relPath: (f as any).webkitRelativePath as string || f.name,
      file: f
    }));
    if (folderMode === 'mirror') {
      await mirrorUpload(items);
    } else {
      await flatUpload(items.map((i) => i.file), '/');
    }
    if (folderInput) folderInput.value = '';
  }

  async function onDrop(e: DragEvent) {
    e.preventDefault();
    dragHot = false;
    const dtItems = e.dataTransfer?.items;
    if (!dtItems || dtItems.length === 0) return;

    // Prefer webkitGetAsEntry — lets us walk folders.
    const entries: any[] = [];
    for (const it of Array.from(dtItems)) {
      const entry = (it as any).webkitGetAsEntry?.();
      if (entry) entries.push(entry);
    }
    if (entries.length === 0) {
      // Fallback: plain files only.
      const files = Array.from(e.dataTransfer?.files || []);
      await flatUpload(files);
      return;
    }

    const collected: Item[] = [];
    for (const entry of entries) {
      collected.push(...await walkEntry(entry, ''));
    }
    if (collected.length === 0) return;

    const anyDir = entries.some((x) => x.isDirectory);
    if (anyDir) await mirrorUpload(collected);
    else await flatUpload(collected.map((c) => c.file));
  }
</script>

<div
  class="card"
  class:drag-hot={dragHot}
  role="region"
  aria-label="Upload — drop files or a folder here"
  ondragover={(e) => { e.preventDefault(); dragHot = true; }}
  ondragleave={() => { dragHot = false; }}
  ondrop={onDrop}
>
  <div class="row" style="margin-bottom: 12px;">
    <span class="muted">Upload to:</span>
    <span class="mono" style="background: var(--bg-2); border: 1px solid var(--border); border-radius: 6px; padding: 2px 10px;">{folderPath}</span>
    <span class="muted" style="font-size: 12px;">— pick a folder in the tree below to change</span>
  </div>

  <div class="row" style="gap: 10px; flex-wrap: wrap;">
    <input
      type="file"
      multiple
      bind:this={fileInput}
      onchange={(e) => onFiles((e.target as HTMLInputElement).files)}
      disabled={busy}
    />
    <button
      onclick={() => openFolderPicker('mirror')}
      disabled={busy}
      title="Pick a folder; its subfolder structure is mirrored under the selected folder as pinned folders."
    >📁 Upload folder (mirror)</button>
    <button
      onclick={() => openFolderPicker('ai')}
      disabled={busy}
      title="Pick a folder; every file is dropped at root and the AI auto-organizes them by doc type."
    >🤖 Upload folder (AI organize)</button>
    <!-- One hidden directory input dispatched by either button via folderMode. -->
    <input
      type="file"
      multiple
      bind:this={folderInput}
      onchange={(e) => onFolderPick((e.target as HTMLInputElement).files)}
      style="display: none;"
    />
  </div>

  <div class="muted" style="font-size: 12px; margin-top: 10px; line-height: 1.5;">
    <strong>Drag</strong> a folder here to mirror its structure under <span class="mono">{folderPath}</span>;
    drag a flat batch of files for a flat upload.
    To let the AI design the structure for you, upload at <span class="mono">/</span> with the file
    picker (classification auto-moves each file into <span class="mono">/&lt;doc_type&gt;</span>).
    <br />
    <strong>🔒 Mirror upload</strong> creates <em>locked</em> folders — the AI auto-organizer
    won't move files in/out or rename them, so your structure is preserved exactly.
    After upload, watch each folder's progress bar in the tree fill as files ingest.
  </div>

  {#if busy}
    <div class="upload-bar" title="Upload progress">
      <span class="upload-fill" style="width: {Math.round(pct * 100)}%"></span>
    </div>
  {/if}
  {#if progress}
    <div class="muted" style="margin-top: 10px; font-size: 13px;">{progress}</div>
  {/if}
  {#if error}
    <div style="margin-top: 10px; color: var(--err); font-size: 13px;">{error}</div>
  {/if}
</div>

<style>
  .card.drag-hot {
    outline: 2px dashed var(--accent);
    background: var(--bg-2);
  }
  /* Overall upload progress while the POST loop runs. */
  .upload-bar {
    margin-top: 12px;
    height: 6px;
    border-radius: 3px;
    background: var(--bg-3, var(--border));
    overflow: hidden;
  }
  .upload-fill {
    display: block;
    height: 100%;
    background: var(--accent);
    transition: width 0.3s ease;
  }
</style>
