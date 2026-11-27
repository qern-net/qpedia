export function archiveFiles(
  entries: AsyncIterable<{ path: string; bytes: Uint8Array }>,
  executable?: string,
): Promise<Buffer>
