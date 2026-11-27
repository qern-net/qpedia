import type { FileAdapter, Listing, ManagedFile } from './types'

/** The same file operations can be transported over HTTP or a desktop IPC bridge. */
export interface QpediaTransport {
  list(folder: string, page: number): Promise<Listing>
  versions(id: string): Promise<{ version: number; filename: string }[]>
  upload(folder: string, file: File): Promise<boolean>
  create(path: string): Promise<unknown>
  save(id: string, version: number | undefined, name: string): Promise<void>
  zip(folder: string): Promise<void>
}
export function qpediaFiles(transport: QpediaTransport): FileAdapter<ManagedFile> {
  return {
    list: transport.list,
    versions: async f => (await transport.versions(f.id)).map(v => ({version: v.version, name: v.filename})),
    upload: async (f, folder) => await transport.upload(folder, f) ? 'unchanged' : 'uploaded',
    createFolder: async (parent, name) => { await transport.create(parent.replace(/\/$/, '') + '/' + name) },
    download: (f, version, name) => transport.save(f.id, version, name ?? f.name),
    archive: transport.zip
  }
}
export async function saveResponse(response: Response, name: string) {
  if (!response.ok) throw new Error((await response.text()).slice(0, 400) || response.statusText)
  const url = URL.createObjectURL(await response.blob())
  const a = document.createElement('a'); a.href = url; a.download = name; a.click()
  setTimeout(() => URL.revokeObjectURL(url), 60000)
}
