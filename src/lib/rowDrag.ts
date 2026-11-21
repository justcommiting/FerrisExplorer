import type { PointerEvent as ReactPointerEvent } from 'react'
import { toast } from 'sonner'

import { api, errorText } from '@/lib/api'
import { useExplorer } from '@/store/explorer'

/** Pixels the pointer must travel before a press becomes a drag. */
const DRAG_THRESHOLD = 6

/**
 * Drag rows with the pointer. Released over the sidebar, a single folder is
 * pinned to Favorites. Once the pointer leaves the window, the rows become a
 * native drag, so the app they land in receives the real files.
 */
export function beginRowDrag(e: ReactPointerEvent, paths: string[], pinFolder: string | null) {
  const x0 = e.clientX
  const y0 = e.clientY
  let active = false

  const cleanup = () => {
    window.removeEventListener('pointermove', onMove)
    window.removeEventListener('pointerup', onUp)
    window.removeEventListener('pointercancel', cleanup)
  }

  const onMove = (ev: PointerEvent) => {
    if (!active) {
      if (Math.hypot(ev.clientX - x0, ev.clientY - y0) < DRAG_THRESHOLD) return
      active = true
    }
    const outside =
      ev.clientX < 0 || ev.clientY < 0 || ev.clientX > window.innerWidth || ev.clientY > window.innerHeight
    if (!outside) return
    cleanup()
    api.startDrag(paths).catch((err) => toast.error("Couldn't drag these items out", { description: errorText(err) }))
  }

  const onUp = (ev: PointerEvent) => {
    cleanup()
    if (!active || !pinFolder) return
    const over = document.elementFromPoint(ev.clientX, ev.clientY)
    if (over?.closest('[data-pin-zone]')) useExplorer.getState().addFavorite(pinFolder)
  }

  window.addEventListener('pointermove', onMove)
  window.addEventListener('pointerup', onUp)
  window.addEventListener('pointercancel', cleanup)
}
