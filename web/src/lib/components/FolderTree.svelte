<script lang="ts">
  import type { Folder, Source } from '$lib/api';

  type Props = {
    folders: Folder[];
    sources: Source[];
    selected: string;
    /** Show + / lock / trash controls (Sources tab). Admin uses read-only select. */
    manage?: boolean;
    /** Accept dropped file rows as move targets (Sources tab). */
    droppable?: boolean;
    onSelect: (path: string) => void;
    onCreateFolder?: (path: string) => void;
    onDeleteFolder?: (path: string) => void;
    onTogglePin?: (path: string, pinned: boolean) => void;
    onMoveSource?: (id: string, folderPath: string) => void;
  };

  let {
    folders,
    sources,
    selected = $bindable('/'),
    manage = false,
    droppable = false,
    onSelect,
    onCreateFolder,
    onDeleteFolder,
    onTogglePin,
    onMoveSource
  }: Props = $props();

  type TreeNode = {
    path: string;
    name: string;
    pinned: boolean;
    children: TreeNode[];
    fileCount: number;   // files directly in this folder
    total: number;       // files in this folder + all descendants
    done: number;        // …of those, finished ingesting (status 'done')
  };

  const tree = $derived(buildTree(folders, sources));

  // Expanded state — default everything open (trees are small).
  let collapsed = $state<Set<string>>(new Set());

  function buildTree(folders: Folder[], sources: Source[]): TreeNode {
    const root: TreeNode = { path: '/', name: 'All files', pinned: false, children: [], fileCount: 0, total: 0, done: 0 };
    const byPath = new Map<string, TreeNode>([['/', root]]);
    const pinnedOf = new Map<string, boolean>();
    for (const f of folders) pinnedOf.set(f.path, f.pinned);

    const ensure = (path: string): TreeNode => {
      if (!path || path === '/') return root;
      const hit = byPath.get(path);
      if (hit) return hit;
      const idx = path.lastIndexOf('/');
      const parent = ensure(idx <= 0 ? '/' : path.slice(0, idx));
      const node: TreeNode = {
        path,
        name: path.slice(idx + 1),
        pinned: pinnedOf.get(path) ?? false,
        children: [],
        fileCount: 0,
        total: 0,
        done: 0
      };
      byPath.set(path, node);
      parent.children.push(node);
      return node;
    };

    for (const f of folders) ensure(f.path);
    for (const s of sources) ensure(s.folder_path || '/');
    // Direct counts.
    for (const s of sources) {
      const node = byPath.get(s.folder_path || '/') ?? root;
      node.fileCount++;
      node.total++;
      if (s.status === 'done') node.done++;
    }
    // Roll total/done up into ancestors (post-order).
    const rollup = (n: TreeNode) => {
      for (const c of n.children) {
        rollup(c);
        n.total += c.total;
        n.done += c.done;
      }
    };
    rollup(root);

    const sortRec = (n: TreeNode) => {
      n.children.sort((a, b) => a.name.localeCompare(b.name));
      n.children.forEach(sortRec);
    };
    sortRec(root);
    return root;
  }

  function toggleCollapse(path: string) {
    const next = new Set(collapsed);
    next.has(path) ? next.delete(path) : next.add(path);
    collapsed = next;
  }

  function addChild(n: TreeNode) {
    const name = prompt(`New folder under ${n.path}:`);
    if (!name || !name.trim()) return;
    const childPath = n.path === '/' ? `/${name.trim()}` : `${n.path}/${name.trim()}`;
    onCreateFolder?.(childPath);
  }

  // Drag highlight is applied via direct DOM class toggling rather than
  // reactive state: mutating $state on every dragover re-renders the tree
  // and replaces the hovered node's DOM mid-drag, which cancels the drop.
  function onDragOver(e: DragEvent) {
    if (!droppable) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
    (e.currentTarget as HTMLElement)?.classList.add('drop');
  }
  function onDragLeave(e: DragEvent) {
    (e.currentTarget as HTMLElement)?.classList.remove('drop');
  }
  function onDrop(e: DragEvent, n: TreeNode) {
    e.preventDefault();
    (e.currentTarget as HTMLElement)?.classList.remove('drop');
    const id =
      e.dataTransfer?.getData('text/source-id') ||
      e.dataTransfer?.getData('text/plain');
    if (id) onMoveSource?.(id, n.path);
  }
</script>

{#snippet treeNode(n: TreeNode, depth: number)}
  <div
    class="node"
    class:selected={selected === n.path}
    style="padding-left: {depth * 16 + 8}px;"
    role="treeitem"
    aria-selected={selected === n.path}
    tabindex="0"
    onclick={() => onSelect(n.path)}
    onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect(n.path); } }}
    ondragover={droppable ? onDragOver : undefined}
    ondragleave={droppable ? onDragLeave : undefined}
    ondrop={droppable ? (e) => onDrop(e, n) : undefined}
  >
    {#if n.children.length > 0}
      <button
        class="twisty"
        title={collapsed.has(n.path) ? 'Expand' : 'Collapse'}
        onclick={(e) => { e.stopPropagation(); toggleCollapse(n.path); }}
      >{collapsed.has(n.path) ? '▸' : '▾'}</button>
    {:else}
      <span class="twisty placeholder"></span>
    {/if}

    <span class="icon">{n.path === '/' ? '🗂' : '📁'}</span>
    <span class="label">{n.name}</span>
    {#if n.pinned}
      <span class="pin" title="Locked — the AI auto-organizer won't move files in/out or delete this folder">🔒</span>
    {/if}
    {#if n.total > 0}
      {#if n.done < n.total}
        <span class="progress" title="{n.done} of {n.total} files ingested">
          <span class="progress-bar"><span class="progress-fill" style="width: {Math.round((100 * n.done) / n.total)}%"></span></span>
          <span class="progress-txt">{n.done}/{n.total}</span>
        </span>
      {:else}
        <span class="count" title="{n.total} files · all ingested">{n.total} ✓</span>
      {/if}
    {/if}

    <span class="grow"></span>

    {#if manage}
      <span class="controls">
        <button class="ctl" title="New subfolder" onclick={(e) => { e.stopPropagation(); addChild(n); }}>＋</button>
        {#if n.path !== '/'}
          <button
            class="ctl"
            title={n.pinned ? 'Unlock (allow AI organization)' : 'Lock against AI organization'}
            onclick={(e) => { e.stopPropagation(); onTogglePin?.(n.path, !n.pinned); }}
          >{n.pinned ? '🔓' : '🔒'}</button>
          {#if n.fileCount === 0 && n.children.length === 0}
            <button class="ctl danger" title="Delete empty folder" onclick={(e) => { e.stopPropagation(); onDeleteFolder?.(n.path); }}>🗑</button>
          {/if}
        {/if}
      </span>
    {/if}
  </div>

  {#if !collapsed.has(n.path)}
    {#each n.children as c (c.path)}
      {@render treeNode(c, depth + 1)}
    {/each}
  {/if}
{/snippet}

<div class="tree card" role="tree" style="padding: 6px;">
  {@render treeNode(tree, 0)}
</div>

<style>
  .tree { font-size: 13px; user-select: none; }
  .node {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 5px 8px;
    border-radius: 6px;
    cursor: pointer;
    white-space: nowrap;
  }
  .node:hover { background: var(--bg-2); }
  .node.selected { background: var(--bg-3, var(--bg-2)); outline: 1px solid var(--accent); }
  /* `.drop` is toggled via JS (classList), so keep it :global so Svelte
     doesn't prune it as an "unused" scoped selector. */
  .node:global(.drop) { outline: 2px dashed var(--accent); background: var(--bg-2); }
  .twisty {
    width: 16px; min-width: 16px; padding: 0; border: none; background: none;
    color: var(--muted, #888); cursor: pointer; font-size: 11px; text-align: center;
  }
  .twisty.placeholder { cursor: default; }
  .icon { width: 18px; text-align: center; }
  .label { overflow: hidden; text-overflow: ellipsis; }
  .pin { font-size: 11px; }
  .count {
    font-size: 11px; color: var(--muted, #888);
    background: var(--bg-2); border-radius: 10px; padding: 0 7px;
  }
  /* Live ingest progress for a folder (subtree). */
  .progress { display: inline-flex; align-items: center; gap: 6px; }
  .progress-bar {
    width: 56px; height: 5px; border-radius: 3px;
    background: var(--bg-3, var(--border)); overflow: hidden;
  }
  .progress-fill {
    display: block; height: 100%; background: var(--accent);
    transition: width 0.4s ease;
  }
  .progress-txt { font-size: 11px; color: var(--fg-dim); font-variant-numeric: tabular-nums; }
  .grow { flex: 1; }
  .controls { display: none; gap: 2px; }
  .node:hover .controls { display: inline-flex; }
  .ctl {
    border: none; background: none; cursor: pointer; font-size: 12px;
    padding: 2px 5px; border-radius: 4px; color: var(--fg);
  }
  .ctl:hover { background: var(--bg-3, var(--bg-2)); }
  .ctl.danger:hover { background: var(--err); color: #fff; }
</style>
