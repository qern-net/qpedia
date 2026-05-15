<script lang="ts">
  import { onMount } from 'svelte';
  import {
    deleteFolderAcl,
    getMe,
    listFolderAcls,
    setFolderAcl,
    type FolderAcl,
    type Me
  } from '$lib/api';

  let me = $state<Me | null>(null);
  let loaded = $state(false);
  let acls = $state<FolderAcl[]>([]);
  let error = $state<string | null>(null);
  let busy = $state(false);

  // New / edit form state.
  let formPath = $state('/');
  let formGroups = $state('');

  async function refresh() {
    try {
      const r = await listFolderAcls();
      acls = r.items;
      error = null;
    } catch (e: any) {
      error = String(e?.message ?? e);
    }
  }

  onMount(async () => {
    try { me = await getMe(); } catch {}
    loaded = true;
    if (me?.is_admin) await refresh();
  });

  function parseGroups(s: string): string[] {
    return s.split(',').map((g) => g.trim()).filter((g) => g.length > 0);
  }

  async function onSubmit() {
    const groups = parseGroups(formGroups);
    if (!formPath.trim() || groups.length === 0) {
      error = 'folder path and at least one group required';
      return;
    }
    busy = true; error = null;
    try {
      await setFolderAcl(formPath.trim(), groups);
      formGroups = '';
      await refresh();
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = false;
    }
  }

  async function onEdit(acl: FolderAcl) {
    formPath = acl.folder_path;
    formGroups = acl.acl.join(', ');
  }

  async function onRemove(acl: FolderAcl) {
    if (!confirm(`Remove the ACL for ${acl.folder_path}? Uploads will fall back to inheritance.`)) return;
    busy = true; error = null;
    try {
      await deleteFolderAcl(acl.folder_path);
      await refresh();
    } catch (e: any) {
      error = String(e?.message ?? e);
    } finally {
      busy = false;
    }
  }
</script>

<h1>Admin · Folder ACLs</h1>

{#if !loaded}
  <div class="muted">Loading…</div>
{:else if !me}
  <div class="card" style="border-color: var(--err); color: var(--err);">
    Sign in required.
  </div>
{:else if !me.is_admin}
  <div class="card" style="border-color: var(--err); color: var(--err);">
    Admin access required. Your groups: <span class="mono">{me.groups.join(', ') || '(none)'}</span>
  </div>
{:else}
  <div class="col" style="gap: 24px;">
    <div class="card">
      <h2 style="margin-top: 0;">Set ACL</h2>
      <p class="muted" style="margin: 0 0 12px;">
        Uploads to <span class="mono">/finance/q4</span> inherit from the
        closest ancestor that has a rule (e.g. <span class="mono">/finance</span>);
        with no match, fall back to the uploader's groups.
      </p>
      <form onsubmit={(e) => { e.preventDefault(); onSubmit(); }} class="col" style="gap: 12px;">
        <div class="row">
          <label class="muted" style="width: 140px;">Folder path:</label>
          <input
            type="text"
            bind:value={formPath}
            placeholder="/finance"
            style="flex: 1; max-width: 360px;"
          />
        </div>
        <div class="row">
          <label class="muted" style="width: 140px;">Groups (comma):</label>
          <input
            type="text"
            bind:value={formGroups}
            placeholder="finance-team, admin"
            style="flex: 1; max-width: 360px;"
          />
        </div>
        <div class="row">
          <button class="primary" type="submit" disabled={busy}>
            {busy ? '…' : 'Save'}
          </button>
          {#if error}
            <span style="color: var(--err);">{error}</span>
          {/if}
        </div>
      </form>
    </div>

    <div>
      <div class="row" style="margin-bottom: 12px;">
        <h2 style="margin: 0;">Current rules</h2>
        <span class="spacer"></span>
        <button onclick={refresh}>Refresh</button>
      </div>
      {#if acls.length === 0}
        <div class="card muted">No folder ACLs set — every upload uses the uploader's groups.</div>
      {:else}
        <div class="card" style="padding: 0; overflow: hidden;">
          <table>
            <thead>
              <tr>
                <th>Folder</th>
                <th>Groups</th>
                <th>Updated</th>
                <th>By</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each acls as a (a.folder_path)}
                <tr>
                  <td class="mono">{a.folder_path}</td>
                  <td>
                    {#each a.acl as g}
                      <span class="chip" style="background: var(--bg-3); color: var(--fg); margin-right: 4px;">{g}</span>
                    {/each}
                  </td>
                  <td class="muted" style="font-size: 12px;">{a.updated_at?.slice(0, 19) ?? '—'}</td>
                  <td class="mono muted" style="font-size: 12px;">{a.updated_by ?? '—'}</td>
                  <td>
                    <button onclick={() => onEdit(a)} style="font-size: 12px; padding: 4px 10px;">edit</button>
                    <button onclick={() => onRemove(a)} style="font-size: 12px; padding: 4px 10px;">remove</button>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>
  </div>
{/if}
