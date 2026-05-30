<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { getMe, getInvite, acceptInvite, type InvitePreview, type Me } from '$lib/api';

  let token = $derived($page.params.token ?? '');
  let me = $state<Me | null>(null);
  let authChecked = $state(false);
  let invite = $state<InvitePreview | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  onMount(async () => {
    try { me = await getMe(); } catch { me = null; }
    authChecked = true;
    if (me) {
      try { invite = await getInvite(token); }
      catch (e: any) { error = `This invite link is invalid or has expired.`; }
    }
  });

  async function accept() {
    busy = true; error = null;
    try {
      await acceptInvite(token);
      // Full reload so the app loads under the joined workspace.
      window.location.href = '/';
    } catch (e: any) {
      error = String(e?.message ?? e);
      busy = false;
    }
  }
</script>

<div class="invite-shell">
  <h1>Workspace invitation</h1>

  {#if !authChecked}
    <div class="muted">Loading…</div>
  {:else if !me}
    <div class="card">
      <p>Sign in to accept this invitation, then open the invite link again.</p>
      <a href="/login" class="primary-link">Sign in →</a>
    </div>
  {:else if error}
    <div class="card" style="border-color: var(--err); color: var(--err);">{error}</div>
  {:else if invite}
    {#if invite.valid}
      <div class="card">
        <p>You've been invited to join</p>
        <p class="ws-name">{invite.workspace}</p>
        <p class="muted">as <strong>{invite.role}</strong> · invite for <span class="mono">{invite.email}</span></p>
        {#if me.email && invite.email && me.email.toLowerCase() !== invite.email.toLowerCase()}
          <p class="muted" style="color: var(--warn); font-size: 12px;">
            Note: you're signed in as {me.email}, but this invite was addressed to {invite.email}.
            You can still accept it.
          </p>
        {/if}
        <button class="primary" onclick={accept} disabled={busy} style="margin-top: 12px;">
          {busy ? 'Joining…' : `Join ${invite.workspace}`}
        </button>
      </div>
    {:else}
      <div class="card muted">This invitation has already been used or has expired.</div>
    {/if}
  {/if}
</div>

<style>
  .invite-shell { max-width: 460px; margin: 64px auto 0; display: flex; flex-direction: column; gap: 16px; }
  .ws-name { font-size: 22px; font-weight: 700; margin: 4px 0; }
  .primary-link {
    display: inline-block; margin-top: 8px; padding: 8px 14px; border-radius: 6px;
    background: var(--accent); color: var(--bg); font-weight: 600;
  }
  .primary-link:hover { text-decoration: none; background: var(--accent-hover); }
</style>
