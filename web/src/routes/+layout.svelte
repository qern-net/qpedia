<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { getMe, type Me } from '$lib/api';
  import WorkspaceSwitcher from '$lib/components/WorkspaceSwitcher.svelte';

  let { children } = $props();
  let me = $state<Me | null>(null);
  let authChecked = $state(false);

  onMount(async () => {
    try { me = await getMe(); }
    catch { me = null; }
    finally { authChecked = true; }
  });
</script>

<div class="app">
  <header>
    <span class="brand">QPEDIA</span>
    <nav>
      <a href="/"        class:active={$page.url.pathname === '/'}>Sources</a>
      <a href="/wiki"    class:active={$page.url.pathname.startsWith('/wiki')}>Wiki</a>
      <a href="/search"  class:active={$page.url.pathname.startsWith('/search')}>Search</a>
      <a href="/chat"    class:active={$page.url.pathname.startsWith('/chat')}>Chat</a>
      {#if me?.is_admin}
        <a href="/admin"   class:active={$page.url.pathname.startsWith('/admin')}>Admin</a>
      {/if}
    </nav>
    <span class="spacer"></span>
    {#if authChecked}
      {#if me}
        <WorkspaceSwitcher />
        <span class="muted mono" title={me.groups.join(', ')} style="margin-left: 12px;">
          {me.name || me.email || me.id}{me.is_admin ? ' · admin' : ''}
        </span>
        <a href="/auth/logout" class="logout-btn" title="Sign out">Log out</a>
      {:else}
        <!-- /login is the universal front door; it adapts to the backend
             auth mode (Firebase buttons, OIDC SSO, or dev notice). -->
        <a href="/login" class="logout-btn">Log in</a>
      {/if}
    {/if}
  </header>

  <!-- Workspace banner: makes the active tenant + individual/org mode
       unmistakable so you always know whose data you're looking at. -->
  {#if me}
    <div class="workspace-banner" class:individual={me.tenant_kind === 'individual'}>
      <span class="ws-icon">{me.tenant_kind === 'individual' ? '👤' : '🏢'}</span>
      <strong class="ws-label">{me.tenant_kind === 'individual' ? 'Individual workspace' : 'Organization workspace'}</strong>
      <span class="ws-desc muted">{me.tenant_kind === 'individual' ? '— your private space, isolated from other users' : '— shared with your team'}</span>
      <span class="ws-tenant mono">{me.tenant}</span>
      <span class="ws-who muted">{me.email || me.id}{me.is_admin ? ' · admin' : ''}</span>
    </div>
  {/if}

  <main>
    {@render children()}
  </main>
</div>

<style>
  .logout-btn {
    margin-left: 12px;
    padding: 4px 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-2);
    color: var(--fg);
    font-size: 13px;
  }
  .logout-btn:hover { background: var(--bg-3); text-decoration: none; }

  .workspace-banner {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 0 24px;
    /* Fixed height + nowrap so the bar never reflows to two lines when a
       page below forces a narrower width. This was the height jitter. */
    height: 38px;
    flex: none;
    white-space: nowrap;
    overflow: hidden;
    font-size: 13px;
    /* Org = brand sky; the border-left makes the mode readable at a glance. */
    background: color-mix(in srgb, var(--accent) 14%, var(--bg));
    border-bottom: 1px solid var(--border);
    border-left: 4px solid var(--accent);
  }
  .workspace-banner.individual {
    /* Individual = amber, distinct from org so the two are never confused. */
    background: color-mix(in srgb, var(--warn) 14%, var(--bg));
    border-left-color: var(--warn);
  }
  .workspace-banner .ws-icon { font-size: 15px; flex: none; }
  .workspace-banner .ws-label { flex: none; }
  /* The description is the first thing to truncate when space is tight. */
  .workspace-banner .ws-desc { overflow: hidden; text-overflow: ellipsis; min-width: 0; }
  .workspace-banner .ws-tenant {
    background: var(--bg-2);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 10px;
    font-size: 12px;
    flex: none;
  }
  .workspace-banner .ws-who { margin-left: auto; font-size: 12px; flex: none; }
</style>
