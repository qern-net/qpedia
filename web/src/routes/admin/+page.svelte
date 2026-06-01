<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    getQueueOverview,
    type QueueOverview,
    deleteConnector,
    deleteFolderAcl,
    enqueueLint,
    enqueueReembed,
    getLintReport,
    getMe,
    googleDriveAuthorizeUrl,
    listConnectors,
    listFolderAcls,
    listFolders,
    listSources,
    listStalledSources,
    resumeStalledSources,
    setFolderAcl,
    triggerConnectorSync,
    listWorkspaceMembers,
    removeWorkspaceMember,
    listWorkspaceInvites,
    createWorkspaceInvite,
    deleteWorkspaceInvite,
    listDomains,
    addDomain,
    verifyDomain,
    deleteDomain,
    type Connector,
    type Folder,
    type FolderAcl,
    type LintReport,
    type Me,
    type Source,
    type WorkspaceMember,
    type WorkspaceInvite,
    type WorkspaceDomain
  } from '$lib/api';
  import { page } from '$app/stores';
  import StatusChip from '$lib/components/StatusChip.svelte';
  import FolderTree from '$lib/components/FolderTree.svelte';

  let me = $state<Me | null>(null);
  let loaded = $state(false);
  let acls = $state<FolderAcl[]>([]);
  let error = $state<string | null>(null);
  let busy = $state(false);

  // Processing-queue state (live-polled).
  let queue = $state<QueueOverview | null>(null);
  let queueError = $state<string | null>(null);
  let queueTimer: ReturnType<typeof setInterval> | null = null;

  // Stalled sources state
  let stalled = $state<Source[]>([]);
  let stalledError = $state<string | null>(null);
  let stalledLoaded = $state(false);
  let resuming = $state(false);
  let resumeMsg = $state<string | null>(null);

  // Reembed state
  let reembedding = $state(false);
  let reembedMsg = $state<string | null>(null);
  let reembedError = $state<string | null>(null);

  // Lint report state
  let lintReport = $state<LintReport | null>(null);
  let lintLoading = $state(false);
  let lintError = $state<string | null>(null);
  let lintRunning = $state(false);
  let lintRunMsg = $state<string | null>(null);

  // Connectors state
  let connectors = $state<Connector[]>([]);
  let connectorsError = $state<string | null>(null);
  let connectMsg = $state<string | null>(null);
  let gdriveFolder = $state('');
  let connecting = $state(false);

  // Members & invites state
  let members = $state<WorkspaceMember[]>([]);
  let invites = $state<WorkspaceInvite[]>([]);
  let membersError = $state<string | null>(null);
  let inviteEmail = $state('');
  let inviteRole = $state<'member' | 'admin'>('member');
  let lastInviteLink = $state<string | null>(null);

  // Domains state
  let domains = $state<WorkspaceDomain[]>([]);
  let domainsError = $state<string | null>(null);
  let newDomain = $state('');
  let pendingTxt = $state<{ name: string; value: string } | null>(null);
  let verifyingDomain = $state<string | null>(null);

  // New / edit form state.
  let formPath = $state('/');
  let formGroups = $state('');

  // Folder tree (read-only picker for the ACL target).
  let treeFolders = $state<Folder[]>([]);
  let treeSources = $state<Source[]>([]);

  async function refresh() {
    try {
      const [r, fld, srcs] = await Promise.all([
        listFolderAcls(),
        listFolders(),
        listSources('/', 1000)
      ]);
      acls = r.items;
      treeFolders = fld.items;
      treeSources = srcs;
      error = null;
    } catch (e: any) {
      error = String(e?.message ?? e);
    }
  }

  async function refreshQueue() {
    try {
      queue = await getQueueOverview();
      queueError = null;
    } catch (e: any) {
      queueError = String(e?.message ?? e);
    }
  }

  /** Total jobs not yet in a terminal state — drives the poll cadence. */
  function queueInFlight(q: QueueOverview | null): number {
    if (!q) return 0;
    return (q.by_state.queued ?? 0) + (q.by_state.running ?? 0);
  }

  function fmtAge(secs: number): string {
    if (secs < 60) return `${secs}s`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
    return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  }

  async function refreshStalled() {
    stalledError = null;
    try {
      const r = await listStalledSources();
      stalled = r.sources;
    } catch (e: any) {
      stalledError = String(e?.message ?? e);
    } finally {
      stalledLoaded = true;
    }
  }

  async function refreshLint() {
    lintLoading = true; lintError = null;
    try {
      lintReport = await getLintReport();
    } catch (e: any) {
      lintError = String(e?.message ?? e);
    } finally {
      lintLoading = false;
    }
  }

  async function onRunLint() {
    lintRunning = true; lintRunMsg = null; lintError = null;
    try {
      const r = await enqueueLint();
      lintRunMsg = `Lint job queued (${r.job_id.slice(0, 10)}…). Re-fetch the report in a moment.`;
    } catch (e: any) {
      lintError = String(e?.message ?? e);
    } finally {
      lintRunning = false;
    }
  }

  function lintIssueCount(r: LintReport): number {
    return (
      r.orphans.length +
      r.broken_links.length +
      r.index_drift.missing_from_index.length +
      r.index_drift.stale_in_index.length +
      r.stale_source_ids.length +
      r.duplicates.length +
      r.contradictions.length
    );
  }

  async function refreshConnectors() {
    connectorsError = null;
    try {
      connectors = (await listConnectors()).items;
    } catch (e: any) {
      connectorsError = String(e?.message ?? e);
    }
  }

  async function refreshMembers() {
    membersError = null;
    try {
      members = (await listWorkspaceMembers()).items;
      invites = (await listWorkspaceInvites()).items;
    } catch (e: any) {
      membersError = String(e?.message ?? e);
    }
  }

  async function onInvite() {
    membersError = null; lastInviteLink = null;
    const email = inviteEmail.trim();
    if (!email) return;
    try {
      const r = await createWorkspaceInvite(email, inviteRole);
      lastInviteLink = `${window.location.origin}${r.invite_path}`;
      inviteEmail = '';
      await refreshMembers();
    } catch (e: any) {
      membersError = String(e?.message ?? e);
    }
  }

  async function onRevokeInvite(i: WorkspaceInvite) {
    try { await deleteWorkspaceInvite(i.id); await refreshMembers(); }
    catch (e: any) { membersError = String(e?.message ?? e); }
  }

  async function onRemoveMember(m: WorkspaceMember) {
    if (!confirm(`Remove ${m.email || m.user_id} from this workspace?`)) return;
    try { await removeWorkspaceMember(m.user_id); await refreshMembers(); }
    catch (e: any) { membersError = String(e?.message ?? e); }
  }

  async function copyInviteLink() {
    if (lastInviteLink) { try { await navigator.clipboard.writeText(lastInviteLink); } catch {} }
  }

  async function refreshDomains() {
    domainsError = null;
    try { domains = (await listDomains()).items; }
    catch (e: any) { domainsError = String(e?.message ?? e); }
  }

  async function onAddDomain() {
    const d = newDomain.trim();
    if (!d) return;
    domainsError = null; pendingTxt = null;
    try {
      const r = await addDomain(d);
      pendingTxt = { name: r.txt_name, value: r.txt_value };
      newDomain = '';
      await refreshDomains();
    } catch (e: any) { domainsError = String(e?.message ?? e); }
  }

  async function onVerifyDomain(d: WorkspaceDomain) {
    verifyingDomain = d.domain; domainsError = null;
    try {
      await verifyDomain(d.domain);
      pendingTxt = null;
      await refreshDomains();
    } catch (e: any) {
      domainsError = String(e?.message ?? e);
    } finally {
      verifyingDomain = null;
    }
  }

  async function onDeleteDomain(d: WorkspaceDomain) {
    if (!confirm(`Remove ${d.domain}?`)) return;
    try { await deleteDomain(d.domain); await refreshDomains(); }
    catch (e: any) { domainsError = String(e?.message ?? e); }
  }

  async function onConnectGoogleDrive() {
    connecting = true; connectMsg = null; connectorsError = null;
    try {
      const url = await googleDriveAuthorizeUrl(gdriveFolder.trim() || undefined);
      // Top-level navigation to Google's consent screen; Google redirects
      // back to /admin?google_connected=1 (or ?google_error=…).
      window.location.href = url;
    } catch (e: any) {
      connectorsError = String(e?.message ?? e);
      connecting = false;
    }
  }

  async function onSyncConnector(c: Connector) {
    try {
      await triggerConnectorSync(c.id);
      connectMsg = `Sync queued for ${c.name}.`;
    } catch (e: any) {
      connectorsError = String(e?.message ?? e);
    }
  }

  async function onDeleteConnector(c: Connector) {
    if (!confirm(`Delete connector "${c.name}"? Already-ingested docs stay; no new syncs run.`)) return;
    try {
      await deleteConnector(c.id);
      await refreshConnectors();
    } catch (e: any) {
      connectorsError = String(e?.message ?? e);
    }
  }

  onMount(async () => {
    try { me = await getMe(); } catch {}
    loaded = true;
    if (me?.is_admin) {
      await Promise.all([refresh(), refreshQueue(), refreshStalled(), refreshLint(), refreshConnectors(), refreshMembers(), refreshDomains()]);
      // Poll the queue: fast while work is in flight, slow when idle.
      queueTimer = setInterval(() => {
        refreshQueue();
      }, 2000);
      // Surface the result of a returning Google OAuth round-trip.
      const params = $page.url.searchParams;
      if (params.get('google_connected')) {
        connectMsg = 'Google Drive connected — first sync will run shortly.';
      } else if (params.get('google_error')) {
        connectorsError = `Google connect failed: ${params.get('google_error')}`;
      }
    }
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

  async function onResume() {
    if (!confirm(`Re-enqueue all ${stalled.length} stalled source(s) for processing?`)) return;
    resuming = true; resumeMsg = null;
    try {
      const r = await resumeStalledSources();
      resumeMsg = `${r.enqueued} source(s) re-enqueued.`;
      await refreshStalled();
    } catch (e: any) {
      stalledError = String(e?.message ?? e);
    } finally {
      resuming = false;
    }
  }

  async function onReembed() {
    if (!confirm(
      'Rebuild the wiki_pages search index from the git wiki repo?\n\n' +
      'This clears the embeddings for this tenant and re-embeds every page. ' +
      'Search will be unavailable until the job completes. Continue?'
    )) return;
    reembedding = true; reembedMsg = null; reembedError = null;
    try {
      const r = await enqueueReembed();
      reembedMsg = `Reembed job queued (${r.job_id.slice(0, 10)}…). Search will rebuild in the background.`;
    } catch (e: any) {
      reembedError = String(e?.message ?? e);
    } finally {
      reembedding = false;
    }
  }

  function fmtDate(s: string) {
    return s ? new Date(s).toLocaleString() : '—';
  }

  onDestroy(() => { if (queueTimer) clearInterval(queueTimer); });
</script>

<h1>Admin</h1>

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
  <div class="col" style="gap: 32px;">

    <!-- ── Processing queue (live) ── -->
    <div>
      <div class="row" style="margin-bottom: 8px; align-items: baseline;">
        <h2 style="margin: 0;">Processing queue</h2>
        <span class="spacer"></span>
        {#if queue}
          <span class="muted" style="font-size: 12px;">
            {queueInFlight(queue) > 0 ? 'live · refreshing every 2s' : 'idle'}
          </span>
        {/if}
      </div>

      {#if queueError}
        <div class="card" style="border-color: var(--err); color: var(--err);">{queueError}</div>
      {:else if !queue}
        <div class="card muted">Loading queue…</div>
      {:else}
        <!-- State counts -->
        <div class="row" style="gap: 8px; flex-wrap: wrap; margin-bottom: 12px;">
          {#each [['queued', 'chip-pending'], ['running', 'chip-embedding'], ['done', 'chip-done'], ['dead', 'chip-failed']] as [st, cls]}
            <span class="chip {cls}" style="display: inline-flex; gap: 6px;">
              {st}<strong>{queue.by_state[st] ?? 0}</strong>
            </span>
          {/each}
        </div>

        <!-- Live processors: running jobs grouped by worker -->
        {#if queue.active.filter((j) => j.state === 'running').length > 0}
          <div class="card" style="padding: 0; overflow: hidden; margin-bottom: 10px;">
            <table>
              <thead>
                <tr><th>Processor</th><th>Job</th><th>Source</th><th>Running for</th></tr>
              </thead>
              <tbody>
                {#each queue.active.filter((j) => j.state === 'running') as j (j.id)}
                  <tr>
                    <td class="mono">{j.worker ?? '—'}</td>
                    <td>{j.kind} <span class="mono muted" style="font-size: 11px;">#{j.id}</span></td>
                    <td class="mono" style="font-size: 12px; max-width: 360px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">{j.source ?? '—'}</td>
                    <td>{fmtAge(j.age_secs)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {:else}
          <div class="muted" style="font-size: 13px; margin-bottom: 6px;">No jobs running right now.</div>
        {/if}

        <!-- Queued backlog (just a count + the next few) -->
        {#if (queue.by_state.queued ?? 0) > 0}
          <div class="muted" style="font-size: 13px;">
            {queue.by_state.queued} queued · next:
            {queue.active.filter((j) => j.state === 'queued').slice(0, 5).map((j) => j.source ?? j.kind).join(', ')}{(queue.by_state.queued ?? 0) > 5 ? ' …' : ''}
          </div>
        {/if}

        <!-- Recent failures -->
        {#if queue.dead.length > 0}
          <details style="margin-top: 10px;">
            <summary class="muted" style="cursor: pointer; font-size: 13px;">⚠ {queue.dead.length} dead job(s) — last error each</summary>
            <div class="card" style="margin-top: 6px;">
              {#each queue.dead as d (d.id)}
                <div style="padding: 4px 0; border-bottom: 1px solid var(--border); font-size: 12px;">
                  <span class="mono">{d.kind} #{d.id}</span>
                  {#if d.source}<span class="mono muted"> · {d.source}</span>{/if}
                  <div class="mono" style="color: var(--err); white-space: pre-wrap; word-break: break-word;">{d.error ?? '(no error recorded)'}</div>
                </div>
              {/each}
            </div>
          </details>
        {/if}
      {/if}
    </div>

    <!-- ── Stalled Sources ── -->
    <div>
      <div class="row" style="margin-bottom: 12px;">
        <h2 style="margin: 0;">Stalled Sources</h2>
        <span class="spacer"></span>
        <button onclick={refreshStalled}>Refresh</button>
        {#if stalled.length > 0}
          <button class="primary" onclick={onResume} disabled={resuming}>
            {resuming ? 'Resuming…' : `Resume all (${stalled.length})`}
          </button>
        {/if}
      </div>

      {#if stalledError}
        <div class="card" style="border-color: var(--err); color: var(--err); margin-bottom: 12px;">{stalledError}</div>
      {/if}
      {#if resumeMsg}
        <div class="card" style="border-color: var(--ok); color: var(--ok); margin-bottom: 12px;">{resumeMsg}</div>
      {/if}

      {#if !stalledLoaded}
        <div class="muted">Loading…</div>
      {:else if stalled.length === 0}
        <div class="card muted">No stalled sources — pipeline is healthy.</div>
      {:else}
        <div class="card" style="padding: 0; overflow: hidden;">
          <table>
            <thead>
              <tr>
                <th>Filename</th>
                <th>Status</th>
                <th>Folder</th>
                <th>Uploaded</th>
                <th>ID</th>
              </tr>
            </thead>
            <tbody>
              {#each stalled as src (src.id)}
                <tr>
                  <td>
                    <div>{src.filename}</div>
                    <div class="muted" style="font-size: 11px;">{src.mime}</div>
                  </td>
                  <td><StatusChip status={src.status} /></td>
                  <td class="mono muted">{src.folder_path}</td>
                  <td class="muted" style="font-size: 12px;">{fmtDate(src.created_at)}</td>
                  <td class="mono muted" style="font-size: 11px;">{src.id}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>

    <!-- ── Rebuild Search Index ── -->
    <div class="card">
      <div class="row" style="margin-bottom: 8px;">
        <div>
          <h2 style="margin: 0 0 4px;">Rebuild Search Index</h2>
          <p class="muted" style="margin: 0; font-size: 12px;">
            Clears the wiki_pages search index and re-embeds every page from the git wiki repo.
            Use when search is broken or the embedding model changed.
            Git is the source of truth.
          </p>
        </div>
        <span class="spacer"></span>
        <button onclick={onReembed} disabled={reembedding} style="white-space: nowrap;">
          {reembedding ? 'Queuing…' : 'Rebuild from git'}
        </button>
      </div>
      {#if reembedError}
        <div style="color: var(--err); font-size: 12px;">{reembedError}</div>
      {/if}
      {#if reembedMsg}
        <div style="color: var(--ok); font-size: 12px;">{reembedMsg}</div>
      {/if}
    </div>

    <!-- ── Members & Invites ── -->
    <div class="card">
      <h2 style="margin: 0 0 4px;">Members & Invites</h2>
      {#if me.tenant_kind === 'individual'}
        <p class="muted" style="margin: 0; font-size: 13px;">
          This is your <strong>personal workspace</strong> — it's just you. Use the
          workspace switcher (top right) to <strong>Create an organization</strong>,
          then invite teammates here.
        </p>
      {:else}
        <p class="muted" style="margin: 0 0 12px; font-size: 12px;">
          People in this organization. Invite by email — they accept via a link and
          join with the role you choose.
        </p>

        {#if membersError}
          <div class="card" style="border-color: var(--err); color: var(--err); margin-bottom: 12px;">{membersError}</div>
        {/if}

        <!-- Invite form -->
        <div class="row" style="gap: 8px; flex-wrap: wrap; margin-bottom: 12px;">
          <input type="email" bind:value={inviteEmail} placeholder="teammate@company.com" style="flex: 1; min-width: 240px;" />
          <select bind:value={inviteRole} style="background: var(--bg-2); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; padding: 8px;">
            <option value="member">member</option>
            <option value="admin">admin</option>
          </select>
          <button class="primary" onclick={onInvite} disabled={!inviteEmail.trim()}>Send invite</button>
        </div>

        {#if lastInviteLink}
          <div class="card" style="border-color: var(--ok); margin-bottom: 12px; font-size: 13px;">
            Invite created. Share this link with them:
            <div class="row" style="gap: 8px; margin-top: 6px;">
              <span class="mono" style="flex: 1; overflow: hidden; text-overflow: ellipsis;">{lastInviteLink}</span>
              <button onclick={copyInviteLink} style="font-size: 12px; padding: 4px 10px;">Copy</button>
            </div>
            <div class="muted" style="font-size: 11px; margin-top: 4px;">
              (Email delivery is coming; for now copy the link.)
            </div>
          </div>
        {/if}

        <!-- Members table -->
        <div class="card" style="padding: 0; overflow: hidden; margin-bottom: 12px;">
          <table>
            <thead><tr><th>Member</th><th>Role</th><th>Joined</th><th></th></tr></thead>
            <tbody>
              {#each members as m (m.user_id)}
                <tr>
                  <td>{m.email || m.user_id}{m.is_you ? ' (you)' : ''}</td>
                  <td><span class="chip" style="background: var(--bg-3); color: var(--fg);">{m.role}</span></td>
                  <td class="muted" style="font-size: 12px;">{m.joined_at.slice(0, 10)}</td>
                  <td>
                    {#if !m.is_you && m.role !== 'owner'}
                      <button onclick={() => onRemoveMember(m)} style="font-size: 12px; padding: 4px 10px;">remove</button>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>

        {#if invites.length > 0}
          <div class="muted" style="font-size: 12px; margin-bottom: 6px;">Pending invites</div>
          <div class="card" style="padding: 0; overflow: hidden;">
            <table>
              <thead><tr><th>Email</th><th>Role</th><th>Expires</th><th></th></tr></thead>
              <tbody>
                {#each invites as i (i.id)}
                  <tr>
                    <td>{i.email}</td>
                    <td class="muted">{i.role}</td>
                    <td class="muted" style="font-size: 12px;">{i.expires_at.slice(0, 10)}</td>
                    <td><button onclick={() => onRevokeInvite(i)} style="font-size: 12px; padding: 4px 10px;">revoke</button></td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}

        <!-- ── Verified domains ── -->
        <div style="margin-top: 20px;">
          <h3 style="margin: 0 0 4px; text-transform: none; letter-spacing: 0; color: var(--fg);">Domains</h3>
          <p class="muted" style="margin: 0 0 12px; font-size: 12px;">
            Verify a domain you control to enable domain-based features (and, later,
            SSO enforcement). Add the TXT record below to your DNS, then click Verify.
          </p>

          {#if domainsError}
            <div class="card" style="border-color: var(--err); color: var(--err); margin-bottom: 12px;">{domainsError}</div>
          {/if}

          <div class="row" style="gap: 8px; flex-wrap: wrap; margin-bottom: 12px;">
            <input type="text" bind:value={newDomain} placeholder="acme.com" style="flex: 1; min-width: 200px;" />
            <button class="primary" onclick={onAddDomain} disabled={!newDomain.trim()}>Add domain</button>
          </div>

          {#if pendingTxt}
            <div class="card" style="margin-bottom: 12px; font-size: 13px;">
              Add this <strong>TXT</strong> record to <span class="mono">{pendingTxt.name}</span>, then click Verify:
              <div class="mono" style="background: var(--code-bg); padding: 8px 10px; border-radius: 6px; margin-top: 6px; word-break: break-all;">
                {pendingTxt.value}
              </div>
              <div class="muted" style="font-size: 11px; margin-top: 4px;">DNS changes can take a few minutes to propagate.</div>
            </div>
          {/if}

          {#if domains.length > 0}
            <div class="card" style="padding: 0; overflow: hidden;">
              <table>
                <thead><tr><th>Domain</th><th>Status</th><th></th></tr></thead>
                <tbody>
                  {#each domains as d (d.domain)}
                    <tr>
                      <td class="mono">{d.domain}</td>
                      <td>
                        {#if d.verified}
                          <span class="chip" style="background: #14532d; color: #bbf7d0;">verified · {d.verified_via}</span>
                        {:else}
                          <span class="chip" style="background: #78350f; color: #fde68a;">pending</span>
                        {/if}
                      </td>
                      <td style="white-space: nowrap;">
                        {#if !d.verified}
                          <button onclick={() => onVerifyDomain(d)} disabled={verifyingDomain === d.domain} style="font-size: 12px; padding: 4px 10px; margin-right: 4px;">
                            {verifyingDomain === d.domain ? 'checking…' : 'Verify'}
                          </button>
                        {/if}
                        <button onclick={() => onDeleteDomain(d)} style="font-size: 12px; padding: 4px 10px;">remove</button>
                      </td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </div>
      {/if}
    </div>

    <!-- ── Connectors ── -->
    <div class="card">
      <div class="row" style="margin-bottom: 8px;">
        <div>
          <h2 style="margin: 0 0 4px;">Connectors</h2>
          <p class="muted" style="margin: 0; font-size: 12px;">
            External sources that auto-sync into the wiki. Google Drive connects
            via your Google account (read-only); set up Google SSO and grant
            Drive access in one click.
          </p>
        </div>
        <span class="spacer"></span>
        <button onclick={refreshConnectors} style="white-space: nowrap;">Refresh</button>
      </div>

      {#if connectorsError}
        <div class="card" style="border-color: var(--err); color: var(--err); margin-bottom: 12px;">{connectorsError}</div>
      {/if}
      {#if connectMsg}
        <div class="card" style="border-color: var(--ok); color: var(--ok); margin-bottom: 12px;">{connectMsg}</div>
      {/if}

      <!-- Connect Google Drive -->
      <div class="row" style="gap: 10px; flex-wrap: wrap; margin-bottom: 14px;">
        <input
          type="text"
          bind:value={gdriveFolder}
          placeholder="Drive folder id (optional — whole Drive if blank)"
          style="flex: 1; min-width: 280px;"
        />
        <button class="primary" onclick={onConnectGoogleDrive} disabled={connecting} style="white-space: nowrap;">
          {connecting ? 'Redirecting…' : '🔗 Connect Google Drive'}
        </button>
      </div>

      {#if connectors.length === 0}
        <div class="muted" style="font-size: 13px;">No connectors yet.</div>
      {:else}
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Enabled</th>
              <th>Last run</th>
              <th>Last error</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each connectors as c (c.id)}
              <tr>
                <td>{c.name}</td>
                <td class="mono">{c.kind}</td>
                <td>{c.enabled ? 'yes' : 'no'}</td>
                <td class="muted" style="font-size: 12px;">{c.last_run_at?.slice(0, 19).replace('T', ' ') ?? '—'}</td>
                <td class="muted" style="font-size: 12px; max-width: 240px; color: {c.last_error ? 'var(--err)' : 'var(--fg-dim)'};">{c.last_error ?? '—'}</td>
                <td style="white-space: nowrap;">
                  <button onclick={() => onSyncConnector(c)} style="font-size: 12px; padding: 4px 10px; margin-right: 4px;">sync</button>
                  <button onclick={() => onDeleteConnector(c)} style="font-size: 12px; padding: 4px 10px;">delete</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>

    <!-- ── Wiki Lint Report ── -->
    <div>
      <div class="row" style="margin-bottom: 8px;">
        <div>
          <h2 style="margin: 0 0 4px;">Wiki Lint</h2>
          <p class="muted" style="margin: 0; font-size: 12px;">
            Periodic health check — orphans, broken links, duplicates, index
            drift, stale source refs, contradictions. The latest report is
            committed at <span class="mono">_meta/lint.json</span>.
            {#if lintReport?.generated_at}
              Last run: <span class="mono">{lintReport.generated_at.slice(0, 19).replace('T', ' ')}</span>
              over {lintReport.page_count} page{lintReport.page_count === 1 ? '' : 's'}.
            {/if}
          </p>
        </div>
        <span class="spacer"></span>
        <button onclick={refreshLint} disabled={lintLoading} style="white-space: nowrap;">
          {lintLoading ? '…' : 'Refresh'}
        </button>
        <button onclick={onRunLint} disabled={lintRunning} class="primary" style="white-space: nowrap;">
          {lintRunning ? 'Queuing…' : 'Run lint now'}
        </button>
      </div>

      {#if lintError}
        <div class="card" style="border-color: var(--err); color: var(--err); margin-bottom: 12px;">{lintError}</div>
      {/if}
      {#if lintRunMsg}
        <div class="card" style="border-color: var(--ok); color: var(--ok); margin-bottom: 12px;">{lintRunMsg}</div>
      {/if}

      {#if !lintReport}
        <div class="card muted">
          No lint report yet — click <strong>Run lint now</strong>. The job
          completes within a few seconds for small wikis; refresh to see
          the report.
        </div>
      {:else}
        {@const r = lintReport}
        {@const total = lintIssueCount(r)}
        <div class="card" style="padding: 0; overflow: hidden;">
          <div class="row" style="padding: 12px 14px; border-bottom: 1px solid var(--border);">
            <strong>{total} issue{total === 1 ? '' : 's'}</strong>
            <span class="spacer"></span>
            <span class="muted" style="font-size: 12px;">
              {r.orphans.length} orphans · {r.broken_links.length} broken ·
              {r.index_drift.missing_from_index.length + r.index_drift.stale_in_index.length} index drift ·
              {r.stale_source_ids.length} stale src ·
              {r.duplicates.length} duplicates ·
              {r.contradictions.length} contradictions
            </span>
          </div>

          {#if r.orphans.length > 0}
            <details class="lint-cat" open>
              <summary>Orphans <span class="badge">{r.orphans.length}</span></summary>
              <p class="muted" style="font-size: 12px; margin: 6px 0 8px;">
                Pages no one links to (excluding system files).
              </p>
              <ul>
                {#each r.orphans as path}
                  <li><a href={`/wiki/${path}`} class="mono">{path}</a></li>
                {/each}
              </ul>
            </details>
          {/if}

          {#if r.broken_links.length > 0}
            <details class="lint-cat" open>
              <summary>Broken links <span class="badge">{r.broken_links.length}</span></summary>
              <ul>
                {#each r.broken_links as bl}
                  <li>
                    <a href={`/wiki/${bl.page}`} class="mono">{bl.page}</a>
                    <span class="muted">→</span>
                    <span class="mono" style="color: var(--err);">{bl.target}</span>
                  </li>
                {/each}
              </ul>
            </details>
          {/if}

          {#if r.index_drift.missing_from_index.length > 0 || r.index_drift.stale_in_index.length > 0}
            <details class="lint-cat">
              <summary>
                Index drift
                <span class="badge">{r.index_drift.missing_from_index.length + r.index_drift.stale_in_index.length}</span>
              </summary>
              {#if r.index_drift.missing_from_index.length > 0}
                <p class="muted" style="font-size: 12px; margin: 6px 0 4px;">Missing from index.md:</p>
                <ul>
                  {#each r.index_drift.missing_from_index as path}
                    <li><a href={`/wiki/${path}`} class="mono">{path}</a></li>
                  {/each}
                </ul>
              {/if}
              {#if r.index_drift.stale_in_index.length > 0}
                <p class="muted" style="font-size: 12px; margin: 6px 0 4px;">In index.md but not on disk:</p>
                <ul>
                  {#each r.index_drift.stale_in_index as path}
                    <li><span class="mono" style="color: var(--err);">{path}</span></li>
                  {/each}
                </ul>
              {/if}
            </details>
          {/if}

          {#if r.stale_source_ids.length > 0}
            <details class="lint-cat">
              <summary>Stale source IDs <span class="badge">{r.stale_source_ids.length}</span></summary>
              <p class="muted" style="font-size: 12px; margin: 6px 0 8px;">
                Pages reference these source IDs in frontmatter, but the source row no longer exists.
              </p>
              <ul>
                {#each r.stale_source_ids as sid}
                  <li class="mono" style="color: var(--err);">{sid}</li>
                {/each}
              </ul>
            </details>
          {/if}

          {#if r.duplicates.length > 0}
            <details class="lint-cat" open>
              <summary>Near-duplicate pages <span class="badge">{r.duplicates.length}</span></summary>
              <p class="muted" style="font-size: 12px; margin: 6px 0 8px;">
                Pairs with cosine similarity ≥ 0.93.
              </p>
              <ul>
                {#each r.duplicates as d}
                  <li>
                    <a href={`/wiki/${d.a}`} class="mono">{d.a}</a>
                    <span class="muted">↔</span>
                    <a href={`/wiki/${d.b}`} class="mono">{d.b}</a>
                    <span class="badge">{(d.certainty * 100).toFixed(0)}%</span>
                  </li>
                {/each}
              </ul>
            </details>
          {/if}

          {#if r.contradictions.length > 0}
            <details class="lint-cat" open>
              <summary>Contradictions <span class="badge">{r.contradictions.length}</span></summary>
              <p class="muted" style="font-size: 12px; margin: 6px 0 8px;">
                Pages sharing a tag with claims that contradict each other, per the LLM.
              </p>
              <ul>
                {#each r.contradictions as c}
                  <li style="margin-bottom: 8px;">
                    <div style="margin-bottom: 4px;">
                      <span class="chip" style="background: var(--bg-3); color: var(--fg);">#{c.tag}</span>
                      <span class="muted">— {c.summary}</span>
                    </div>
                    <div>
                      {#each c.pages as p}
                        <a href={`/wiki/${p}`} class="mono" style="margin-right: 8px;">{p}</a>
                      {/each}
                    </div>
                  </li>
                {/each}
              </ul>
            </details>
          {/if}

          {#if total === 0}
            <div style="padding: 14px; color: var(--ok);">Clean — no issues found.</div>
          {/if}
        </div>
      {/if}
    </div>

    <!-- ── Folder ACLs ── -->
    <div class="card">
      <h2 style="margin-top: 0;">Set Folder ACL</h2>
      <p class="muted" style="margin: 0 0 12px;">
        Uploads to <span class="mono">/finance/q4</span> inherit from the
        closest ancestor that has a rule (e.g. <span class="mono">/finance</span>);
        with no match, fall back to the uploader's groups.
      </p>
      <form onsubmit={(e) => { e.preventDefault(); onSubmit(); }} class="col" style="gap: 12px;">
        <div class="row" style="align-items: flex-start;">
          <span class="muted" style="width: 140px; padding-top: 6px;">Folder:</span>
          <div style="flex: 1; max-width: 420px;">
            <FolderTree
              folders={treeFolders}
              sources={treeSources}
              bind:selected={formPath}
              onSelect={(p) => (formPath = p)}
            />
            <div class="muted" style="font-size: 12px; margin-top: 6px;">
              Selected: <span class="mono">{formPath}</span>
            </div>
          </div>
        </div>
        <div class="row">
          <label class="muted" for="acl-groups-input" style="width: 140px;">Groups (comma):</label>
          <input
            id="acl-groups-input"
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
        <h2 style="margin: 0;">Current Folder ACL Rules</h2>
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

<style>
  .lint-cat {
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
  }
  .lint-cat:last-child { border-bottom: none; }
  .lint-cat > summary {
    cursor: pointer;
    user-select: none;
    font-weight: 600;
  }
  .lint-cat ul {
    margin: 4px 0 0 0;
    padding-left: 18px;
    font-size: 13px;
  }
  .lint-cat li { margin: 2px 0; }
  .badge {
    display: inline-block;
    margin-left: 6px;
    padding: 0 7px;
    font-size: 11px;
    font-weight: 500;
    background: var(--bg-2);
    color: var(--muted, #888);
    border-radius: 999px;
  }
</style>
