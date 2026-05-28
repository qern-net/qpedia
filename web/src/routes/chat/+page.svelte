<script lang="ts">
  import { tick } from 'svelte';
  import { marked } from 'marked';
  import { streamChat, type ChatTurn } from '$lib/api';
  import { chatHistory, type ChatMsg } from '$lib/stores.svelte';

  let input = $state('');
  let busy = $state(false);
  let scroller: HTMLDivElement | undefined = $state();

  async function scrollToEnd() {
    await tick();
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  }

  function renderMarkdown(s: string): string {
    if (!s) return '';
    // Rewrite [[wikilinks]] to /wiki/<path> hrefs so they're clickable.
    const linked = s.replace(/\[\[([^\]]+)\]\]/g, (_m, t) => `[${t}](/wiki/${t.trim()})`);
    return marked.parse(linked, { async: false }) as string;
  }

  async function send() {
    const q = input.trim();
    if (!q || busy) return;
    input = '';
    busy = true;

    // Snapshot prior turns for the API (text only, no citations/metadata).
    const turnsBefore: ChatTurn[] = chatHistory.msgs.map((m) => ({
      role: m.role,
      content: m.content
    }));

    chatHistory.push({ role: 'user', content: q });
    const idx = chatHistory.push({ role: 'assistant', content: '' });
    await scrollToEnd();

    try {
      for await (const ev of streamChat({ message: q, history: turnsBefore, max_pages: 5 })) {
        if (ev.type === 'meta') {
          chatHistory.update(idx, { citations: ev.retrieved, mode: ev.mode });
        } else if (ev.type === 'token') {
          const cur = chatHistory.msgs[idx];
          chatHistory.update(idx, { content: cur.content + ev.text });
          await scrollToEnd();
        } else if (ev.type === 'error') {
          chatHistory.update(idx, { content: `error: ${ev.message}`, error: true });
        }
        if (ev.type === 'done' || ev.type === 'error') break;
      }
    } catch (e: any) {
      chatHistory.update(idx, {
        content: `error: ${String(e?.message ?? e)}`,
        error: true
      });
    } finally {
      busy = false;
      await scrollToEnd();
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  function onClear() {
    if (confirm('Clear chat history?')) chatHistory.clear();
  }
</script>

<div class="chat-shell">
  <div class="row" style="margin-bottom: 0;">
    <h1 style="margin: 0;">Chat</h1>
    <span class="spacer"></span>
    {#if chatHistory.msgs.length > 0}
      <button onclick={onClear} style="font-size: 12px; padding: 4px 10px;">Clear history</button>
    {/if}
  </div>

  <div class="chat-scroll" bind:this={scroller}>
    {#if chatHistory.msgs.length === 0}
      <div class="muted card">
        Ask a question grounded in your wiki. Retrieved pages appear as citations
        beside the answer; the model cites them inline as
        <code>[[path/to/page.md]]</code> — click any link to open the page.
      </div>
    {/if}

    {#each chatHistory.msgs as m, i (i)}
      <div class="msg msg-{m.role}" class:msg-error={m.error}>
        <div class="msg-role">{m.role}</div>

        {#if m.role === 'user'}
          <div class="msg-body user-body">{m.content}</div>
        {:else}
          {#if m.mode}
            <div class="msg-meta">
              retrieved {m.citations?.length ?? 0} page{(m.citations?.length ?? 0) === 1 ? '' : 's'} via <strong>{m.mode}</strong>
            </div>
          {/if}
          {#if m.citations && m.citations.length > 0}
            <div class="msg-citations">
              {#each m.citations as c}
                <a href={`/wiki/${c.path}`} class="cite" title={c.path}>{c.title || c.path}</a>
              {/each}
            </div>
          {/if}
          <div class="msg-body markdown">
            {#if m.content}
              {@html renderMarkdown(m.content)}
            {:else if busy && i === chatHistory.msgs.length - 1}
              <span class="muted">…</span>
            {/if}
          </div>
        {/if}
      </div>
    {/each}
  </div>

  <form class="chat-input" onsubmit={(e) => { e.preventDefault(); send(); }}>
    <textarea
      bind:value={input}
      onkeydown={onKey}
      placeholder="Ask about your wiki…"
      rows="2"
      disabled={busy}
    ></textarea>
    <button class="primary" type="submit" disabled={busy || !input.trim()}>
      {busy ? '…' : 'Send'}
    </button>
  </form>
</div>

<style>
  .chat-shell {
    display: grid;
    grid-template-rows: auto 1fr auto;
    gap: 12px;
    height: calc(100vh - 110px);
  }
  .chat-scroll {
    overflow-y: auto;
    padding-right: 8px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .msg-role {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--fg-dim);
    margin-bottom: 4px;
  }
  .msg-meta {
    font-size: 12px;
    color: var(--fg-dim);
    margin-bottom: 6px;
  }
  .msg-citations {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 10px;
  }
  .cite {
    background: var(--bg-2);
    border: 1px solid var(--border);
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 12px;
  }
  .cite:hover {
    background: var(--bg-3);
    text-decoration: none;
  }
  .user-body {
    background: var(--bg-3);
    border-radius: 6px;
    padding: 8px 12px;
    white-space: pre-wrap;
    max-width: 70ch;
  }
  .msg-user { align-self: flex-end; }
  .msg-assistant .msg-body { max-width: 80ch; }
  .msg-error .msg-body { color: var(--err); }
  .chat-input {
    display: flex;
    gap: 8px;
    align-items: flex-end;
  }
  .chat-input textarea {
    flex: 1;
    background: var(--bg-2);
    color: var(--fg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
    font: inherit;
    resize: vertical;
    min-height: 44px;
  }
</style>
