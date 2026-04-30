<script lang="ts">
  import { uploadSource } from '$lib/api';

  let { folderPath = $bindable('/'), onUploaded }: {
    folderPath?: string;
    onUploaded?: () => void;
  } = $props();

  let busy = $state(false);
  let error = $state<string | null>(null);
  let fileInput: HTMLInputElement;

  async function handleFiles(files: FileList | null) {
    if (!files || files.length === 0) return;
    busy = true;
    error = null;
    try {
      // Sequential to keep server-side ordering predictable.
      for (const f of Array.from(files)) {
        await uploadSource(folderPath, f);
      }
      onUploaded?.();
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = false;
      if (fileInput) fileInput.value = '';
    }
  }
</script>

<div class="card">
  <div class="row" style="margin-bottom: 12px;">
    <label class="muted">Folder:</label>
    <input type="text" bind:value={folderPath} placeholder="/finance" style="flex: 1; max-width: 320px;" />
  </div>
  <div class="row">
    <input
      type="file"
      multiple
      bind:this={fileInput}
      onchange={(e) => handleFiles((e.target as HTMLInputElement).files)}
      disabled={busy}
    />
    {#if busy}<span class="muted">uploading…</span>{/if}
  </div>
  {#if error}
    <div style="margin-top: 8px; color: var(--err);">{error}</div>
  {/if}
</div>
