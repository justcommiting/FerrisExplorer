import { useEffect, useState } from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'

import { isMac, isWindows } from '@/lib/platform'

/**
 * Tracks whether the window is maximized or fullscreen. Both change the
 * frame: the restore glyph on Windows, and on macOS the traffic lights
 * disappear in fullscreen so the tab strip no longer needs room for them.
 */
export function useWindowFrame() {
  const [frame, setFrame] = useState({ maximized: false, fullscreen: false })

  useEffect(() => {
    if (!isWindows && !isMac) return
    const win = getCurrentWindow()
    let alive = true
    const sync = async () => {
      const [maximized, fullscreen] = await Promise.all([win.isMaximized(), win.isFullscreen()])
      if (alive) setFrame((f) => (f.maximized === maximized && f.fullscreen === fullscreen ? f : { maximized, fullscreen }))
    }
    void sync()
    const unlisten = win.onResized(() => void sync())
    return () => {
      alive = false
      void unlisten.then((f) => f())
    }
  }, [])

  return frame
}
