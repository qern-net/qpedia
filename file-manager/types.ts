export interface ManagedFile {
  id: string
  name: string
  size: number
  version?: number
  status?: string
  downloadable?: boolean
}
export interface Revision {
  version: number
  name: string
  downloadable?: boolean
  status?: string
}
export interface Folder {
  id: string
  name: string
}
export interface Listing<F extends ManagedFile = ManagedFile> {
  files: F[]
  folders: Folder[]
  ancestors: Folder[]
  hasMore: boolean
}
export interface FileAdapter<F extends ManagedFile = ManagedFile> {
  list(folder: string, page: number): Promise<Listing<F>>
  versions(file: F): Promise<Revision[]>
  download(file: F, version?: number, name?: string): Promise<void>
  archive(folder: string): Promise<void>
  upload?(file: File, folder: string): Promise<'uploaded' | 'unchanged'>
  createFolder?(parent: string, name: string): Promise<void>
}
