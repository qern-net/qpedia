import { spawn } from 'node:child_process'
import { Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'

/** Pack authorized, version-pinned originals. The executable and native engine share the Rust writer. */
export async function archiveFiles(
  entries,
  executable = 'qpedia-file-archive',
) {
  const child = spawn(executable, [], {
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  })
  const completion = new Promise((resolve, reject) => {
    child.once('error', reject)
    child.once('close', (code) =>
      code === 0
        ? resolve()
        : reject(new Error(detail || 'Could not build ZIP')),
    )
  })
  let detail = ''
  child.stderr.setEncoding('utf8').on('data', (text) => {
    detail = (detail + text).slice(0, 1024)
  })
  const output = []
  child.stdout.on('data', (chunk) => output.push(chunk))
  async function* records() {
    for await (const { path, bytes } of entries) {
      const name = Buffer.from(path)
      const header = Buffer.alloc(8)
      header.writeUInt32LE(name.length)
      header.writeUInt32LE(bytes.length, 4)
      yield header
      yield name
      yield bytes
    }
    yield Buffer.alloc(8)
  }
  try {
    await Promise.all([
      completion,
      pipeline(Readable.from(records()), child.stdin),
    ])
    return Buffer.concat(output)
  } finally {
    child.kill()
  }
}
