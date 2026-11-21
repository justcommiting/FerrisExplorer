import { useEffect, useMemo, useRef, useState } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { AlertTriangle, ArrowDown, ArrowUp, Filter, Folder, FolderOpen, Loader2, SearchX } from 'lucide-react'

import { ColumnView } from '@/components/ColumnView'
import { FileIcon } from '@/components/FileIcon'
import { RefinePanel, SearchHeader } from '@/components/SearchFilters'
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuShortcut,
  ContextMenuTrigger,
} from '@/components/ui/context-menu'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { commandById, pluginCommands, runCommand } from '@/commands'
import type { VNode } from '@/lib/api'
import { matchesQuickFilter } from '@/lib/quickFilter'
import { volumeLabel } from '@/lib/listing'
import { formatSize, formatTimestamp, kindLabel, parentOf } from '@/lib/format'
import { beginRowDrag } from '@/lib/rowDrag'
import { typeaheadStep, type TypeaheadState } from '@/lib/typeahead'
import { vimStep, type VimAction } from '@/lib/vim'
import { cn } from '@/lib/utils'
import { displayItems, effectiveView, useExplorer, type Pane, type SortColumn, type Tab } from '@/store/explorer'

const ROW = 30

/** Stands in for the size map when sizes do not affect the order, so the sort memo ignores them. */
const NO_SIZES: Record<string, never> = {}
/** How long a listing may take before the window says it is waiting on the disk. */
const SLOW_LISTING_MS = 1000

/** The last navigation-free change (trash or paste) that `.` repeats in vim mode. */
let lastVimRepeat: 'delete' | 'paste' | null = null

const GIT_STYLE: Record<string, string> = {
  M: 'text-git-modified',
  '+': 'text-git-added',
  U: 'text-git-untracked',
  D: 'text-git-deleted',
  I: 'text-muted-foreground/60',
}

function HeaderCell({
  tab,
  pane,
  col,
  label,
  className,
  sortable,
}: {
  tab: Tab
  pane: Pane
  col: SortColumn
  label: string
  className?: string
  sortable: boolean
}) {
  const sort = tab.sort
  const active = sortable && sort.col === col
  return (
    <button
      disabled={!sortable}
      onClick={() => {
        const st = useExplorer.getState()
        st.focusPane(pane)
        st.setSort(col)
      }}
      className={cn(
        'flex h-full items-center gap-1 px-2 text-left text-[11.5px] tracking-[.04em] text-muted-foreground hover:text-foreground disabled:hover:text-muted-foreground',
        active && 'text-foreground',
        className,
      )}
    >
      {label}
      {active && (sort.dir === 'asc' ? <ArrowUp className="size-3" /> : <ArrowDown className="size-3" />)}
    </button>
  )
}

function EmptyState({ icon, title, detail }: { icon: React.ReactNode; title: string; detail?: string }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 text-center text-muted-foreground">
      <div className="text-muted-foreground/50">{icon}</div>
      <div className="text-sm text-foreground/80">{title}</div>
      {detail && <div className="max-w-sm text-xs">{detail}</div>}
    </div>
  )
}

const MENU: (string | '-')[] = [
  'open', 'editor', 'reveal', '-', 'copy', 'cut', 'paste', '-', 'copy-path', 'copy-name', '-',
  'rename', 'bulk-rename', 'duplicate', 'move-to', 'compress', 'extract-here', 'extract-to', 'dir-size', 'git-diff', 'git-stage', 'git-unstage', 'git-discard', '-', 'new-folder', 'terminal', '-', 'delete',
]

function RowMenu() {
  const pluginReport = useExplorer((s) => s.pluginReport)
  // Plugin actions whose `when` fits the selection, after the built-in commands.
  const plugins = pluginCommands(pluginReport).filter((c) => !c.enabled || c.enabled())
  return (
    <ContextMenuContent className="w-60">
      {MENU.map((id, i) => {
        if (id === '-') return <ContextMenuSeparator key={`sep-${i}`} />
        const cmd = commandById[id]
        const enabled = !cmd.enabled || cmd.enabled()
        if (!enabled && (id.startsWith('git-') || id === 'paste' || id.startsWith('extract-'))) return null
        return (
          <ContextMenuItem
            key={id}
            disabled={!enabled}
            variant={id === 'delete' ? 'destructive' : 'default'}
            onClick={() => runCommand(id)}
            className="text-[12.5px]"
          >
            {cmd.label}
            {cmd.shortcut && <ContextMenuShortcut className="text-[10px]">{cmd.shortcut}</ContextMenuShortcut>}
          </ContextMenuItem>
        )
      })}
      {plugins.length > 0 && <ContextMenuSeparator />}
      {plugins.map((c) => (
        <ContextMenuItem key={c.id} onClick={() => void c.run()} className="text-[12.5px]">
          {c.label}
        </ContextMenuItem>
      ))}
    </ContextMenuContent>
  )
}

export function FileList({ tabId, pane }: { tabId: number; pane: Pane }) {
  const tab = useExplorer((s) => s.tabs.find((t) => t.id === tabId) ?? s.activeTab())
  const focused = useExplorer((s) => s.focusedTabId() === tab.id)
  const isSplit = useExplorer((s) => s.splitTabId !== null)
  const dropHover = useExplorer((s) => s.dropHover)
  const dropPane = dropHover?.tabId === tab.id && dropHover.dir === null
  const focusPane = useExplorer((s) => s.focusPane)
  const showHidden = useExplorer((s) => s.showHidden)
  const dirSizes = useExplorer((s) => s.dirSizes)
  const clipboard = useExplorer((s) => s.clipboard)
  const focusRequest = useExplorer((s) => s.focusRequest)
  const setSelection = useExplorer((s) => s.setSelection)
  const openNode = useExplorer((s) => s.openNode)
  const vimMode = useExplorer((s) => s.vimMode)
  const hideIgnored = useExplorer((s) => s.hideIgnored)
  const ignoredSet = useMemo(() => new Set(tab.ignored), [tab.ignored])

  // Only the fields displayItems reads are listed, so a selection or cursor
  // change does not re-sort the folder. Sizes matter only when sorting by size.
  const sizesInOrder = tab.sort.col === 'size' ? dirSizes : NO_SIZES
  const allItems = useMemo(
    () => displayItems(tab, showHidden, sizesInOrder, hideIgnored),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [tab.items, tab.results, tab.activeQuery, tab.ignored, tab.git?.marks, tab.sort, tab.listing, showHidden, sizesInOrder, hideIgnored],
  )
  // The quick filter narrows the folder by name while its box is open.
  const items = useMemo(
    () => (tab.filter === null ? allItems : allItems.filter((n) => matchesQuickFilter(n.name, tab.filter ?? ''))),
    [allItems, tab.filter],
  )
  const index = useMemo(() => new Map(items.map((n, i) => [n.path, i])), [items])
  const names = useMemo(() => items.map((n) => n.name), [items])
  const selected = useMemo(() => new Set(tab.selected), [tab.selected])
  const cutPaths = useMemo(() => new Set(clipboard?.cut ? clipboard.paths : []), [clipboard])
  const searching = tab.activeQuery !== null
  // Columns view replaces the list body; smart folders and searches always show the list.
  const columns = effectiveView(tab) === 'columns'

  const scrollRef = useRef<HTMLDivElement>(null)
  const filterInput = useRef<HTMLInputElement>(null)
  const typeahead = useRef<TypeaheadState>({ text: '', at: 0 })
  /** A half-typed vim chord such as `g` before `gg`. */
  const vimPending = useRef('')
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW,
    overscan: 12,
  })
  const virtualRows = virtualizer.getVirtualItems()
  const firstRow = virtualRows[0]?.index ?? -1
  const lastRow = virtualRows[virtualRows.length - 1]?.index ?? -1

  // Size only the folders on screen. Sizing every subfolder is deferred to
  // sorting by size, where the whole list has to be ordered by it.
  useEffect(() => {
    if (columns || searching || tab.archive) return
    const wanted: string[] = []
    for (let i = Math.max(firstRow, 0); i <= lastRow; i++) {
      const n = items[i]
      if (n?.is_dir && dirSizes[n.path] === undefined) wanted.push(n.path)
    }
    if (wanted.length) useExplorer.getState().calculateDirSizes(wanted)
  }, [items, firstRow, lastRow, columns, searching, tab.archive, dirSizes])

  // Say so when a listing is taking a while, as on a slow or network mount.
  const [slow, setSlow] = useState(false)
  useEffect(() => {
    if (!tab.loading) return
    const timer = window.setTimeout(() => setSlow(true), SLOW_LISTING_MS)
    return () => {
      window.clearTimeout(timer)
      setSlow(false)
    }
  }, [tab.loading, tab.dir])

  useEffect(() => {
    if (focusRequest.target === 'list' && focused) scrollRef.current?.focus()
    if (focusRequest.target === 'filter' && focused) filterInput.current?.focus()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest])

  // Keep the cursor row in view when the cursor itself moves. This must not
  // rerun when the rows merely change (a folder size arriving re-sorts the
  // list), or scrolling away from the selection snaps back to it.
  const indexRef = useRef(index)
  indexRef.current = index
  useEffect(() => {
    const i = tab.cursor ? indexRef.current.get(tab.cursor) : undefined
    if (i !== undefined) virtualizer.scrollToIndex(i, { align: 'auto' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.cursor])

  // Scroll to top when the folder or search changes.
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: 0 })
  }, [tab.dir, tab.activeQuery, tab.id])

  const clickRow = (e: React.MouseEvent, node: VNode) => {
    const path = node.path
    if (e.shiftKey && tab.anchor && index.has(tab.anchor)) {
      const a = index.get(tab.anchor)!
      const b = index.get(path)!
      const [lo, hi] = a < b ? [a, b] : [b, a]
      setSelection(items.slice(lo, hi + 1).map((n) => n.path), tab.anchor, path)
    } else if (e.ctrlKey || e.metaKey) {
      const next = selected.has(path) ? tab.selected.filter((p) => p !== path) : [...tab.selected, path]
      setSelection(next, path, path)
    } else {
      setSelection([path], path, path)
    }
  }

  /** Vim-style actions, run on the cursor row (`cur` is -1 when there is none). */
  const runVim = (action: VimAction, cur: number) => {
    const last = items.length - 1
    const at = (i: number) => {
      const n = items[Math.min(Math.max(i, 0), last)]
      setSelection([n.path], n.path, n.path)
    }
    switch (action) {
      case 'down': return at(cur + 1)
      case 'up': return at(cur - 1)
      case 'top': return at(0)
      case 'bottom': return at(last)
      case 'parent': return runCommand('parent')
      case 'open': return void openNode(items[Math.max(cur, 0)])
      case 'nextMatch': return tab.filter ? at(cur >= last ? 0 : cur + 1) : undefined
      case 'prevMatch': return tab.filter ? at(cur <= 0 ? last : cur - 1) : undefined
      case 'copyPath':
        at(Math.max(cur, 0))
        return runCommand('copy-path')
      case 'trash':
        at(Math.max(cur, 0))
        lastVimRepeat = 'delete'
        return runCommand('delete')
      case 'paste':
        lastVimRepeat = 'paste'
        return runCommand('paste')
      case 'repeat':
        if (lastVimRepeat === 'delete') at(Math.max(cur, 0))
        if (lastVimRepeat) runCommand(lastVimRepeat)
        return undefined
      default:
        return undefined
    }
  }

  const closeFilter = () => {
    useExplorer.getState().updateTab(tab.id, { filter: null })
    useExplorer.getState().requestFocus('list')
  }

  const onFilterKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      closeFilter()
    } else if (e.key === 'Enter') {
      e.preventDefault()
      const first = items[0]
      closeFilter()
      if (first) void openNode(first)
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      scrollRef.current?.focus()
    }
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Tab' && isSplit && !e.ctrlKey && !e.altKey) {
      e.preventDefault()
      const st = useExplorer.getState()
      st.focusPane(pane === 'split' ? 'primary' : 'split')
      st.requestFocus('list')
      return
    }
    // Any key that is not a printable character breaks a half-typed chord.
    if (e.key.length > 1) vimPending.current = ''
    if (!items.length) return
    const cur = tab.cursor ? (index.get(tab.cursor) ?? -1) : -1
    const page = Math.max(1, Math.floor((scrollRef.current?.clientHeight ?? 300) / ROW) - 1)
    if (vimMode && e.ctrlKey && !e.metaKey && !e.altKey && (e.key === 'd' || e.key === 'u')) {
      e.preventDefault()
      const half = Math.max(1, Math.floor(page / 2))
      const base = Math.max(cur, 0)
      const target = Math.min(Math.max(e.key === 'd' ? base + half : base - half, 0), items.length - 1)
      setSelection([items[target].path], items[target].path, items[target].path)
      return
    }
    if (vimMode && !e.ctrlKey && !e.metaKey && !e.altKey && e.key.length === 1) {
      const step = vimStep(vimPending.current, e.key)
      vimPending.current = step.pending
      if (step.action || step.pending) e.preventDefault()
      if (step.action) runVim(step.action, cur)
      return
    }
    let next: number | null = null
    switch (e.key) {
      case 'ArrowDown': next = Math.min(cur + 1, items.length - 1); break
      case 'ArrowUp': next = Math.max(cur - 1, 0); break
      case 'PageDown': next = Math.min(cur + page, items.length - 1); break
      case 'PageUp': next = Math.max(cur - page, 0); break
      case 'Home': next = 0; break
      case 'End': next = items.length - 1; break
      case 'ArrowRight': {
        const node = items[cur]
        if (node?.is_dir && !e.altKey) {
          e.preventDefault()
          void openNode(node)
        }
        return
      }
      case 'ArrowLeft':
        if (!e.altKey && !searching) {
          e.preventDefault()
          useExplorer.getState().goUp()
        }
        return
      default:
        // Type-ahead: jump to the next name starting with what was typed.
        // "/" and "?" are shortcuts (search, help), so they are never typed ahead.
        if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey && e.key !== ' ' && e.key !== '/' && e.key !== '?') {
          const r = typeaheadStep(typeahead.current, e.key, Date.now(), names, cur)
          typeahead.current = r.state
          if (r.index !== null) setSelection([items[r.index].path], items[r.index].path, items[r.index].path)
          e.preventDefault()
        }
        return
    }
    e.preventDefault()
    const node = items[next]
    if (e.shiftKey && tab.anchor && index.has(tab.anchor)) {
      const a = index.get(tab.anchor)!
      const [lo, hi] = a < next ? [a, next] : [next, a]
      setSelection(items.slice(lo, hi + 1).map((n) => n.path), tab.anchor, node.path)
    } else {
      setSelection([node.path], node.path, node.path)
    }
  }

  let body: React.ReactNode = null
  if (tab.listingError && !searching) {
    body = <EmptyState icon={<AlertTriangle className="size-8" />} title="Can't read this folder" detail={tab.listingError} />
  } else if (items.length === 0) {
    if (tab.filter !== null && allItems.length > 0) {
      body = <EmptyState icon={<SearchX className="size-8" />} title="Nothing matches the filter" detail="Esc clears it." />
    } else if (searching && tab.searching) {
      body = <EmptyState icon={<Loader2 className="size-7 animate-spin text-primary" />} title="Searching…" />
    } else if (searching) {
      body = <EmptyState icon={<SearchX className="size-8" />} title="No matches" detail={tab.searchError ?? 'Try fewer filters, or rebuild the index if files are missing.'} />
    } else if (!tab.loading) {
      body = <EmptyState icon={<FolderOpen className="size-8" />} title="This folder is empty" detail={!showHidden && tab.items.length ? `${tab.items.length} hidden item(s). Toggle hidden files to show them.` : undefined} />
    }
  }

  // Two panes side by side drop the Kind column, as in the design.
  const cols = isSplit ? 'grid-cols-[minmax(0,1fr)_76px_92px]' : 'grid-cols-[minmax(0,1fr)_84px_100px_150px]'

  return (
    <section
      aria-label={isSplit ? `${pane === 'split' ? 'Right' : 'Left'} pane` : 'Files'}
      onMouseDownCapture={() => focusPane(pane)}
      onFocusCapture={() => focusPane(pane)}
      data-drop-tab={tab.id}
      className={cn(
        'flex min-w-0 flex-1 flex-col bg-background',
        isSplit && focused && 'shadow-[inset_0_2px_0_var(--primary)]',
        dropPane && 'shadow-[inset_0_0_0_2px_var(--primary)]',
      )}
    >
      {isSplit && (
        <div
          className={cn(
            'flex h-7 shrink-0 items-center gap-2 border-b bg-card px-3 pt-0.5 font-mono text-xs',
            focused ? 'text-foreground' : 'text-muted-foreground',
          )}
          title={tab.dir}
        >
          <Folder className="size-3.5 shrink-0" strokeWidth={1.5} />
          <span className="truncate">{tab.activeQuery !== null ? `search: ${tab.activeQuery}` : tab.dir}</span>
        </div>
      )}
      {searching && <SearchHeader tab={tab} />}
      {tab.loading && !searching && (tab.listing !== null || slow) && (
        <div role="status" className="flex h-7 shrink-0 items-center gap-2 border-b bg-card px-3 text-xs text-muted-foreground">
          <Loader2 className="size-3.5 shrink-0 animate-spin text-primary" />
          <span className="truncate">
            {slow && tab.listing === null
              ? `Waiting for ${volumeLabel(tab.dir)}…`
              : `Loading… ${(tab.listing ?? 0).toLocaleString()} items`}
          </span>
          {slow && (
            <Button
              variant="outline"
              size="xs"
              className="ml-auto"
              onClick={() => useExplorer.getState().abandonListing(tab.id)}
            >
              Cancel
            </Button>
          )}
        </div>
      )}

      <div className="flex min-h-0 flex-1">
      {searching && !isSplit && <RefinePanel tab={tab} />}
      <div className="flex min-w-0 flex-1 flex-col">
      {tab.filter !== null && (
        <div className="flex h-8 shrink-0 items-center gap-2 border-b bg-card px-3">
          <Filter className="size-3.5 shrink-0 text-muted-foreground" strokeWidth={1.5} />
          <Input
            ref={filterInput}
            value={tab.filter}
            onChange={(e) => useExplorer.getState().updateTab(tab.id, { filter: e.target.value })}
            onKeyDown={onFilterKey}
            onFocus={() => focusPane(pane)}
            placeholder="Filter this folder by name"
            aria-label="Quick filter"
            className="h-6 border-0 bg-transparent px-0 shadow-none focus-visible:ring-0"
          />
          <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
            {items.length}/{allItems.length}
          </span>
        </div>
      )}
      {!columns && (
      <div className={cn('grid h-7.5 shrink-0 border-b bg-background', cols)}>
        <HeaderCell tab={tab} pane={pane} col="name" label={searching ? 'Name' : 'Name'} sortable={!searching} className="pl-10" />
        <HeaderCell tab={tab} pane={pane} col="size" label="Size" sortable={!searching} className="justify-end" />
        <HeaderCell tab={tab} pane={pane} col="modified" label="Modified" sortable={!searching} className="justify-end" />
        {!isSplit && <HeaderCell tab={tab} pane={pane} col="kind" label={searching ? 'Location' : 'Kind'} sortable={!searching} />}
      </div>
      )}

      {columns ? <ColumnView tab={tab} pane={pane} /> : (
      <ContextMenu>
        <ContextMenuTrigger
          render={
            <div
              ref={scrollRef}
              tabIndex={0}
              onKeyDown={onKeyDown}
              onMouseDown={(e) => {
                if (e.target === e.currentTarget && e.button === 0) setSelection([], null, null)
              }}
              className="relative min-h-0 flex-1 overflow-y-auto outline-none"
            />
          }
        >
          {body ?? (
            <div style={{ height: virtualizer.getTotalSize() }} className="relative w-full">
              {virtualizer.getVirtualItems().map((v) => {
                const node = items[v.index]
                const isSel = selected.has(node.path)
                const isCursor = tab.cursor === node.path
                const mark = tab.git?.marks[node.path]
                const size = node.is_dir ? dirSizes[node.path] : node.size
                const dropOn = dropHover?.tabId === tab.id && dropHover.dir === node.path
                return (
                  <div
                    key={node.path}
                    data-index={v.index}
                    data-drop-dir={node.is_dir && !tab.archive ? node.path : undefined}
                    onPointerDown={(e) => {
                      if (e.button !== 0) return
                      const paths = selected.has(node.path) ? tab.selected : [node.path]
                      beginRowDrag(e, paths, node.is_dir && paths.length === 1 ? node.path : null)
                    }}
                    onMouseDown={(e) => {
                      if (e.button === 0) clickRow(e, node)
                    }}
                    onContextMenu={() => {
                      if (!isSel) setSelection([node.path], node.path, node.path)
                    }}
                    onDoubleClick={() => void openNode(node)}
                    style={{ transform: `translateY(${v.start}px)`, height: ROW }}
                    className={cn(
                      'absolute top-0 left-0 grid w-full cursor-default items-center',
                      cols,
                      isSel
                        ? 'bg-row-selected shadow-[inset_2px_0_0_var(--primary)]'
                        : 'hover:shadow-[inset_0_0_0_999px_rgb(255_255_255/0.04)]',
                      isCursor && !isSel && 'shadow-[inset_2px_0_0_var(--input)]',
                      cutPaths.has(node.path) && 'opacity-50',
                      dropOn && 'bg-primary/15',
                    )}
                  >
                    <div className="flex min-w-0 items-center gap-2.5 pl-3">
                      <FileIcon node={node} />
                      <span className={cn('truncate', (node.hidden || mark === 'I' || ignoredSet.has(node.path)) && 'text-dim')}>{node.name}</span>
                      {mark && mark !== 'I' && (
                        <span className={cn('shrink-0 font-mono text-[11px] font-medium', GIT_STYLE[mark])} title="git status">
                          {mark}
                        </span>
                      )}
                    </div>
                    <div className="px-2 text-right font-mono text-xs text-muted-foreground">
                      {node.is_dir
                        ? size === 'pending'
                          ? <Loader2 className="ml-auto size-3 animate-spin" />
                          : typeof size === 'number'
                            ? formatSize(size)
                            : size === 'failed' ? '?' : '—'
                        : formatSize(node.size)}
                    </div>
                    <div className="truncate px-2 text-right text-xs text-muted-foreground">{formatTimestamp(node.modified_at)}</div>
                    {!isSplit && (
                      <div className={cn('truncate px-3 text-xs text-muted-foreground', searching && 'font-mono text-[11px]')} title={searching ? parentOf(node.path) ?? '' : undefined}>
                        {searching ? parentOf(node.path) : kindLabel(node)}
                      </div>
                    )}
                  </div>
                )
              })}
            </div>
          )}
        </ContextMenuTrigger>
        <RowMenu />
      </ContextMenu>
      )}
      {isSplit && focused && (
        <div className="m-3 flex shrink-0 items-center gap-2 rounded-md border border-dashed border-input px-3 py-2 text-xs text-muted-foreground">
          <kbd className="rounded border border-input px-1.5 font-mono text-[11px]">F5</kbd> copy to the other pane
          <kbd className="rounded border border-input px-1.5 font-mono text-[11px]">F6</kbd> move
          <kbd className="rounded border border-input px-1.5 font-mono text-[11px]">Tab</kbd> switch
        </div>
      )}
      </div>
      </div>
    </section>
  )
}
