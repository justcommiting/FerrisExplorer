import { AlertTriangle, GitBranch, ListChecks, X } from 'lucide-react'

import { api } from '@/lib/api'
import { formatSize } from '@/lib/format'
import { cn } from '@/lib/utils'
import { CommitDialog, DiscardDialog } from '@/components/CommitDialog'
import { useExplorer } from '@/store/explorer'

const VERB: Record<string, string> = {
  copy: 'Copying',
  move: 'Moving',
  delete: 'Deleting',
  duplicate: 'Duplicating',
  compress: 'Compressing',
  extract: 'Extracting',
}

export function StatusBar() {
  const tab = useExplorer((s) => s.activeTab())
  const ops = useExplorer((s) => s.ops)
  const showHidden = useExplorer((s) => s.showHidden)
  const index = useExplorer((s) => s.index)
  const folderTotal = useExplorer((s) => s.dirSizes[s.activeTab().dir])
  const set = useExplorer((s) => s.set)
  const opsMeta = useExplorer((s) => s.opsMeta)
  const allOps = Object.values(ops)
  const running = allOps.filter((o) => !o.done)
  const failed = allOps.filter((o) => o.done && o.error && o.error !== 'cancelled').length

  const searching = tab.activeQuery !== null
  const pool = searching ? tab.results : tab.items.filter((n) => showHidden || !n.hidden)
  const selectedNodes = pool.filter((n) => tab.selected.includes(n.path))
  const selectedBytes = selectedNodes.filter((n) => !n.is_dir).reduce((a, n) => a + n.size, 0)
  const changed = tab.git ? Object.values(tab.git.marks).filter((m) => m !== 'I').length : 0

  const indexState = index.indexing || index.syncing ? 'busy' : index.exists ? 'live' : 'none'

  return (
    <footer className="flex min-h-7.5 shrink-0 flex-wrap items-center gap-x-5 gap-y-1 border-t bg-chrome px-3 py-1 text-xs text-muted-foreground">
      <span>
        {pool.length.toLocaleString()} {searching ? 'result' : 'item'}
        {pool.length === 1 ? '' : 's'}
        {searching && tab.searching && ' so far…'}
        {!searching && typeof folderTotal === 'number' && ` · ${formatSize(folderTotal)}`}
        {selectedNodes.length > 0 && (
          <>
            {' · '}
            {selectedNodes.length} selected
            {selectedBytes > 0 && ` (${formatSize(selectedBytes)})`}
          </>
        )}
        {showHidden && ' · hidden shown'}
      </span>

      {tab.git && (
        <span className="flex items-center gap-1.5 font-mono text-git-modified" title={tab.git.root}>
          <GitBranch className="size-3" />
          {tab.git.branch}
          {(tab.git.ahead > 0 || tab.git.behind > 0) && (
            <span title="Commits ahead of and behind the upstream branch">
              {tab.git.ahead > 0 && ` ↑${tab.git.ahead}`}
              {tab.git.behind > 0 && ` ↓${tab.git.behind}`}
            </span>
          )}
          {changed > 0 && ` · ${changed} changed`}
        </span>
      )}

      {running.map((op) => {
        const pct = Math.round(
          op.bytes_total ? (op.bytes_done / op.bytes_total) * 100 : op.files_total ? (op.files_done / op.files_total) * 100 : 0,
        )
        return (
          <span key={op.id} className="flex min-w-0 flex-[1_1_260px] items-center gap-2.5">
            <button onClick={() => set({ opsPanelOpen: true })} className="truncate whitespace-nowrap text-foreground hover:underline">
              {op.conflict ? (
                <span className="flex items-center gap-1 text-git-modified"><AlertTriangle className="size-3" /> Waiting: name clash</span>
              ) : (
                opsMeta[op.id]?.title ?? `${VERB[op.kind] ?? 'Working on'} ${op.files_total} file${op.files_total === 1 ? '' : 's'}`
              )}
            </button>
            <span
              role="progressbar"
              aria-valuenow={pct}
              aria-valuemin={0}
              aria-valuemax={100}
              className="h-1 max-w-50 flex-1 overflow-hidden rounded-sm bg-border"
            >
              <span className="block h-full bg-primary transition-[width]" style={{ width: `${pct}%` }} />
            </span>
            <span className="font-mono">{pct}%</span>
            {op.bytes_per_sec > 0 && <span className="font-mono">{formatSize(op.bytes_per_sec)}/s</span>}
            <button aria-label="Cancel" onClick={() => void api.cancelOp(op.id)} className="grid size-5.5 place-items-center rounded hover:bg-accent hover:text-foreground">
              <X className="size-3" />
            </button>
          </span>
        )
      })}

      {allOps.length > 0 && (
        <button
          onClick={() => set({ opsPanelOpen: true })}
          className="flex items-center gap-1.5 rounded px-1.5 py-0.5 hover:bg-accent hover:text-foreground"
          title="Operations"
        >
          <ListChecks className="size-3.5" />
          Operations
          {failed > 0 && <span className="rounded bg-destructive/20 px-1 font-mono text-[10px] text-destructive">{failed} failed</span>}
        </button>
      )}

      <span className="ml-auto flex items-center gap-2">
        <span
          aria-hidden
          className={cn(
            'size-1.75 rounded-full',
            indexState === 'live' && 'bg-chart-3',
            indexState === 'busy' && 'animate-pulse bg-primary',
            indexState === 'none' && 'bg-dim',
          )}
        />
        <span title={index.message}>
          {indexState === 'live' && (index.message.includes('live') ? 'Index live · watching' : index.message || 'Index ready')}
          {indexState === 'busy' && index.message}
          {indexState === 'none' && (
            <>
              Not indexed ·{' '}
              <button onClick={() => void api.rebuildIndex()} className="text-primary hover:underline">
                Build index
              </button>
            </>
          )}
        </span>
      </span>
      {/* The git dialogs portal to the page, so they sit here: this bar is always mounted. */}
      <CommitDialog />
      <DiscardDialog />
    </footer>
  )
}
