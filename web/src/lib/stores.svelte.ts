/**
 * Persistent client-side stores backed by localStorage.
 * Chat history survives page reloads and tab switches.
 */

import type { Citation } from '$lib/api';

export type ChatMsg = {
  role: 'user' | 'assistant';
  content: string;
  citations?: Citation[];
  mode?: string;
  error?: boolean;
};

const CHAT_KEY = 'qpedia:chat:history';
const MAX_STORED = 100; // cap stored messages to avoid unbounded growth

function loadHistory(): ChatMsg[] {
  if (typeof localStorage === 'undefined') return [];
  try {
    const raw = localStorage.getItem(CHAT_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function saveHistory(msgs: ChatMsg[]): void {
  if (typeof localStorage === 'undefined') return;
  try {
    const capped = msgs.slice(-MAX_STORED);
    localStorage.setItem(CHAT_KEY, JSON.stringify(capped));
  } catch {
    // Storage quota exceeded or private browsing — fail silently.
  }
}

export function clearHistory(): void {
  if (typeof localStorage !== 'undefined') {
    localStorage.removeItem(CHAT_KEY);
  }
}

/**
 * A simple reactive chat history that persists to localStorage.
 * Usage:
 *   import { chatHistory } from '$lib/stores';
 *   chatHistory.push({ role: 'user', content: '...' });
 *   chatHistory.update(idx, patch);
 *   chatHistory.clear();
 */
class ChatHistoryStore {
  #msgs = $state<ChatMsg[]>(loadHistory());

  get msgs(): ChatMsg[] {
    return this.#msgs;
  }

  push(msg: ChatMsg): number {
    this.#msgs = [...this.#msgs, msg];
    saveHistory(this.#msgs);
    return this.#msgs.length - 1;
  }

  update(idx: number, patch: Partial<ChatMsg>): void {
    const next = this.#msgs.slice();
    next[idx] = { ...next[idx], ...patch };
    this.#msgs = next;
    saveHistory(this.#msgs);
  }

  clear(): void {
    this.#msgs = [];
    clearHistory();
  }
}

export const chatHistory = new ChatHistoryStore();
