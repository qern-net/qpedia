/* Typed wrappers around the qpedia REST surface. */

export type Source = {
  id: string;
  folder_path: string;
  filename: string;
  mime: string;
  sha256: string;
  size_bytes: number;
  acl: string[];
  status: SourceStatus;
  language: string | null;
  classification?: Classification;
  created_at: string;
  ingested_at: string | null;
};

export type SourceStatus =
  | 'pending'
  | 'extracting'
  | 'extracted'
  | 'classifying'
  | 'classified'
  | 'agent_distilling'
  | 'agent_distilled'
  | 'validating'
  | 'validated'
  | 'committing'
  | 'committed'
  | 'embedding'
  | 'done'
  | 'tainted'
  | 'failed'
  | 'dead';

export type Classification = {
  doc_type?: string;
  language?: string;
  sensitivity?: string;
  hints?: string[];
};

export type SearchHit = { path: string; title: string; snippet: string };
export type SearchResp = { query: string; mode: 'hybrid' | 'filesystem'; hits: SearchHit[] };

const json = async <T>(r: Response): Promise<T> => {
  if (!r.ok) {
    let detail = '';
    try { detail = (await r.text()).slice(0, 400); } catch {}
    throw new Error(`${r.status} ${r.statusText} ${detail}`);
  }
  return r.json() as Promise<T>;
};

export async function listSources(folder: string = '/', limit = 200): Promise<Source[]> {
  const r = await fetch(`/api/v1/sources?folder=${encodeURIComponent(folder)}&limit=${limit}`);
  return json<Source[]>(r);
}

export async function getSource(id: string): Promise<Source> {
  return json<Source>(await fetch(`/api/v1/sources/${id}`));
}

export async function uploadSource(folderPath: string, file: File): Promise<Source> {
  const fd = new FormData();
  fd.append('folder_path', folderPath);
  fd.append('file', file);
  return json<Source>(await fetch('/api/v1/sources', { method: 'POST', body: fd }));
}

/** Enqueue a Remove job. Cleanup happens async; the row may linger briefly. */
export async function deleteSource(id: string): Promise<{ job_id: string }> {
  return json<{ job_id: string }>(await fetch(`/api/v1/sources/${id}`, { method: 'DELETE' }));
}

/**
 * Replace a source's file in place. Keeps the slug, folder, and ACL;
 * the pipeline re-runs from Pending so the wiki agent updates the
 * already-existing pages that reference this source_id instead of
 * creating duplicates. Identical bytes are a no-op.
 */
export async function replaceSource(id: string, file: File): Promise<Source> {
  const fd = new FormData();
  fd.append('file', file);
  return json<Source>(
    await fetch(`/api/v1/sources/${id}/replace`, { method: 'POST', body: fd })
  );
}

/** Returns the URL for downloading the original file. Use as href or window.open. */
export function sourceOriginalUrl(id: string): string {
  return `/api/v1/sources/${id}/original`;
}

/** Move a source to a different folder (drag-and-drop). */
export async function moveSource(id: string, folder_path: string): Promise<{ id: string; folder_path: string }> {
  return json(
    await fetch(`/api/v1/sources/${id}/move`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ folder_path })
    })
  );
}

// ---------- folders (File Explorer tree) ----------

export type Folder = {
  path: string;
  /** Pinned folders are locked against the AI auto-organizer. */
  pinned: boolean;
};

export async function listFolders(): Promise<{ items: Folder[] }> {
  return json(await fetch('/api/v1/folders'));
}

/** Create a folder. Manually-created folders are pinned by default. */
export async function createFolder(path: string, pinned = true): Promise<Folder> {
  return json(
    await fetch('/api/v1/folders', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ path, pinned })
    })
  );
}

/** Lock/unlock a folder against the AI auto-organizer. */
export async function setFolderPinned(path: string, pinned: boolean): Promise<Folder> {
  return json(
    await fetch('/api/v1/folders', {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ path, pinned })
    })
  );
}

/** Delete an empty folder. Throws if it still holds files. */
export async function deleteFolder(path: string): Promise<void> {
  const r = await fetch(`/api/v1/folders?path=${encodeURIComponent(path)}`, { method: 'DELETE' });
  if (!r.ok) {
    let detail = '';
    try { detail = (await r.text()).slice(0, 300); } catch {}
    throw new Error(detail || `delete folder: ${r.status}`);
  }
}

// ---------- auth ----------

export type Me = {
  id: string;
  email: string | null;
  name: string | null;
  groups: string[];
  is_admin: boolean;
};

export async function getMe(): Promise<Me | null> {
  const r = await fetch('/api/v1/auth/me');
  if (r.status === 401) return null;
  if (!r.ok) throw new Error(`me ${r.status}`);
  return r.json();
}

export type AuthConfig = { mode: 'dev' | 'firebase' | 'oidc'; firebase: boolean };

/** Public — tells the SPA which login UI to render. */
export async function getAuthConfig(): Promise<AuthConfig> {
  const r = await fetch('/api/v1/auth/config');
  if (!r.ok) throw new Error(`auth config ${r.status}`);
  return r.json();
}

// ---------- admin: folder ACLs ----------

export type FolderAcl = {
  folder_path: string;
  acl: string[];
  updated_at?: string;
  updated_by?: string;
};

export async function listFolderAcls(): Promise<{ items: FolderAcl[] }> {
  return json(await fetch('/api/v1/admin/folder-acls'));
}

export async function setFolderAcl(folder_path: string, acl: string[]): Promise<FolderAcl> {
  return json(
    await fetch('/api/v1/admin/folder-acls', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ folder_path, acl })
    })
  );
}

export async function deleteFolderAcl(folder_path: string): Promise<void> {
  const r = await fetch(`/api/v1/admin/folder-acls?folder_path=${encodeURIComponent(folder_path)}`, {
    method: 'DELETE'
  });
  if (!r.ok) throw new Error(`delete folder acl: ${r.status}`);
}

// ---------- admin: connectors ----------

export type Connector = {
  id: string;
  tenant: string;
  kind: string;
  name: string;
  cursor: string | null;
  enabled: boolean;
  last_run_at: string | null;
  last_error: string | null;
};

export async function listConnectors(): Promise<{ items: Connector[] }> {
  return json(await fetch('/api/v1/admin/connectors'));
}

export async function deleteConnector(id: string): Promise<void> {
  const r = await fetch(`/api/v1/admin/connectors/${id}`, { method: 'DELETE' });
  if (!r.ok) throw new Error(`delete connector: ${r.status}`);
}

export async function triggerConnectorSync(id: string): Promise<{ job_id: string }> {
  return json(await fetch(`/api/v1/admin/connectors/${id}/sync`, { method: 'POST' }));
}

/** Get the Google consent URL to start the Connect-Google-Drive flow.
 *  The caller then sets window.location to it; Google redirects back to
 *  the server callback, which creates the connector and bounces to
 *  /admin?google_connected=1 (or ?google_error=…). */
export async function googleDriveAuthorizeUrl(folderId?: string): Promise<string> {
  const q = folderId ? `?folder_id=${encodeURIComponent(folderId)}` : '';
  const r = await json<{ authorize_url: string }>(
    await fetch(`/api/v1/connectors/google/authorize${q}`)
  );
  return r.authorize_url;
}

export async function listStalledSources(): Promise<{ sources: Source[]; count: number }> {
  return json(await fetch('/api/v1/admin/sources/stalled'));
}

export async function resumeStalledSources(): Promise<{ enqueued: number }> {
  return json(await fetch('/api/v1/admin/sources/resume', { method: 'POST' }));
}

export async function enqueueReembed(): Promise<{ job_id: string; kind: string }> {
  return json(await fetch('/api/v1/admin/reembed', { method: 'POST' }));
}

// ---------- admin: lint ----------

export type LintBrokenLink = { page: string; target: string };
export type LintIndexDrift = {
  missing_from_index: string[];
  stale_in_index: string[];
};
export type LintDuplicate = { a: string; b: string; certainty: number };
export type LintContradiction = { tag: string; pages: string[]; summary: string };

export type LintReport = {
  generated_at: string;
  page_count: number;
  orphans: string[];
  broken_links: LintBrokenLink[];
  index_drift: LintIndexDrift;
  stale_source_ids: string[];
  duplicates: LintDuplicate[];
  contradictions: LintContradiction[];
  /** Added by the lint handler when it audits — present on stored reports. */
  tenant?: string;
};

export async function enqueueLint(): Promise<{ job_id: string; kind: string }> {
  return json(await fetch('/api/v1/admin/lint', { method: 'POST' }));
}

/** Returns the latest committed `_meta/lint.json`, or null if none yet. */
export async function getLintReport(): Promise<LintReport | null> {
  const r = await fetch('/api/v1/admin/lint');
  if (r.status === 404) return null;
  return json<LintReport>(r);
}

// ---------- admin: first-run bootstrap ----------

export type BootstrapRequest = {
  tenant_id: string;
  display_name: string;
  email_domain?: string;
  initial_folder_acls?: { folder_path: string; acl: string[] }[];
  initial_folders?: { path: string; pinned?: boolean }[];
};

export type BootstrapResult = {
  tenant: string;
  display_name: string;
  email_domain: string | null;
  folder_acls: number;
  folders: number;
  firebase_project_id: string | null;
};

/** One-shot tenant bootstrap (admin only). Idempotent: re-calling
 *  with the same tenant_id upserts. */
export async function bootstrapTenant(body: BootstrapRequest): Promise<BootstrapResult> {
  return json(
    await fetch('/api/v1/admin/bootstrap', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body)
    })
  );
}

export async function listWikiPages(prefix: string = ''): Promise<{ prefix: string; pages: string[] }> {
  return json(await fetch(`/api/v1/wiki/list?prefix=${encodeURIComponent(prefix)}`));
}

export async function getWikiPage(path: string): Promise<string> {
  const r = await fetch(`/api/v1/wiki/pages/${path}`);
  if (!r.ok) throw new Error(`get page ${path}: ${r.status}`);
  return r.text();
}

export async function searchWiki(q: string, limit = 10): Promise<SearchResp> {
  return json<SearchResp>(await fetch(`/api/v1/wiki/search?q=${encodeURIComponent(q)}&limit=${limit}`));
}

/** Terminal states that don't transition further. */
export const TERMINAL: ReadonlySet<SourceStatus> = new Set(['done', 'failed', 'dead', 'tainted'] as const);

// ---------- chat ----------

export type ChatTurn = { role: 'user' | 'assistant'; content: string };
export type Citation = { path: string; title: string };

export type ChatEvent =
  | { type: 'meta'; retrieved: Citation[]; mode: 'hybrid' | 'filesystem' }
  | { type: 'token'; text: string }
  | { type: 'done' }
  | { type: 'error'; message: string };

export type ChatRequestBody = {
  message: string;
  history?: ChatTurn[];
  max_pages?: number;
};

/** POSTs to /api/v1/chat and yields parsed SSE events. */
export async function* streamChat(req: ChatRequestBody): AsyncGenerator<ChatEvent> {
  const r = await fetch('/api/v1/chat', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(req)
  });
  if (!r.ok || !r.body) {
    let detail = '';
    try { detail = (await r.text()).slice(0, 400); } catch {}
    throw new Error(`chat ${r.status} ${detail}`);
  }
  const reader = r.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  // SSE events are separated by a blank line; lines within an event start
  // with "event:" or "data:". We accept LF or CRLF terminators.
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    while (true) {
      const lf = buffer.indexOf('\n\n');
      const cr = buffer.indexOf('\r\n\r\n');
      let idx = -1;
      let term = 0;
      if (lf >= 0 && (cr < 0 || lf < cr)) { idx = lf; term = 2; }
      else if (cr >= 0)                   { idx = cr; term = 4; }
      if (idx < 0) break;
      const block = buffer.slice(0, idx);
      buffer = buffer.slice(idx + term);
      let evName = '';
      let dataStr = '';
      for (const raw of block.split(/\r?\n/)) {
        if (raw.startsWith('event:'))      evName = raw.slice(6).trim();
        else if (raw.startsWith('data:'))  dataStr += raw.slice(5).replace(/^\s/, '');
      }
      if (evName === 'done') { yield { type: 'done' }; continue; }
      if (!dataStr) continue;
      try { yield JSON.parse(dataStr) as ChatEvent; }
      catch { /* ignore malformed event */ }
    }
  }
}
