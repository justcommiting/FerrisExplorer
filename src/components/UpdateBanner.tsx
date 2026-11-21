import { useEffect, useState } from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { Download, Loader2, X } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { api, errorText, type UpdateInfo } from '@/lib/api'

/** Runs after startup so the update check never competes with the first listing. */
const CHECK_DELAY_MS = 5000

/**
 * Offers a newer release from GitHub Releases. Nothing installs until the user clicks
 * "Update and restart". Only the main window checks; explorer windows render nothing.
 */
export function UpdateBanner() {
  const [update, setUpdate] = useState<UpdateInfo | null>(null)
  const [dismissed, setDismissed] = useState(false)
  const [installing, setInstalling] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let isMain = false
    try {
      isMain = getCurrentWindow().label === 'main'
    } catch {
      return
    }
    if (!isMain) return

    let cancelled = false
    const timer = window.setTimeout(() => {
      api
        .checkForUpdate()
        .then((info) => {
          if (!cancelled) setUpdate(info)
        })
        .catch((e) => console.warn('Update check failed', e))
    }, CHECK_DELAY_MS)
    return () => {
      cancelled = true
      window.clearTimeout(timer)
    }
  }, [])

  if (!update || dismissed) return null

  const install = () => {
    setInstalling(true)
    setError(null)
    // On success the app restarts, so this promise never resolves.
    api.installUpdate(update.version).catch((e) => {
      setError(errorText(e))
      setInstalling(false)
    })
  }

  return (
    <div role="status" className="flex items-center gap-2 border-b border-border bg-muted px-3 py-1.5 text-sm">
      <Download className="size-4 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate">FerrisExplorer {update.version} is available</span>
      {error && (
        <span className="max-w-[40%] truncate text-destructive" title={error}>
          Update failed: {error}
        </span>
      )}
      <Button size="sm" onClick={install} disabled={installing}>
        {installing ? (
          <>
            <Loader2 className="animate-spin" />
            Updating…
          </>
        ) : (
          'Update and restart'
        )}
      </Button>
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label="Dismiss update notice"
        onClick={() => setDismissed(true)}
        disabled={installing}
      >
        <X />
      </Button>
    </div>
  )
}
