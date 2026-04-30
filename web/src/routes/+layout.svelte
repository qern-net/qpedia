<script lang="ts">
  import '../app.css';
  import { page } from '$app/stores';

  let { children } = $props();
</script>

<div class="app">
  <header>
    <span class="brand">QPEDIA</span>
    <nav>
      <a href="/"        class:active={$page.url.pathname === '/'}>Sources</a>
      <a href="/wiki"    class:active={$page.url.pathname.startsWith('/wiki')}>Wiki</a>
      <a href="/search"  class:active={$page.url.pathname.startsWith('/search')}>Search</a>
    </nav>
    <span class="spacer"></span>
    <span class="muted mono">karpathy llmwiki</span>
  </header>
  <main>
    {@render children()}
  </main>
</div>
