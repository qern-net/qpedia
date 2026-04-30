<script lang="ts">
  import { tick } from 'svelte';
  import { marked } from 'marked';
  import { streamChat, type Citation, type ChatTurn } from '$lib/api';

  type Msg = {
    role: 'user' | 'assistant';
    content: string;
    citations?: Citation[];
    mode?: string;
    error?: boolean;
  };

  let history = $state<Msg[]>([]);
  let input = $state('');
  let busy = $state(false);
  let scroller: HTMLDivElement | undefined = $state();

  async function scrollToEnd() {
    await tick();
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  }

  function renderMarkdown(s: string): string {
    if (!s) return '';
    // Same wikilink rewrite as the wiki page viewer.
    const linked = s.replace(/\[\[([^\]]+)\]\]/g, (_m, t) => `[${t}](/wiki/${t.trim()})`);
    return marked.parse(linked, { async: false }) as string;
  }

  async function send() {
    const q = input.trim();
    if (!q || busy) return;
    input = '';
    busy = true;

    // Snapshot prior turns BEFORE we append the new user/assistant pair.
    const turnsBefore: ChatTurn[] = history.map((m) => ({ role: m.role, content: m.content }));

    history = [
      ...history,
      { role: 'user', content: q },
      { role: 'assistant', content: '' }
    ];
    const idx = history.length - 1;
    await scrollToEnd();

    try {
      for await (const ev of streamChat({ message: q, history: turnsBefore, max_pages: 5 })) {
        const next = history.slice();
        const cur = { ...next[idx] } as Msg;
        if (ev.type === 'meta') {
          cur.citations = ev.retrieved;
          cur.mode = ev.mode;
        } else if (ev.type === 'token') {
          cur.content += ev.text;
        } else if (ev.type === 'error') {
          cur.content = `error: ${ev.message}`;
          cur.error = true;
        } else if (ev.type === 'done') {
          // nothing extra
        }
        next[idx] = cur;
        history = next;
        if (ev.type === 'token') await scrollToEnd();
        if (ev.type === 'done' || ev.type === 'error') break;
      }
    } catch (e: any) {
      const next = history.slice();
      next[idx] = { ...next[idx], content: `error: ${String(e?.message ?? e)}`, error: true };
      history = next;
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
</script>

<div class="chat-shell">
  <h1>Chat</h1>

  <div class="chat-scroll" bind:this={scroller}>
    {#if history.length === 0}
      <div class="muted card">
        Ask a question grounded in your wiki. Retrieved pages appear as citations
        beside the answer; the model is told to cite them inline as
        <code>[[path/to/page.md]]</code> when it draws on them.
      </div>
    {/if}

    {#each history as m, i (i)}
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
                <a href={`/wiki/${c.path}`} class="cite">{c.title || c.path}</a>
              {/each}
            </div>
          {/if}
          <div class="msg-body markdown">
            {#if m.content}
              {@html renderMarkdown(m.content)}
            {:else if busy}
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
