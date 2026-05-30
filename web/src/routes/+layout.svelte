<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { getMe, type Me } from '$lib/api';

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
        <span class="muted mono" title={me.groups.join(', ')}>
          {me.name || me.email || me.id}{me.is_admin ? ' · admin' : ''}
        </span>
        <a href="/auth/logout" style="margin-left: 12px;">logout</a>
      {:else}
        <!-- /login is the universal front door; it adapts to the backend
             auth mode (Firebase buttons, OIDC SSO, or dev notice). -->
        <a href="/login">login</a>
      {/if}
    {/if}
  </header>
  <main>
    {@render children()}
  </main>
</div>
