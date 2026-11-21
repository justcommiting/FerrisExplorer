import { useEffect, useState } from 'react'
import { Loader2 } from 'lucide-react'

import { DetailsPanel } from '@/components/DetailsPanel'
import { CompareFolders } from '@/components/CompareFolders'
import { DuplicateFinder } from '@/components/DuplicateFinder'
import { FileList } from '@/components/FileList'
import { BulkRenameDialog, CommandPalette, DeleteDialog, PromptDialog, QuickOpen } from '@/components/Overlays'
import { ConflictDialog, OperationsPanel } from '@/components/Operations'
import { ShellPanel, ShellRunDialog } from '@/components/ShellPanel'
import { SpaceUsage } from '@/components/SpaceUsage'
import { Sidebar, WorkspaceDialogs } from '@/components/Sidebar'
import { StatusBar } from '@/components/StatusBar'
import { TabBar } from '@/components/TabBar'
import { UpdateBanner } from '@/components/UpdateBanner'
import { Toolbar } from '@/components/Toolbar'
import { Toaster } from '@/components/ui/sonner'
import { TooltipProvider } from '@/components/ui/tooltip'
import { useBackendEvents } from '@/hooks/useBackendEvents'
import { useFileDrop } from '@/hooks/useFileDrop'
import { useShortcuts } from '@/hooks/useShortcuts'
import { errorText } from '@/lib/api'
import { useExplorer } from '@/store/explorer'

/** The one startup run. StrictMode mounts twice, and a second init would overwrite the first one's tabs. */
let starting: Promise<void> | null = null

export default function App() {
  const ready = useExplorer((s) => s.ready)
  const showDetails = useExplorer((s) => s.showDetails)
  const activeTabId = useExplorer((s) => s.activeTabId)
  const splitTabId = useExplorer((s) => s.splitTabId)
  const [failed, setFailed] = useState<string | null>(null)

  useBackendEvents()
  useShortcuts()
  useFileDrop()

  useEffect(() => {
    starting ??= useExplorer.getState().init()
    starting.catch((e) => setFailed(errorText(e)))
  }, [])

  if (!ready) {
    return (
      <div className="grid h-full place-items-center bg-background text-sm text-muted-foreground">
        {failed ? `FerrisExplorer could not start: ${failed}` : <Loader2 className="size-5 animate-spin" />}
      </div>
    )
  }

  return (
    <TooltipProvider delay={500}>
      <div className="flex h-full flex-col bg-background text-foreground">
        <UpdateBanner />
        <TabBar />
        <Toolbar />
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <div className="flex min-w-0 flex-1 gap-px bg-border">
            <FileList tabId={activeTabId} pane="primary" />
            {splitTabId !== null && <FileList tabId={splitTabId} pane="split" />}
          </div>
          {showDetails && <DetailsPanel />}
        </div>
        <ShellPanel />
        <StatusBar />
      </div>
      <CommandPalette />
      <QuickOpen />
      <PromptDialog />
      <BulkRenameDialog />
      <DeleteDialog />
      <SpaceUsage />
      <DuplicateFinder />
      <CompareFolders />
      <ShellRunDialog />
      <WorkspaceDialogs />
      <OperationsPanel />
      <ConflictDialog />
      <Toaster position="bottom-right" />
    </TooltipProvider>
  )
}
