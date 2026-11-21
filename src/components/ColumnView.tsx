import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { AlertTriangle, ChevronRight, Loader2 } from 'lucide-react'

import { PreviewView } from '@/components/DetailsPanel'
import { FileIcon } from '@/components/FileIcon'
import { api, errorText, events, type VNode } from '@/lib/api'
import { matchesQuickFilter } from '@/lib/quickFilter'
import { typeaheadStep, type TypeaheadState } from '@/lib/typeahead'
import { cn } from '@/lib/utils'
import { displayItems, useExplorer, type Pane, type Tab } from '@/store/explorer'

const ROW = 28
const COLUMN_WIDTH = 240
const PREVIEW_WIDTH = 380
/** Rows moved by PageUp and PageDown. */
const PAGE = 10

/**
 * A folder's listing with the paths git ignores in it, or why it could not be read.
 * Column 0 uses the tab's own listing.
 */
type Listing = { items: VNode[]; ignored: string[] } | { error: string }

function normDir(p: string): string {
  return p.replace(/[\\/]+$/, '').toLowerCase()
}

/** Reads a folder and asks git which of its entries it ignores. One batched call per read. */
function readFolder(path: string): Promise<Listing> {
  return api
    .listDir(path)
    .then(async (items): Promise<Listing> => {
      const ignored = await api.gitIgnored(path, items.map((n) => n.path)).catch(() => [])
      return { items, ignored }
    })
    .catch((e): Listing => ({ error: errorText(e) }))
}

function Column({
  path,
  col,
  items,
  error,
  cursor,
  openPath,
  active,
  onChoose,
  onOpen,
}: {
  path: string
  col: number
  /** Null while the folder is still being read. */
  items: VNode[] | null
  error: string | null
  cursor: string | null
  /** The folder this column opens, which its row in this column keeps marked. */
  openPath: string | null
  active: boolean
  onChoose: (col: number, node: VNode) => void
  onOpen: (node: VNode) => void
}) {
  const scrollRef = useRef<HTMLDivElement>(null)
  // The compiler skips memoizing this component, which is harmless: the virtualizer is only read here.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: items?.length ?? 0,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW,
    overscan: 10,
  })

  // The cursor row follows the keyboard, but only in the column that has focus.
  const cursorIndex = items ? items.findIndex((n) => n.path === cursor) : -1
  useEffect(() => {
    if (active && cursorIndex >= 0) virtualizer.scrollToIndex(cursorIndex, { align: 'auto' })
  }, [active, cursorIndex, virtualizer])

  let body: React.ReactNode
  if (error) {
    body = (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-4 text-center text-xs text-muted-foreground">
        <AlertTriangle className="size-5" />
        {error}
      </div>
    )
  } else if (items === null) {
    body = (
      <div className="grid h-full place-items-center">
        <Loader2 className="size-4 animate-spin text-muted-foreground" />
      </div>
    )
  } else if (items.length === 0) {
    body = <div className="grid h-full place-items-center text-xs text-dim">Empty folder</div>
  } else {
    body = (
      <div style={{ height: virtualizer.getTotalSize() }} className="relative w-full">
        {virtualizer.getVirtualItems().map((v) => {
          const node = items[v.index]
          const isCursor = node.path === cursor
          return (
            <div
              key={node.path}
              onMouseDown={(e) => {
                if (e.button === 0) onChoose(col, node)
              }}
              onDoubleClick={() => onOpen(node)}
              style={{ transform: `translateY(${v.start}px)`, height: ROW }}
              className={cn(
                'absolute top-0 left-0 flex w-full cursor-default items-center gap-2 pr-2 pl-3 text-[12.5px]',
                isCursor && active
                  ? 'bg-row-selected shadow-[inset_2px_0_0_var(--primary)]'
                  : (isCursor || node.path === openPath) && 'bg-accent',
              )}
            >
              <FileIcon node={node} className="size-4 shrink-0" />
              <span className={cn('min-w-0 flex-1 truncate', node.hidden && 'text-dim')}>{node.name}</span>
              {node.is_dir && <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />}
            </div>
          )
        })}
      </div>
    )
  }

  return (
    <div
      data-col={col}
      title={path}
      style={{ width: COLUMN_WIDTH }}
      className="flex h-full shrink-0 flex-col border-r bg-background"
    >
      <div ref={scrollRef} className="relative min-h-0 flex-1 overflow-y-auto">
        {body}
      </div>
    </div>
  )
}

/** Stands in for the size map when sizes do not affect the order. */
const NO_SIZES: Record<string, never> = {}

/**
 * Finder-style columns: one column per open folder, and the selected file's
 * preview as the last column. Keys: ← → move between columns, ↑ ↓ within one,
 * type-ahead searches the active column. Archives stay read-only, because
 * the file commands already refuse inside them.
 */
export function ColumnView({ tab, pane }: { tab: Tab; pane: Pane }) {
  const focused = useExplorer((s) => s.focusedTabId() === tab.id)
  const isSplit = useExplorer((s) => s.splitTabId !== null)
  const focusRequest = useExplorer((s) => s.focusRequest)
  const showHidden = useExplorer((s) => s.showHidden)
  const hideIgnored = useExplorer((s) => s.hideIgnored)
  const dirSizes = useExplorer((s) => s.dirSizes)
  const focusPane = useExplorer((s) => s.focusPane)

  const wrapRef = useRef<HTMLDivElement>(null)
  const typeahead = useRef<TypeaheadState>({ text: '', at: 0 })
  const [listings, setListings] = useState<Record<string, Listing>>({})

  // Each read gets a token from one counter, and only the newest token for a path may store its
  // result. A slow read that finishes after a newer one (or after the tab changed) is dropped.
  const readCounter = useRef(0)
  const latestRead = useRef<Record<string, number>>({})
  const loadListing = useCallback((path: string) => {
    const token = ++readCounter.current
    latestRead.current[path] = token
    void readFolder(path).then((got) => {
      if (latestRead.current[path] !== token) return
      setListings((l) => ({ ...l, [path]: got }))
    })
  }, [])

  const colPath = tab.colPath
  const colKey = colPath.join('\u0000')
  const last = colPath.length

  // Cached listings belong to one tab. On a tab switch they are dropped, and reads still in
  // flight for the old tab are abandoned.
  const openedBefore = useRef<string[]>([])
  const tabId = tab.id
  const shownTab = useRef(tabId)
  useEffect(() => {
    if (shownTab.current === tabId) return
    shownTab.current = tabId
    latestRead.current = {}
    openedBefore.current = []
    setListings((l) => (Object.keys(l).length ? {} : l))
  }, [tabId])

  // A folder is read when it is first opened. Once open, watchers keep it current, so moving
  // the cursor within the columns does not read them again. Column 0 comes from the tab.
  useEffect(() => {
    const prev = openedBefore.current
    openedBefore.current = colPath
    for (const path of colPath) {
      if (prev.includes(path)) continue
      loadListing(path)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [colKey, tabId, loadListing])

  // A change in an open folder re-reads just that folder. The store refreshes the tab's own folder.
  const colPathRef = useRef(colPath)
  colPathRef.current = colPath
  useEffect(() => {
    const sub = events.onDirChanged((dir) => {
      const key = normDir(dir)
      for (const path of colPathRef.current) {
        if (normDir(path) !== key) continue
        loadListing(path)
      }
    })
    return () => {
      void sub.then((un) => un())
    }
  }, [loadListing])

  // Sizes only change the order when sorting by size; otherwise they are left out of the memo.
  const sizesInOrder = tab.sort.col === 'size' ? dirSizes : NO_SIZES
  const columns = useMemo(() => {
    const visible = (items: VNode[], ignored: string[]) => {
      const sorted = displayItems({ ...tab, items, ignored }, showHidden, sizesInOrder, hideIgnored)
      return tab.filter === null ? sorted : sorted.filter((n) => matchesQuickFilter(n.name, tab.filter ?? ''))
    }
    const out: { path: string; items: VNode[] | null; error: string | null }[] = [
      {
        path: tab.dir,
        items: tab.loading && tab.items.length === 0 && !tab.listingError ? null : visible(tab.items, tab.ignored),
        error: tab.listingError,
      },
    ]
    for (const path of colPath) {
      const got = listings[path]
      if (!got) out.push({ path, items: null, error: null })
      else if ('error' in got) out.push({ path, items: null, error: got.error })
      else out.push({ path, items: visible(got.items, got.ignored), error: null })
    }
    return out
    // `listings` is keyed by folder path, so colKey already names the folders involved.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.dir, tab.items, tab.ignored, tab.loading, tab.listing, tab.listingError, tab.sort, tab.filter, colKey, listings, showHidden, sizesInOrder, hideIgnored])

  // The column holding the cursor. A folder row is in the column it opens from; anything else is in the last column.
  const cursor = tab.cursor
  const openIndex = cursor ? colPath.indexOf(cursor) : -1
  const active = openIndex >= 0 ? openIndex : last
  const activeItems = columns[active]?.items ?? []
  const cur = cursor ? activeItems.findIndex((n) => n.path === cursor) : -1
  const cursorNode = cur >= 0 ? activeItems[cur] : undefined
  const previewNode = cursorNode && !cursorNode.is_dir && active === last ? cursorNode : null

  // Keep the active column (or the preview) in view when it moves.
  const scrollTarget = previewNode ? 'preview' : String(active)
  useEffect(() => {
    wrapRef.current
      ?.querySelector<HTMLElement>(`[data-col="${scrollTarget}"]`)
      ?.scrollIntoView({ inline: 'nearest', block: 'nearest' })
  }, [scrollTarget, last])

  useEffect(() => {
    if (focusRequest.target === 'list' && focused) wrapRef.current?.focus()
  }, [focusRequest, focused])

  const choose = (col: number, node: VNode) => {
    useExplorer.getState().selectColumnRow(tab.id, col, node.path, node.is_dir)
  }
  /** Move the cursor to a row without changing which folders are open. */
  const select = (path: string) => {
    useExplorer.getState().updateTab(tab.id, { selected: [path], anchor: path, cursor: path })
  }
  const onOpen = (node: VNode) => void useExplorer.getState().openNode(node)

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Tab' && isSplit && !e.ctrlKey && !e.altKey) {
      e.preventDefault()
      const st = useExplorer.getState()
      st.focusPane(pane === 'split' ? 'primary' : 'split')
      st.requestFocus('list')
      return
    }
    const plain = !e.ctrlKey && !e.metaKey && !e.altKey
    const n = activeItems.length
    let next: number | null = null
    switch (e.key) {
      case 'ArrowDown':
        next = Math.min(cur + 1, n - 1)
        break
      case 'ArrowUp':
        next = Math.max(cur - 1, 0)
        break
      case 'PageDown':
        next = Math.min(cur + PAGE, n - 1)
        break
      case 'PageUp':
        next = Math.max(cur - PAGE, 0)
        break
      case 'Home':
        next = 0
        break
      case 'End':
        next = n - 1
        break
      case 'ArrowRight': {
        e.preventDefault()
        if (!cursorNode) return
        if (cursorNode.is_dir && active === last) {
          // On the last column a folder opens its own column.
          choose(active, cursorNode)
        } else if (active < last) {
          // Move into the column this one opens. Its first row, if no folder is open there yet.
          if (active + 1 < last) {
            useExplorer.getState().selectColumnRow(tab.id, active + 1, colPath[active + 1], true)
          } else {
            const first = columns[last]?.items?.[0]
            if (first) choose(last, first)
          }
        }
        return
      }
      case 'ArrowLeft': {
        e.preventDefault()
        if (active > 0) select(colPath[active - 1])
        else useExplorer.getState().goUp()
        return
      }
      case 'Enter':
        e.preventDefault()
        if (cursorNode) onOpen(cursorNode)
        return
      default:
        // Type-ahead within the active column. "/" and "?" are shortcuts, so they are never typed ahead.
        if (plain && e.key.length === 1 && e.key !== ' ' && e.key !== '/' && e.key !== '?') {
          const r = typeaheadStep(typeahead.current, e.key, Date.now(), activeItems.map((x) => x.name), cur)
          typeahead.current = r.state
          if (r.index !== null) choose(active, activeItems[r.index])
          e.preventDefault()
        }
        return
    }
    if (next === null || n === 0) return
    e.preventDefault()
    choose(active, activeItems[next])
  }

  return (
    <div
      ref={wrapRef}
      tabIndex={0}
      aria-label="Columns"
      onKeyDown={onKeyDown}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) focusPane(pane)
      }}
      className="flex min-h-0 flex-1 overflow-x-auto overflow-y-hidden outline-none"
    >
      {columns.map((c, col) => (
        <Column
          key={c.path}
          path={c.path}
          col={col}
          items={c.items}
          error={c.error}
          cursor={cursor}
          openPath={col < last ? colPath[col] : null}
          active={col === active}
          onChoose={choose}
          onOpen={onOpen}
        />
      ))}
      {previewNode && (
        <div
          data-col="preview"
          style={{ width: PREVIEW_WIDTH }}
          className="flex h-full shrink-0 flex-col gap-2 border-r bg-card p-3"
        >
          <div className="truncate text-[12.5px] font-medium" title={previewNode.path}>
            {previewNode.name}
          </div>
          <PreviewView key={previewNode.path} path={previewNode.path} />
        </div>
      )}
    </div>
  )
}
