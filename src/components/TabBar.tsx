import { useRef, useState } from 'react'
import { ExternalLink, Folder, Plus, Search, X } from 'lucide-react'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { toast } from 'sonner'

import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from '@/components/ui/context-menu'
import { WindowControls } from '@/components/WindowControls'
import { useWindowFrame } from '@/hooks/useWindowFrame'
import { api, errorText } from '@/lib/api'
import { basename } from '@/lib/format'
import { isMac } from '@/lib/platform'
import { cn } from '@/lib/utils'
import { useExplorer, type Tab } from '@/store/explorer'

/** Pixels the pointer must travel before a press becomes a drag. */
const DRAG_THRESHOLD = 6

interface DragState {
  id: number
  x: number
  y: number
  /** Insertion point among the tabs (0..n), when over the tab strip. */
  over: number | null
  /** The pointer has left the tab strip: dropping tears the tab off. */
  outside: boolean
}

function tabLabel(t: Tab) {
  return t.activeQuery !== null ? t.activeQuery : basename(t.dir) || t.dir
}

export function TabBar() {
  const allTabs = useExplorer((s) => s.tabs)
  const tabs = allTabs.filter((t) => !t.split)
  const activeTabId = useExplorer((s) => s.activeTabId)
  const setActiveTab = useExplorer((s) => s.setActiveTab)
  const closeTab = useExplorer((s) => s.closeTab)
  const newTab = useExplorer((s) => s.newTab)
  const moveTab = useExplorer((s) => s.moveTab)
  const moveTabToWindow = useExplorer((s) => s.moveTabToWindow)

  const frame = useWindowFrame()
  const barRef = useRef<HTMLDivElement>(null)
  const press = useRef<{ id: number; x: number; y: number; dragging: boolean } | null>(null)
  const [drag, setDrag] = useState<DragState | null>(null)

  const insertionPoint = (clientX: number, clientY: number): number | null => {
    const bar = barRef.current?.getBoundingClientRect()
    if (!bar || clientY < bar.top - 24 || clientY > bar.bottom + 24) return null
    const els = [...(barRef.current?.querySelectorAll<HTMLElement>('[data-tab-id]') ?? [])]
    let at = els.length
    for (let i = 0; i < els.length; i++) {
      const r = els[i].getBoundingClientRect()
      if (clientX < r.left + r.width / 2) {
        at = i
        break
      }
    }
    return at
  }

  const onPointerDown = (e: React.PointerEvent, t: Tab) => {
    if (e.button !== 0) return
    setActiveTab(t.id)
    press.current = { id: t.id, x: e.clientX, y: e.clientY, dragging: false }
    // Capture keeps pointer events coming while the pointer is outside the
    // window, which is how a tab is dragged out.
    e.currentTarget.setPointerCapture(e.pointerId)
  }

  const onPointerMove = (e: React.PointerEvent) => {
    const p = press.current
    if (!p) return
    if (!p.dragging && Math.hypot(e.clientX - p.x, e.clientY - p.y) < DRAG_THRESHOLD) return
    p.dragging = true
    const outsideWindow =
      e.clientX < 0 || e.clientY < 0 || e.clientX > window.innerWidth || e.clientY > window.innerHeight
    const over = outsideWindow ? null : insertionPoint(e.clientX, e.clientY)
    // Pulling the tab off the strip is enough to tear it off; no need to reach the window edge.
    setDrag({ id: p.id, x: e.clientX, y: e.clientY, over, outside: over === null })
  }

  const finish = async (e: React.PointerEvent) => {
    const p = press.current
    press.current = null
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId)
    const d = drag
    setDrag(null)
    if (!p?.dragging || !d) return
    const tab = useExplorer.getState().tabs.find((t) => t.id === p.id)
    if (!tab) return

    if (d.outside) {
      const only = useExplorer.getState().visibleTabs().length === 1
      const dpr = window.devicePixelRatio || 1
      try {
        // Another window under the pointer adopts the tab; otherwise a new
        // window opens there (unless this is the window's only tab).
        const result = await api.dropTab(tab.dir, e.screenX * dpr, e.screenY * dpr, !only)
        if (result === 'none') return
        if (only) await getCurrentWindow().close()
        else closeTab(tab.id)
      } catch (err) {
        toast.error('Could not move the tab', { description: errorText(err) })
      }
    } else if (d.over !== null) {
      moveTab(tab.id, d.over)
    }
  }

  const dragged = drag ? tabs.find((t) => t.id === drag.id) : null

  return (
    // The strip is also the window's title bar: empty space drags the window
    // and double-clicking it maximizes (data-tauri-drag-region).
    <div data-tauri-drag-region className="relative flex h-10 shrink-0 border-b bg-chrome">
    <div
      ref={barRef}
      role="tablist"
      aria-label="Tabs"
      data-tauri-drag-region
      className={cn(
        'flex min-w-0 flex-1 items-end gap-0.5 pt-2 pr-3',
        // Room for the traffic lights, which overlay the strip on macOS.
        isMac && !frame.fullscreen ? 'pl-[78px]' : 'pl-3',
      )}
    >
      {tabs.map((t, i) => {
        const active = t.id === activeTabId
        const searching = t.activeQuery !== null
        const Icon = searching ? Search : Folder
        const showMarker = drag && !drag.outside && drag.over === i && drag.id !== t.id && tabs[i - 1]?.id !== drag.id
        return (
          <ContextMenu key={t.id}>
            <ContextMenuTrigger
              render={<div />}
              data-tab-id={t.id}
              role="tab"
              aria-selected={active}
              title={searching ? `Search: ${t.activeQuery}` : t.dir}
              onPointerDown={(e: React.PointerEvent) => onPointerDown(e, t)}
              onPointerMove={onPointerMove}
              onPointerUp={(e: React.PointerEvent) => void finish(e)}
              onPointerCancel={() => {
                press.current = null
                setDrag(null)
              }}
              onMouseDown={(e: React.MouseEvent) => {
                if (e.button === 1) {
                  e.preventDefault()
                  closeTab(t.id)
                }
              }}
              className={cn(
                'group relative -mb-px flex h-8 max-w-56 min-w-28 cursor-default touch-none items-center gap-2 rounded-t-md px-3.5 transition-colors',
                active
                  ? 'border border-b-card bg-card text-foreground shadow-[inset_0_2px_0_var(--primary)]'
                  : 'text-muted-foreground hover:bg-accent',
                drag?.id === t.id && 'opacity-40',
              )}
            >
              {showMarker && <span aria-hidden className="absolute -left-1 top-1 bottom-1 w-0.5 rounded-full bg-primary" />}
              <Icon className="size-3.5 shrink-0" strokeWidth={1.6} />
              <span className={cn('truncate', active && 'font-medium', searching && 'font-mono text-xs')}>{tabLabel(t)}</span>
              {i < 9 && !active && (
                <span className="ml-auto hidden font-mono text-[10px] text-dim group-hover:inline">{i + 1}</span>
              )}
              {tabs.length > 1 && (
                <button
                  aria-label="Close tab"
                  onPointerDown={(e) => e.stopPropagation()}
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={() => closeTab(t.id)}
                  className={cn(
                    'ml-auto rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground',
                    active ? 'opacity-100' : 'hidden group-hover:block',
                  )}
                >
                  <X className="size-3" />
                </button>
              )}
            </ContextMenuTrigger>
            <ContextMenuContent className="w-52">
              <ContextMenuItem className="text-[12.5px]" onClick={() => newTab(t.dir)}>Duplicate Tab</ContextMenuItem>
              <ContextMenuItem className="text-[12.5px]" onClick={() => moveTabToWindow(t.id)}>Move to New Window</ContextMenuItem>
              <ContextMenuSeparator />
              <ContextMenuItem className="text-[12.5px]" disabled={tabs.length <= 1} onClick={() => closeTab(t.id)}>Close Tab</ContextMenuItem>
            </ContextMenuContent>
          </ContextMenu>
        )
      })}
      {drag && !drag.outside && drag.over === tabs.length && tabs[tabs.length - 1]?.id !== drag.id && (
        <span aria-hidden className="mb-1 h-6 w-0.5 rounded-full bg-primary" />
      )}
      <button
        aria-label="New tab"
        onClick={() => newTab()}
        className="mb-0.5 ml-1 grid size-7.5 place-items-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
      >
        <Plus className="size-3.5" />
      </button>

      {/* The tab follows the pointer while dragged. */}
      {drag && dragged && (
        <div
          aria-hidden
          style={{
            left: Math.min(Math.max(drag.x, 8), window.innerWidth - 220) + 12,
            top: Math.min(Math.max(drag.y, 8), window.innerHeight - 40) + 10,
          }}
          className="pointer-events-none fixed z-50 flex max-w-56 items-center gap-2 rounded-md border border-primary/60 bg-card px-3 py-1.5 text-xs shadow-2xl shadow-black/60"
        >
          {drag.outside ? <ExternalLink className="size-3.5 text-primary" /> : <Folder className="size-3.5" />}
          <span className="truncate">{tabLabel(dragged)}</span>
          {drag.outside && <span className="text-muted-foreground">· release to move</span>}
        </div>
      )}
    </div>
    <WindowControls maximized={frame.maximized} />
    </div>
  )
}
