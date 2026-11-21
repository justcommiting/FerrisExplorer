import { useState } from 'react'
import { ArrowLeft, ArrowLeftRight, ArrowRight, Loader2 } from 'lucide-react'

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog'
import type { CompareBulk, CompareEntry, CompareStat, CompareStatus, ComparePick } from '@/lib/api'
import { basename, formatFullDate, formatSize } from '@/lib/format'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

const PAGE_SIZE = 300

const STATUS_LABEL: Record<CompareStatus, string> = {
  only_left: 'Only on the left',
  only_right: 'Only on the right',
  identical: 'Identical',
  newer_left: 'Left is newer',
  newer_right: 'Right is newer',
  same_time_size_differs: 'Same time, different size',
  content_differs: 'Contents differ',
  type_conflict: 'File on one side, folder on the other',
}

const PICK_LABEL: Record<ComparePick, string> = {
  copy_right: 'Copy to right',
  copy_left: 'Copy to left',
  delete_right: 'Remove from right',
  delete_left: 'Remove from left',
}

const BULK: { label: string; bulk: CompareBulk; title: string }[] = [
  { label: 'Mirror left → right', bulk: 'mirror_left_right', title: 'Make right match left, including removing what only the right has' },
  { label: 'Mirror right → left', bulk: 'mirror_right_left', title: 'Make left match right, including removing what only the left has' },
  { label: 'Update right', bulk: 'update_right', title: 'Copy new and newer files to the right. Removes nothing.' },
  { label: 'Update left', bulk: 'update_left', title: 'Copy new and newer files to the left. Removes nothing.' },
]

function StatCell({ stat }: { stat: CompareStat | null }) {
  if (!stat) return <span className="w-44 shrink-0 text-xs text-muted-foreground/60">Nothing here</span>
  return (
    <span className="w-44 shrink-0 truncate font-mono text-xs text-muted-foreground">
      {stat.kind === 'dir' ? 'Folder' : formatSize(stat.size)} · {formatFullDate(stat.modified_at)}
    </span>
  )
}

function EntryRow({
  entry,
  pick,
  onPick,
}: {
  entry: CompareEntry
  pick: ComparePick | undefined
  onPick: (pick: ComparePick | null) => void
}) {
  const name = basename(entry.rel)
  const canCopyRight = entry.status !== 'only_right' && entry.status !== 'identical'
  const canCopyLeft = entry.status !== 'only_left' && entry.status !== 'identical'
  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-t px-3 py-1.5 first:border-t-0">
      <div className="min-w-0 flex-1 basis-56">
        <div className={cn('truncate text-sm', entry.status === 'identical' && 'text-muted-foreground')} title={entry.rel}>
          {entry.rel}
        </div>
        <div className="text-[11px] text-muted-foreground">{STATUS_LABEL[entry.status]}</div>
      </div>
      <StatCell stat={entry.left} />
      <StatCell stat={entry.right} />
      <div className="flex shrink-0 items-center gap-1">
        {pick && <span className="mr-1 text-xs font-medium text-primary">{PICK_LABEL[pick]}</span>}
        {canCopyLeft && (
          <Button
            variant="outline"
            size="icon-sm"
            aria-label={`Copy ${name} to the left`}
            aria-pressed={pick === 'copy_left'}
            title="Copy to left"
            onClick={() => onPick(pick === 'copy_left' ? null : 'copy_left')}
          >
            <ArrowLeft />
          </Button>
        )}
        {canCopyRight && (
          <Button
            variant="outline"
            size="icon-sm"
            aria-label={`Copy ${name} to the right`}
            aria-pressed={pick === 'copy_right'}
            title="Copy to right"
            onClick={() => onPick(pick === 'copy_right' ? null : 'copy_right')}
          >
            <ArrowRight />
          </Button>
        )}
        <Button variant="ghost" size="sm" aria-label={`Skip ${name}`} disabled={!pick} onClick={() => onPick(null)}>
          Skip
        </Button>
      </div>
    </li>
  )
}

export function CompareFolders() {
  const open = useExplorer((s) => s.compareOpen)
  const set = useExplorer((s) => s.set)
  const left = useExplorer((s) => s.compareLeft)
  const right = useExplorer((s) => s.compareRight)
  const hasSplit = useExplorer((s) => s.splitTabId !== null)
  const by = useExplorer((s) => s.compareBy)
  const status = useExplorer((s) => s.compareStatus)
  const progress = useExplorer((s) => s.compareProgress)
  const comparison = useExplorer((s) => s.comparison)
  const compareError = useExplorer((s) => s.compareError)
  const hideIdentical = useExplorer((s) => s.compareHideIdentical)
  const picks = useExplorer((s) => s.comparePicks)
  const plan = useExplorer((s) => s.comparePlan)
  const syncStatus = useExplorer((s) => s.syncStatus)
  const syncProgress = useExplorer((s) => s.syncProgress)
  const syncOutcome = useExplorer((s) => s.syncOutcome)
  const syncError = useExplorer((s) => s.syncError)
  const closeCompare = useExplorer((s) => s.closeCompare)
  const startCompare = useExplorer((s) => s.startCompare)
  const cancelCompare = useExplorer((s) => s.cancelCompare)
  const setPick = useExplorer((s) => s.setComparePick)
  const applyBulk = useExplorer((s) => s.applyCompareBulk)
  const review = useExplorer((s) => s.reviewCompareSync)
  const runSync = useExplorer((s) => s.runCompareSync)
  const cancelSync = useExplorer((s) => s.cancelCompareSync)

  const [shown, setShown] = useState(PAGE_SIZE)

  const entries = comparison?.entries ?? []
  const rows = entries.filter((e) => !hideIdentical || e.status !== 'identical')
  const counts = { onlyLeft: 0, onlyRight: 0, differ: 0, conflict: 0, identical: 0 }
  for (const e of entries) {
    if (e.status === 'only_left') counts.onlyLeft++
    else if (e.status === 'only_right') counts.onlyRight++
    else if (e.status === 'identical') counts.identical++
    else if (e.status === 'type_conflict') counts.conflict++
    else counts.differ++
  }
  const pickCount = Object.keys(picks).length
  const busy = status === 'running' || syncStatus === 'running'
  const progressText =
    progress?.phase === 'hash'
      ? `Comparing contents: ${progress.done.toLocaleString()} of ${progress.total.toLocaleString()} files`
      : `Scanning… ${(progress?.done ?? 0).toLocaleString()} items`

  return (
    <Dialog open={open} onOpenChange={(o) => !o && closeCompare()}>
      <DialogContent className="flex h-[86vh] max-w-none flex-col gap-0 p-0 sm:max-w-[min(1200px,95vw)]">
        <header className="flex flex-wrap items-center gap-x-6 gap-y-2 border-b px-5 py-3.5 pr-12">
          <div className="flex min-w-0 flex-col gap-1">
            <DialogTitle className="flex items-center gap-2 text-lg font-semibold">
              <ArrowLeftRight className="size-4 text-primary" /> Compare folders
            </DialogTitle>
            <DialogDescription className="truncate font-mono text-xs">
              {comparison ? `${comparison.left}  ⇄  ${comparison.right}` : `${left || '(no folder)'}  ⇄  ${right || '(choose a folder)'}`}
            </DialogDescription>
          </div>
          <div className="ml-auto flex items-center gap-2">
            <div role="group" aria-label="How to compare" className="flex rounded-md border border-input p-0.5 text-xs">
              {(
                [
                  ['metadata', 'Size and date'],
                  ['contents', 'Compare contents'],
                ] as const
              ).map(([value, label]) => (
                <button
                  key={value}
                  aria-pressed={by === value}
                  disabled={busy}
                  onClick={() => set({ compareBy: value })}
                  className={cn(
                    'h-6 rounded px-2.5 disabled:opacity-40',
                    by === value ? 'bg-accent text-foreground' : 'text-muted-foreground hover:text-foreground',
                  )}
                >
                  {label}
                </button>
              ))}
            </div>
            {status === 'running' ? (
              <Button variant="outline" size="sm" onClick={cancelCompare}>
                Cancel
              </Button>
            ) : (
              <Button
                size="sm"
                disabled={busy || !left || !right}
                onClick={() => {
                  setShown(PAGE_SIZE)
                  void startCompare()
                }}
              >
                {status === 'done' ? 'Compare again' : 'Compare'}
              </Button>
            )}
          </div>
        </header>

        <main className="flex min-h-0 flex-1 flex-col gap-3 overflow-hidden p-4">
          {compareError && <p className="m-0 text-sm text-destructive">{compareError}</p>}
          {!hasSplit && status !== 'running' && (
            <label className="flex items-center gap-2 text-sm">
              <span className="w-28 shrink-0 text-muted-foreground">Right folder</span>
              <input
                aria-label="Right folder"
                value={right}
                disabled={busy}
                placeholder="Path of the folder to compare with"
                onChange={(e) => set({ compareRight: e.target.value })}
                className="h-8 min-w-0 flex-1 rounded-md border border-input bg-field px-2 font-mono text-xs text-foreground"
              />
            </label>
          )}

          {status === 'idle' && !comparison && !compareError && (
            <p className="m-auto max-w-lg text-center text-sm text-muted-foreground">
              Compares the two folders recursively. Symlinks are skipped and listed. Choose an action for each difference, or
              use a bulk rule. Nothing changes on disk until you review the sync and run it.
            </p>
          )}

          {status === 'running' && (
            <div className="m-auto flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" /> {progressText}
            </div>
          )}

          {syncStatus === 'running' && (
            <div className="flex items-center gap-3 rounded-md border bg-card px-3 py-2 text-sm">
              <Loader2 className="size-4 animate-spin" />
              <span>
                Syncing {syncProgress ? `${syncProgress.done.toLocaleString()} of ${syncProgress.total.toLocaleString()}` : '…'}
              </span>
              <Button variant="outline" size="sm" className="ml-auto" onClick={cancelSync}>
                Stop
              </Button>
            </div>
          )}

          {syncStatus === 'done' && (
            <div className="rounded-md border bg-card px-3 py-2 text-sm">
              {syncError && <p className="m-0 text-destructive">{syncError}</p>}
              {syncOutcome && (
                <>
                  <p className="m-0">
                    {syncOutcome.cancelled ? 'Stopped. ' : ''}
                    <span className="font-semibold">{syncOutcome.copied}</span> copied ·{' '}
                    <span className="font-semibold">{syncOutcome.overwritten}</span> overwritten ·{' '}
                    <span className="font-semibold">{syncOutcome.removed}</span> removed to the Trash
                    {syncOutcome.not_run > 0 && ` · ${syncOutcome.not_run} not reached`}
                  </p>
                  <p className="m-0 text-xs text-muted-foreground">Undo reverses this sync from the Undo menu.</p>
                  {syncOutcome.failures.length > 0 && (
                    <ul className="m-0 mt-2 list-none space-y-0.5 p-0 font-mono text-xs text-destructive">
                      {syncOutcome.failures.map(([p, why]) => (
                        <li key={p} className="truncate" title={`${p}: ${why}`}>
                          Failed: {p}: {why}
                        </li>
                      ))}
                    </ul>
                  )}
                  {syncOutcome.skipped.length > 0 && (
                    <ul className="m-0 mt-2 list-none space-y-0.5 p-0 font-mono text-xs text-muted-foreground">
                      {syncOutcome.skipped.map(([p, why]) => (
                        <li key={p} className="truncate" title={`${p}: ${why}`}>
                          Left alone: {p}: {why}
                        </li>
                      ))}
                    </ul>
                  )}
                </>
              )}
            </div>
          )}

          {comparison && status === 'done' && (
            <>
              <div className="flex flex-wrap items-center gap-x-3 gap-y-2 text-sm">
                <span>
                  <span className="font-semibold">{counts.onlyLeft.toLocaleString()}</span> only left ·{' '}
                  <span className="font-semibold">{counts.onlyRight.toLocaleString()}</span> only right ·{' '}
                  <span className="font-semibold">{counts.differ.toLocaleString()}</span> differ ·{' '}
                  <span className="font-semibold">{counts.conflict.toLocaleString()}</span> type conflicts ·{' '}
                  <span className="text-muted-foreground">{counts.identical.toLocaleString()} identical</span>
                </span>
                <label className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground">
                  <input
                    type="checkbox"
                    checked={hideIdentical}
                    onChange={(e) => set({ compareHideIdentical: e.target.checked })}
                    className="size-3.5 accent-primary"
                  />
                  Hide identical
                </label>
              </div>
              <div className="flex flex-wrap items-center gap-1.5">
                {BULK.map(({ label, bulk, title }) => (
                  <Button key={bulk} variant="outline" size="sm" title={title} disabled={busy} onClick={() => void applyBulk(bulk)}>
                    {label}
                  </Button>
                ))}
                <Button variant="ghost" size="sm" disabled={pickCount === 0} onClick={() => set({ comparePicks: {}, comparePlan: null })}>
                  Clear
                </Button>
              </div>
              {rows.length === 0 ? (
                <p className="m-auto text-sm text-muted-foreground">
                  {entries.length === 0 ? 'Both folders are empty.' : 'Nothing differs.'}
                </p>
              ) : (
                <ul className="m-0 flex min-h-0 flex-1 list-none flex-col overflow-y-auto rounded-lg border bg-card p-0">
                  {rows.slice(0, shown).map((e) => (
                    <EntryRow key={e.rel} entry={e} pick={picks[e.rel]} onPick={(pick) => setPick(e.rel, pick)} />
                  ))}
                  {shown < rows.length && (
                    <li className="flex justify-center border-t p-2">
                      <Button variant="outline" size="sm" onClick={() => setShown((n) => n + PAGE_SIZE)}>
                        Show {Math.min(PAGE_SIZE, rows.length - shown).toLocaleString()} more
                      </Button>
                    </li>
                  )}
                </ul>
              )}
              {comparison.skipped.length > 0 && (
                <details className="text-xs text-muted-foreground">
                  <summary className="cursor-pointer">
                    {comparison.skipped.length.toLocaleString()} skipped (symlinks and unreadable items)
                  </summary>
                  <ul className="m-0 mt-1 list-none space-y-0.5 p-0 font-mono">
                    {comparison.skipped.slice(0, 500).map((s) => (
                      <li key={`${s.side}:${s.rel}`} className="truncate" title={s.reason}>
                        {s.side} · {s.rel} — {s.reason}
                      </li>
                    ))}
                  </ul>
                </details>
              )}
            </>
          )}
        </main>

        <footer className="flex items-center gap-3 border-t px-5 py-3">
          <span className="text-sm text-muted-foreground">
            {pickCount.toLocaleString()} action{pickCount === 1 ? '' : 's'} chosen
          </span>
          <Button
            size="sm"
            className="ml-auto"
            disabled={status !== 'done' || pickCount === 0 || busy}
            onClick={() => void review()}
          >
            Review sync…
          </Button>
        </footer>
      </DialogContent>

      <AlertDialog open={plan !== null} onOpenChange={(o) => !o && set({ comparePlan: null })}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Run this sync?</AlertDialogTitle>
            <AlertDialogDescription>
              {plan?.copies.toLocaleString()} {plan?.copies === 1 ? 'copy' : 'copies'} ·{' '}
              {plan?.overwrites.toLocaleString()} {plan?.overwrites === 1 ? 'overwrite' : 'overwrites'} ·{' '}
              {plan?.deletions.toLocaleString()} {plan?.deletions === 1 ? 'deletion' : 'deletions'}.{' '}
              {formatSize(plan?.bytes_written ?? 0)} written, {formatSize(plan?.bytes_trashed ?? 0)} moved to the Trash.
              Replaced and removed items go to the Trash. Undo removes the copies and restores trashed items, except on macOS,
              where trashed items are put back from Finder&apos;s Trash.
            </AlertDialogDescription>
          </AlertDialogHeader>
          {plan && plan.skipped.length > 0 && (
            <ul className="m-0 max-h-40 list-none space-y-0.5 overflow-y-auto p-0 font-mono text-xs text-muted-foreground">
              {plan.skipped.slice(0, 200).map(([p, why]) => (
                <li key={p} className="truncate" title={`${p}: ${why}`}>
                  Not included: {p}: {why}
                </li>
              ))}
            </ul>
          )}
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction onClick={() => void runSync()}>Run sync</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Dialog>
  )
}
