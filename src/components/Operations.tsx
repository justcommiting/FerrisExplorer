import { useState } from 'react'
import { AlertTriangle, Archive, Check, Copy, CopyPlus, MoveRight, PackageOpen, Trash2, X } from 'lucide-react'
import { toast } from 'sonner'

import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { api, errorText, type OpProgress, type Resolution } from '@/lib/api'
import { basename, formatFullDate, formatSize, parentOf } from '@/lib/format'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

const GLYPH = { copy: Copy, move: MoveRight, delete: Trash2, duplicate: CopyPlus, compress: Archive, extract: PackageOpen }

function opPercent(op: OpProgress): number {
  if (op.done && !op.error) return 100
  const f = op.bytes_total ? op.bytes_done / op.bytes_total : op.files_total ? op.files_done / op.files_total : 0
  return Math.round(Math.min(1, f) * 100)
}

type Status = 'running' | 'waiting' | 'done' | 'failed' | 'cancelled'

function statusOf(op: OpProgress): Status {
  if (!op.done) return op.conflict ? 'waiting' : 'running'
  if (op.error === 'cancelled') return 'cancelled'
  return op.error ? 'failed' : 'done'
}

export function OperationsPanel() {
  const open = useExplorer((s) => s.opsPanelOpen)
  const ops = useExplorer((s) => s.ops)
  const meta = useExplorer((s) => s.opsMeta)
  const set = useExplorer((s) => s.set)
  const clearFinishedOps = useExplorer((s) => s.clearFinishedOps)

  const list = Object.values(ops).sort((a, b) => b.id - a.id)
  const counts = { running: 0, waiting: 0, finished: 0 }
  for (const op of list) {
    const st = statusOf(op)
    if (st === 'running') counts.running++
    else if (st === 'waiting') counts.waiting++
    else counts.finished++
  }
  const summary = [
    counts.running && `${counts.running} running`,
    counts.waiting && `${counts.waiting} needs you`,
    counts.finished && `${counts.finished} finished`,
  ]
    .filter(Boolean)
    .join(' · ')

  return (
    <Dialog open={open} onOpenChange={(o) => set({ opsPanelOpen: o })}>
      <DialogContent className="gap-0 p-0 sm:max-w-2xl" showCloseButton={false}>
        <DialogHeader className="flex-row items-center gap-3 border-b px-5 py-4">
          <DialogTitle className="text-lg font-semibold">Operations</DialogTitle>
          <DialogDescription className="text-xs">{summary || 'Nothing has run yet'}</DialogDescription>
          <button
            onClick={clearFinishedOps}
            disabled={counts.finished === 0}
            className="ml-auto h-7 rounded-md border border-input bg-accent px-2.5 text-xs hover:bg-[#2c2c29] disabled:opacity-40"
          >
            Clear finished
          </button>
        </DialogHeader>
        <ul className="m-0 max-h-[60vh] list-none overflow-y-auto p-0">
          {list.length === 0 && (
            <li className="px-5 py-10 text-center text-sm text-muted-foreground">
              Copies, moves, duplicates and compressions show up here.
            </li>
          )}
          {list.map((op) => {
            const st = statusOf(op)
            const pct = opPercent(op)
            const Glyph = GLYPH[op.kind] ?? Copy
            return (
              <li key={op.id} className="flex items-center gap-3.5 border-b px-5 py-3 last:border-b-0">
                <div className="grid size-9 shrink-0 place-items-center rounded-lg bg-accent">
                  <Glyph className="size-4 text-muted-foreground" strokeWidth={1.6} />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="flex items-baseline gap-2">
                    <span className="truncate font-medium">{meta[op.id]?.title ?? op.message}</span>
                    <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
                      {op.files_done}/{op.files_total}
                      {op.skipped > 0 && ` · ${op.skipped} skipped`}
                    </span>
                  </div>
                  <div className="mt-1.5 flex items-center gap-2.5">
                    <div
                      role="progressbar"
                      aria-valuenow={pct}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      className="h-1 flex-1 overflow-hidden rounded-sm bg-border"
                    >
                      <div
                        className={cn(
                          'h-full transition-[width]',
                          st === 'failed' ? 'bg-destructive' : st === 'waiting' ? 'bg-git-modified' : st === 'done' ? 'bg-chart-3' : st === 'cancelled' ? 'bg-dim' : 'bg-primary',
                        )}
                        style={{ width: `${pct}%` }}
                      />
                    </div>
                    <span
                      className={cn(
                        'w-36 shrink-0 truncate text-right text-xs',
                        st === 'failed' && 'text-destructive',
                        st === 'waiting' && 'text-git-modified',
                        st === 'done' && 'text-chart-3',
                        (st === 'running' || st === 'cancelled') && 'text-muted-foreground',
                      )}
                      title={op.error ?? undefined}
                    >
                      {st === 'running' && `${pct}%${op.bytes_per_sec ? ` · ${formatSize(op.bytes_per_sec)}/s` : ''}`}
                      {st === 'waiting' && 'Waiting for you'}
                      {st === 'done' && op.message}
                      {st === 'failed' && (op.error ?? 'Failed')}
                      {st === 'cancelled' && 'Cancelled'}
                    </span>
                  </div>
                </div>
                {!op.done ? (
                  <button
                    onClick={() => void api.cancelOp(op.id)}
                    className="h-7 shrink-0 rounded-md border border-input bg-accent px-2.5 text-xs hover:bg-[#2c2c29]"
                  >
                    Cancel
                  </button>
                ) : (
                  <span className="grid size-7 shrink-0 place-items-center">
                    {st === 'done' ? <Check className="size-4 text-chart-3" /> : <X className="size-4 text-muted-foreground" />}
                  </span>
                )}
              </li>
            )
          })}
        </ul>
      </DialogContent>
    </Dialog>
  )
}

function Side({ label, size, modified, isDir, accent }: { label: string; size: number; modified: number; isDir?: boolean; accent?: boolean }) {
  return (
    <div className={cn('rounded-md border p-3 text-xs', accent ? 'border-primary/50 bg-primary/8' : 'bg-field')}>
      <div className="mb-1 text-[11px] tracking-[.08em] text-muted-foreground uppercase">{label}</div>
      <div className="font-mono">{isDir ? 'Folder' : formatSize(size)}</div>
      <div className="text-muted-foreground">{formatFullDate(modified)}</div>
    </div>
  )
}

/** Shown while any transfer is paused on a name that already exists. */
export function ConflictDialog() {
  const op = useExplorer((s) => Object.values(s.ops).find((o) => o.conflict && !o.done && o.id in s.opsMeta) ?? null)
  const [applyAll, setApplyAll] = useState(false)
  const [busy, setBusy] = useState(false)
  const c = op?.conflict

  const answer = async (resolution: Resolution) => {
    if (!op) return
    setBusy(true)
    try {
      await api.resolveConflict(op.id, resolution, applyAll && resolution !== 'cancel')
    } catch (e) {
      toast.error('Could not answer', { description: errorText(e) })
    }
    setBusy(false)
    setApplyAll(false)
  }

  const verb = op?.kind === 'move' ? 'Move' : 'Copy'
  return (
    <Dialog open={!!c} onOpenChange={(o) => !o && void answer('cancel')}>
      <DialogContent className="sm:max-w-md" showCloseButton={false}>
        {c && (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2 text-sm">
                <AlertTriangle className="size-4 text-git-modified" />
                {verb} paused: name already exists
              </DialogTitle>
              <DialogDescription className="text-xs leading-relaxed">
                {verb === 'Move' ? 'Moves' : 'Copies'} never overwrite.{' '}
                <code className="rounded bg-field px-1 font-mono text-foreground">{basename(c.dest)}</code> is already in{' '}
                <span className="font-mono">{basename(parentOf(c.dest) ?? '') || parentOf(c.dest)}</span>.
              </DialogDescription>
            </DialogHeader>
            <div className="grid grid-cols-2 gap-2.5">
              <Side label={verb === 'Move' ? 'Moving' : 'Copying'} size={c.src_size} modified={c.src_modified} isDir={c.src_is_dir} accent />
              <Side label="Already there" size={c.dest_size} modified={c.dest_modified} />
            </div>
            {c.remaining > 0 && (
              <label className="flex items-center gap-2 text-xs text-muted-foreground">
                <input type="checkbox" checked={applyAll} onChange={(e) => setApplyAll(e.target.checked)} className="size-3.5 accent-[var(--primary)]" />
                Do the same for any other clashes ({c.remaining} item{c.remaining === 1 ? '' : 's'} left)
              </label>
            )}
            <div className="flex flex-wrap gap-2">
              <button
                autoFocus
                disabled={busy}
                onClick={() => void answer('keep_both')}
                className="h-8 rounded-md bg-primary px-3.5 text-[13px] font-semibold text-primary-foreground hover:bg-primary/90"
              >
                Keep both
              </button>
              <button
                disabled={busy}
                onClick={() => void answer('skip')}
                className="h-8 rounded-md border border-input bg-accent px-3 text-[13px] hover:bg-[#2c2c29]"
              >
                Skip this {c.src_is_dir ? 'folder' : 'file'}
              </button>
              <button
                disabled={busy}
                onClick={() => void answer('cancel')}
                className="ml-auto h-8 rounded-md px-3 text-[13px] text-muted-foreground hover:bg-accent hover:text-foreground"
              >
                Cancel {verb.toLowerCase()}
              </button>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  )
}
