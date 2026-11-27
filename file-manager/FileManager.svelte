<script lang="ts" generics="F extends ManagedFile">
  import { untrack, type Snippet } from 'svelte'
  import type {
    FileAdapter,
    Folder,
    Listing,
    ManagedFile,
    Revision,
  } from './types'
  let {
    adapter,
    folder = $bindable(''),
    root = '',
    rootLabel = 'All files',
    uploadLabel = 'Upload files',
    uploadId,
    accept,
    disabled = false,
    showFolders = true,
    refreshKey = 0,
    onnavigate,
    onchanged,
    folderHref,
    ondragfile,
    actions,
    tools,
    fileInfo,
    uploadTools,
  }: {
    adapter: FileAdapter<F>
    folder?: string
    root?: string
    rootLabel?: string
    uploadLabel?: string
    uploadId?: string
    accept?: string
    disabled?: boolean
    showFolders?: boolean
    refreshKey?: unknown
    onnavigate?: (folder: string) => void
    onchanged?: () => void | Promise<void>
    folderHref?: (id: string) => string
    ondragfile?: (event: DragEvent, file: F) => void
    actions?: Snippet<[F]>
    tools?: Snippet
    fileInfo?: Snippet<[F]>
    uploadTools?: Snippet
  } = $props()
  let page = $state(0),
    refresh = $state(0),
    busy = $state(false),
    creating = $state(false)
  let folderName = $state(''),
    error = $state(''),
    notice = $state('')
  let listing = $state.raw<Listing<F> | null>(null)
  let history = $state<Record<string, Revision[]>>({}),
    open = $state(''),
    historyError = $state('')
  let moving = $state(''),
    destination = $state('')
  let generation = 0,
    lastScope = ''
  $effect(() => {
    void folder
    untrack(() => {
      page = 0
      creating = false
      notice = ''
      open = ''
      moving = ''
    })
  })
  $effect(() => {
    const source = adapter,
      location = folder,
      index = page
    void refresh
    void refreshKey
    const ticket = ++generation
    const scope = JSON.stringify([location, index])
    if (scope !== lastScope) {
      listing = null
      history = {}
      open = ''
      lastScope = scope
    }
    error = ''
    source
      .list(location, index)
      .then((result) => {
        if (ticket !== generation) return
        if (index && !result.files.length) page = index - 1
        else {
          if (
            open &&
            listing?.files.find((f) => f.id === open)?.version !==
              result.files.find((f) => f.id === open)?.version
          )
            open = ''
          listing = result
        }
      })
      .catch((e) => {
        if (ticket === generation) {
          listing = null
          error = describe(e)
        }
      })
    return () => {
      generation++
    }
  })
  function describe(e: unknown) {
    return e instanceof Error ? e.message : String(e)
  }
  function navigate(to: Folder, event?: MouseEvent) {
    if (
      event &&
      (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey)
    )
      return
    event?.preventDefault()
    folder = to.id
    onnavigate?.(to.id)
  }
  async function run(work: () => Promise<void>) {
    if (busy) return
    busy = true
    error = ''
    notice = ''
    try {
      await work()
    } catch (e) {
      error = describe(e)
    } finally {
      busy = false
    }
  }
  async function upload(input: HTMLInputElement) {
    const files = Array.from(input.files ?? [])
    input.value = ''
    if (!files.length || !adapter.upload) return
    const target = folder,
      save = adapter.upload
    await run(async () => {
      const messages: string[] = []
      for (const file of files) {
        try {
          messages.push(
            file.name +
              ' · ' +
              ((await save(file, target)) === 'unchanged'
                ? 'Skipped unchanged'
                : 'Uploaded'),
          )
        } catch (e) {
          messages.push(file.name + ' · ' + describe(e))
        }
      }
      notice = messages.join('\n')
      refresh++
      await onchanged?.()
    })
  }
  async function revisions(file: F) {
    if (open === file.id && !historyError) {
      open = ''
      return
    }
    const ticket = generation
    open = file.id
    historyError = ''
    try {
      const rows = await adapter.versions(file)
      if (generation === ticket && open === file.id) history[file.id] = rows
    } catch (e) {
      if (generation === ticket && open === file.id) historyError = describe(e)
    }
  }
</script>

<section class="file-manager" aria-label="Files" aria-busy={busy}>
  {#if showFolders}
    <nav aria-label="Folder path">
      {#if folderHref}<a
          href={folderHref(root)}
          aria-current={folder === root ? 'location' : undefined}
          onclick={(e) => navigate({ id: root, name: rootLabel }, e)}
          >{rootLabel}</a
        >
      {:else}<button onclick={() => navigate({ id: root, name: rootLabel })}
          >{rootLabel}</button
        >{/if}
      {#each listing?.ancestors ?? [] as ancestor (ancestor.id)}
        <span aria-hidden="true"> / </span>{#if folderHref}<a
            href={folderHref(ancestor.id)}
            aria-current={folder === ancestor.id ? 'location' : undefined}
            onclick={(e) => navigate(ancestor, e)}>{ancestor.name}</a
          >{:else}<button onclick={() => navigate(ancestor)}
            >{ancestor.name}</button
          >{/if}
      {/each}
    </nav>
  {/if}
  <div class="tools">
    <button
      disabled={disabled || busy || !listing}
      onclick={() => run(() => adapter.archive(folder))}>Download ZIP</button
    >
    {#if adapter.createFolder}<button
        disabled={disabled || busy}
        onclick={() => {
          creating = !creating
          folderName = ''
        }}>New folder</button
      >{/if}
    {#if adapter.removeFolder && folder !== root && listing && !listing.files.length && !listing.folders.length && !page}
      <button
        disabled={disabled || busy}
        onclick={() =>
          run(async () => {
            if (!confirm('Remove this empty folder?')) return
            const parent = listing?.ancestors.at(-2) ?? {
              id: root,
              name: rootLabel,
            }
            await adapter.removeFolder!(folder)
            navigate(parent)
            await onchanged?.()
          })}>Remove empty folder</button
      >
    {/if}
    {@render tools?.()}
  </div>
  {#if creating}
    <form
      onsubmit={(e) => {
        e.preventDefault()
        void run(async () => {
          await adapter.createFolder!(folder, folderName.trim())
          creating = false
          refresh++
          await onchanged?.()
        })
      }}
    >
      <label>Folder path <input required bind:value={folderName} /></label>
      <button disabled={busy || !folderName.trim()}>Create folder</button>
      <button type="button" onclick={() => (creating = false)}>Cancel</button>
    </form>
  {/if}
  {#if adapter.upload}
    <label
      >{uploadLabel}
      <input
        id={uploadId}
        type="file"
        multiple
        {accept}
        disabled={disabled || busy}
        onchange={(e) => void upload(e.currentTarget)}
      /></label
    >
  {/if}
  {@render uploadTools?.()}
  {#if notice}<p role="status" class="notice">{notice}</p>{/if}
  {#if error}<p role="alert">
      {error} <button onclick={() => refresh++}>Retry listing</button>
    </p>{/if}
  {#if listing}
    {#if showFolders && listing.folders.length}
      <ul aria-label="Folders">
        {#each listing.folders as child (child.id)}<li>
            {#if folderHref}<a
                href={folderHref(child.id)}
                onclick={(e) => navigate(child, e)}>{child.name}</a
              >{:else}<button onclick={() => navigate(child)}
                >{child.name}</button
              >{/if}
          </li>{/each}
      </ul>
    {/if}
    {#if listing.files.length}
      <ul aria-label="Documents">
        {#each listing.files as file (file.id)}
          <li
            class="file"
            draggable={!!ondragfile}
            ondragstart={(event) => ondragfile?.(event, file)}
          >
            <div>
              <strong>{file.name}</strong>
              {#if fileInfo}{@render fileInfo(file)}{:else}<small
                  >{Math.ceil(file.size / 1024)} KiB {file.status ??
                    ''}{file.version ? ' · v' + file.version : ''}</small
                >{/if}
            </div>
            <div class="tools">
              {#if file.downloadable !== false}<button
                  disabled={busy}
                  onclick={() => run(() => adapter.download(file))}
                  >Download</button
                >{/if}
              <button
                aria-expanded={open === file.id}
                onclick={() => revisions(file)}>Versions</button
              >
              {#if adapter.move}<button
                  disabled={disabled || busy}
                  onclick={() => {
                    moving = file.id
                    destination = folder
                  }}>Move file</button
                >{/if}
              {@render actions?.(file)}
            </div>
            {#if moving === file.id}
              <form
                onsubmit={(e) => {
                  e.preventDefault()
                  void run(async () => {
                    await adapter.move!(file, destination)
                    moving = ''
                    refresh++
                    await onchanged?.()
                  })
                }}
              >
                <label
                  >Destination folder <input
                    required
                    bind:value={destination}
                  /></label
                >
                <button
                  disabled={busy ||
                    !destination.trim() ||
                    destination === folder}>Move</button
                >
                <button type="button" onclick={() => (moving = '')}
                  >Cancel</button
                >
              </form>
            {/if}
            {#if open === file.id}
              {#if historyError}<p role="alert">
                  {historyError}
                  <button onclick={() => revisions(file)}
                    >Retry loading versions</button
                  >
                </p>
              {:else if !history[file.id]}<p role="status">Loading versions…</p>
              {:else}<ul aria-label="Versions">
                  {#each history[file.id] as v (v.version)}<li>
                      {#if v.downloadable !== false}<button
                          onclick={() =>
                            run(() =>
                              adapter.download(file, v.version, v.name),
                            )}>Download v{v.version} · {v.name}</button
                        >
                      {:else}v{v.version} · {v.status}{/if}
                    </li>{/each}
                </ul>{/if}
            {/if}
          </li>
        {/each}
      </ul>
    {:else if !listing.folders.length}<p>This folder is empty.</p>{/if}
    {#if page || listing.hasMore}<nav aria-label="File pages">
        <button disabled={!page} onclick={() => page--}>Previous files</button>
        Page {page + 1}
        <button disabled={!listing.hasMore} onclick={() => page++}
          >Next files</button
        >
      </nav>{/if}
  {:else if !error}<p>Loading files…</p>{/if}
</section>

<style>
  .file-manager {
    display: grid;
    gap: 0.8rem;
    min-width: 0;
  }
  .tools,
  nav,
  form {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    flex-wrap: wrap;
  }
  ul {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .file {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 0.6rem;
    flex-wrap: wrap;
    border-bottom: 1px solid currentColor;
    padding: 0.65rem 0;
  }
  .file > ul {
    width: 100%;
  }
  small {
    display: block;
    opacity: 0.8;
  }
  button {
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 0.2em;
  }
  button:disabled {
    cursor: default;
    opacity: 0.5;
  }
  .notice {
    white-space: pre-line;
  }
</style>
