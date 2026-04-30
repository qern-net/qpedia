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
export const TERMINAL: ReadonlySet<SourceStatus> = new Set(['done', 'failed', 'dead'] as const);
