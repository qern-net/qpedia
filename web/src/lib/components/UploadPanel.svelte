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
    <span class="muted">Upload to:</span>
    <span class="mono" style="background: var(--bg-2); border: 1px solid var(--border); border-radius: 6px; padding: 2px 10px;">{folderPath}</span>
    <span class="muted" style="font-size: 12px;">— pick a folder in the tree below to change</span>
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
