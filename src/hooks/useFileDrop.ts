import { useEffect } from 'react'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { toast } from 'sonner'

import { api, errorText } from '@/lib/api'
import { useExplorer } from '@/store/explorer'

/**
 * Which pane and folder row sit under a point. The native drag reports the
 * point in physical pixels, relative to the window.
 */
function targetAt(x: number, y: number): { tabId: number | null; dir: string | null } {
  const dpr = window.devicePixelRatio || 1
  const el = document.elementFromPoint(x / dpr, y / dpr)
  const row = el?.closest<HTMLElement>('[data-drop-dir]')
  const pane = el?.closest<HTMLElement>('[data-drop-tab]')
  return { tabId: pane ? Number(pane.dataset.dropTab) : null, dir: row?.dataset.dropDir ?? null }
}

/**
 * Files dragged in from Finder, Explorer or another app are copied into the
 * folder row under the pointer, or else into the folder of the pane they are
 * dropped on. Copies go through the normal transfer, so progress, conflicts
 * and undo all apply.
 */
export function useFileDrop() {
  useEffect(() => {
    const setHover = (tabId: number | null, dir: string | null) => {
      const st = useExplorer.getState()
      const next = { tabId: tabId ?? st.activeTabId, dir }
      const cur = st.dropHover
      if (cur?.tabId === next.tabId && cur.dir === next.dir) return
      useExplorer.setState({ dropHover: next })
    }

    const sub = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload
      if (p.type === 'leave') {
        useExplorer.setState({ dropHover: null })
        return
      }
      if (p.type === 'enter' || p.type === 'over') {
        const t = targetAt(p.position.x, p.position.y)
        setHover(t.tabId, t.dir)
        return
      }
      // drop
      useExplorer.setState({ dropHover: null })
      if (p.paths.length === 0) return
      const t = targetAt(p.position.x, p.position.y)
      const st = useExplorer.getState()
      const pane = st.tabs.find((tab) => tab.id === (t.tabId ?? st.activeTabId)) ?? st.activeTab()
      if (pane.archive) {
        toast.error("Can't copy into an archive", { description: 'Archives are read-only. Drop onto a folder outside it.' })
        return
      }
      void api
        .resolveDrop(p.paths, t.dir, pane.dir)
        .then((plan) => st.startTransfer(plan.sources, plan.dest, false))
        .catch((e) => toast.error("Couldn't copy here", { description: errorText(e) }))
    })
    return () => {
      void sub.then((un) => un())
    }
  }, [])
}
