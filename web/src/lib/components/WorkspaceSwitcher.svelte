<script lang="ts">
  import { onMount } from 'svelte';
  import {
    listWorkspaces,
    switchWorkspace,
    createWorkspace,
    type Workspace
  } from '$lib/api';

  let workspaces = $state<Workspace[]>([]);
  let open = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let creating = $state(false);
  let newName = $state('');

  const active = $derived(workspaces.find((w) => w.active) ?? null);

  async function load() {
    try {
      workspaces = (await listWorkspaces()).workspaces;
    } catch {
      workspaces = [];
    }
  }
  onMount(load);

  async function choose(w: Workspace) {
    if (w.active) { open = false; return; }
    busy = true; error = null;
    try {
      await switchWorkspace(w.tenant);
      // Full reload so every page re-fetches under the new workspace.
      window.location.href = '/';
    } catch (e: any) {
      error = String(e?.message ?? e);
      busy = false;
    }
  }

  async function create() {
    const name = newName.trim();
    if (!name) return;
    busy = true; error = null;
    try {
      const r = await createWorkspace(name);
      await switchWorkspace(r.tenant);
      window.location.href = '/';
    } catch (e: any) {
      error = String(e?.message ?? e);
      busy = false;
    }
  }
</script>

<div class="ws-switcher">
  <button class="ws-trigger" onclick={() => (open = !open)} title="Switch workspace">
    <span class="ws-dot" class:org={active?.kind === 'org'}></span>
    <span class="ws-name">{active?.name ?? 'Workspace'}</span>
    <span class="ws-caret">▾</span>
  </button>

  {#if open}
    <!-- click-away backdrop -->
    <button class="ws-backdrop" onclick={() => (open = false)} aria-label="Close"></button>
    <div class="ws-menu">
      <div class="ws-section">Your workspaces</div>
      {#each workspaces as w (w.tenant)}
        <button class="ws-item" class:active={w.active} disabled={busy} onclick={() => choose(w)}>
          <span class="ws-dot" class:org={w.kind === 'org'}></span>
          <span class="ws-item-name">{w.name}</span>
          <span class="ws-role muted">{w.kind === 'individual' ? 'personal' : w.role}</span>
          {#if w.active}<span class="ws-check">✓</span>{/if}
        </button>
      {/each}

      <div class="ws-divider"></div>
      {#if creating}
        <form class="ws-create" onsubmit={(e) => { e.preventDefault(); create(); }}>
          <input
            type="text"
            bind:value={newName}
            placeholder="Organization name"
            disabled={busy}
          />
          <button class="primary" type="submit" disabled={busy || !newName.trim()}>Create</button>
        </form>
      {:else}
        <button class="ws-item ws-new" onclick={() => (creating = true)}>＋ Create organization</button>
      {/if}
      {#if error}<div class="ws-error">{error}</div>{/if}
    </div>
  {/if}
</div>

<style>
  .ws-switcher { position: relative; }
  .ws-trigger {
    display: flex; align-items: center; gap: 8px;
    background: var(--bg-2); border: 1px solid var(--border);
    border-radius: 6px; padding: 5px 10px; color: var(--fg); font: inherit; cursor: pointer;
    max-width: 220px;
  }
  .ws-trigger:hover { background: var(--bg-3); }
  .ws-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ws-caret { color: var(--fg-dim); font-size: 11px; }
  .ws-dot {
    width: 8px; height: 8px; border-radius: 50%;
    background: var(--warn); flex: none;            /* individual = amber */
  }
  .ws-dot.org { background: var(--accent); }        /* org = sky */

  .ws-backdrop {
    position: fixed; inset: 0; background: transparent; border: none; cursor: default; z-index: 40;
  }
  .ws-menu {
    position: absolute; top: calc(100% + 6px); left: 0; z-index: 41;
    min-width: 260px; background: var(--bg-2); border: 1px solid var(--border);
    border-radius: 8px; padding: 6px; box-shadow: 0 8px 24px rgba(0,0,0,0.4);
  }
  .ws-section {
    font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em;
    color: var(--fg-dim); padding: 6px 8px 4px;
  }
  .ws-item {
    display: flex; align-items: center; gap: 8px; width: 100%;
    background: none; border: none; color: var(--fg); font: inherit; text-align: left;
    padding: 7px 8px; border-radius: 6px; cursor: pointer;
  }
  .ws-item:hover { background: var(--bg-3); }
  .ws-item.active { background: var(--bg-3); }
  .ws-item-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ws-role { font-size: 11px; }
  .ws-check { color: var(--accent); }
  .ws-new { color: var(--accent); }
  .ws-divider { height: 1px; background: var(--border); margin: 6px 4px; }
  .ws-create { display: flex; gap: 6px; padding: 4px; }
  .ws-create input { flex: 1; min-width: 0; }
  .ws-error { color: var(--err); font-size: 12px; padding: 4px 8px; }
</style>
