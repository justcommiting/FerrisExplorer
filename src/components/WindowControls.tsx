import { getCurrentWindow } from '@tauri-apps/api/window'

import { isWindows } from '@/lib/platform'
import { cn } from '@/lib/utils'

/** Code points in the system icon font (Segoe Fluent / MDL2), so the buttons match native ones. */
const GLYPH = { minimize: 0xe921, maximize: 0xe922, restore: 0xe923, close: 0xe8bb }

/** Minimize / maximize / close for the undecorated Windows frame. */
export function WindowControls({ maximized }: { maximized: boolean }) {
  if (!isWindows) return null
  const win = getCurrentWindow()
  const button =
    'grid h-full w-[46px] place-items-center text-[10px] text-muted-foreground outline-none transition-colors duration-100 hover:text-foreground'

  return (
    <div className="flex h-full shrink-0 font-['Segoe_Fluent_Icons','Segoe_MDL2_Assets']">
      <button aria-label="Minimize" tabIndex={-1} className={cn(button, 'hover:bg-white/8')} onClick={() => void win.minimize()}>
        {String.fromCharCode(GLYPH.minimize)}
      </button>
      <button
        aria-label={maximized ? 'Restore' : 'Maximize'}
        tabIndex={-1}
        className={cn(button, 'hover:bg-white/8')}
        onClick={() => void win.toggleMaximize()}
      >
        {String.fromCharCode(maximized ? GLYPH.restore : GLYPH.maximize)}
      </button>
      <button
        aria-label="Close"
        tabIndex={-1}
        className={cn(button, 'hover:bg-[#C42B1C] hover:text-white')}
        onClick={() => void win.close()}
      >
        {String.fromCharCode(GLYPH.close)}
      </button>
    </div>
  )
}
