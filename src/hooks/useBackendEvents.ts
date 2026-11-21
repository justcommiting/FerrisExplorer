import { useEffect } from 'react'

import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

import { events } from '@/lib/api'
import { useExplorer } from '@/store/explorer'

/** Subscribe the store to everything the Rust side pushes. */
export function useBackendEvents() {
  useEffect(() => {
    const st = useExplorer.getState
    const subs = [
      events.onSearch((e) => st().onSearchEvent(e)),
      events.onOpProgress((p) => st().onOpProgress(p)),
      events.onDirSize((e) => st().onDirSize(e.path, e.size)),
      events.onDirChanged((d) => st().onDirChanged(d)),
      events.onIndexChanged(() => st().onIndexChanged()),
      events.onIndexStatus((s) => st().setIndex(s)),
      events.onDuplicatesProgress((p) => st().onDuplicatesProgress(p)),
      events.onDuplicatesDone((d) => st().onDuplicatesDone(d)),
      events.onCompareProgress((p) => st().onCompareProgress(p)),
      events.onCompareDone((d) => st().onCompareDone(d)),
      events.onSyncProgress((p) => st().onSyncProgress(p)),
      events.onSyncDone((d) => st().onSyncDone(d)),
      // Sent only to this window, when a tab is dragged onto it from another.
      getCurrentWebviewWindow().listen<string>('tab-received', (e) => st().newTab(e.payload)),
    ]
    return () => {
      for (const p of subs) void p.then((un) => un())
    }
  }, [])
}
