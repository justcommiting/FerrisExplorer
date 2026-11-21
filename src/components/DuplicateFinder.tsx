import { useState } from 'react'
import { ExternalLink, Files, FolderOpen, Loader2, Trash2 } from 'lucide-react'
import { toast } from 'sonner'

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
import { api, errorText, type DupGroup } from '@/lib/api'
import {
  folderChoices,
  selectionForScan,
  selectionTotals,
  trashFor,
  withGroupSelection,
  type KeepRule,
} from '@/lib/duplicates'
import { basename, formatFullDate, formatSize, parentOf } from '@/lib/format'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

const PAGE_SIZE = 200
/** Most folders offered in a "keep the one in" picker. */
const FOLDER_LIMIT = 300

const RULES: { label: string; short: string; rule: KeepRule }[] = [
  { label: 'Keep newest', short: 'Newest', rule: { kind: 'newest' } },
  { label: 'Keep oldest', short: 'Oldest', rule: { kind: 'oldest' } },
  { label: 'Keep shortest path', short: 'Shortest path', rule: { kind: 'shortest' } },
]

const SCOPES = [
  { value: 'folder', label: 'This folder and below' },
  { value: 'index', label: 'Whole index' },
] as const

/** A folder picker that keeps the copy in the chosen folder and trashes the rest. */
function KeepInFolder({ groups, onPick }: { groups: DupGroup[]; onPick: (folder: string) => void }) {
  const folders = folderChoices(groups).slice(0, FOLDER_LIMIT)
  return (
    <select
      aria-label="Keep the one in folder"
      value=""
      disabled={folders.length === 0}
      onChange={(e) => e.target.value && onPick(e.target.value)}
      className="h-7 max-w-56 rounded-md border border-input bg-field px-1.5 text-xs text-foreground"
    >
      <option value="">Keep the one in…</option>
      {folders.map((f) => (
        <option key={f} value={f}>
          {f || '(root)'}
        </option>
      ))}
    </select>
  )
}

function GroupCard({
  group,
  selected,
  onPick,
}: {
  group: DupGroup
  selected: Set<string>
  onPick: (next: Set<string>) => void
}) {
  return (
    <li className="overflow-hidden rounded-lg border bg-card">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b px-3 py-2 text-xs">
        <span className="text-sm font-semibold">{group.members.length} copies</span>
        <span className="font-mono text-muted-foreground">{formatSize(group.size)} each</span>
        <span className="text-primary">{formatSize(group.wasted)} reclaimable</span>
        <div className="ml-auto flex flex-wrap items-center gap-1">
          {RULES.map(({ short, rule }) => (
            <button
              key={short}
              onClick={() => onPick(withGroupSelection(selected, group, trashFor(group, rule)))}
              className="h-6 rounded px-2 text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              {short}
            </button>
          ))}
          <KeepInFolder
            groups={[group]}
            onPick={(folder) => onPick(withGroupSelection(selected, group, trashFor(group, { kind: 'folder', folder })))}
          />
        </div>
      </div>
      <ul className="m-0 flex list-none flex-col p-0">
        {group.members.map((m) => {
          const checked = selected.has(m.path)
          // Ticking this copy would leave the set with none: the store refuses it too.
          const lastKept = !checked && group.members.every((o) => o.path === m.path || selected.has(o.path))
          return (
            <li key={m.path} className="flex items-center gap-3 border-t px-3 py-1.5 first:border-t-0">
              <input
                type="checkbox"
                aria-label={`Move ${basename(m.path)} to the Trash`}
                checked={checked}
                disabled={lastKept}
                title={lastKept ? 'At least one copy must be kept' : undefined}
                onChange={(e) => {
                  const next = new Set(selected)
                  if (e.target.checked) next.add(m.path)
                  else next.delete(m.path)
                  onPick(next)
                }}
                className="size-4 shrink-0 accent-primary disabled:opacity-40"
              />
              <div className="min-w-0 flex-1">
                <div className={cn('truncate text-sm', checked && 'text-muted-foreground line-through')}>{basename(m.path)}</div>
                <div className="truncate font-mono text-[11px] text-muted-foreground">{parentOf(m.path)}</div>
              </div>
              <span className="hidden shrink-0 font-mono text-xs text-muted-foreground sm:block">{formatFullDate(m.modified_at)}</span>
              <span className="w-20 shrink-0 text-right font-mono text-xs">{formatSize(m.size)}</span>
              <button
                aria-label={`Reveal ${basename(m.path)}`}
                title="Show in folder"
                onClick={() => void api.revealPath(m.path).catch((e) => toast.error('Could not reveal', { description: errorText(e) }))}
                className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
              >
                <FolderOpen className="size-3.5" />
              </button>
              <button
                aria-label={`Open ${basename(m.path)}`}
                title="Open"
                onClick={() => void api.openPath(m.path).catch((e) => toast.error('Could not open', { description: errorText(e) }))}
                className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
              >
                <ExternalLink className="size-3.5" />
              </button>
            </li>
          )
        })}
      </ul>
    </li>
  )
}

export function DuplicateFinder() {
  const open = useExplorer((s) => s.duplicateFinderOpen)
  const set = useExplorer((s) => s.set)
  const scope = useExplorer((s) => s.dupScope)
  const root = useExplorer((s) => s.dupRoot)
  const here = useExplorer((s) => s.activeTab().dir)
  const status = useExplorer((s) => s.dupStatus)
  const progress = useExplorer((s) => s.dupProgress)
  const scan = useExplorer((s) => s.dupScan)
  const error = useExplorer((s) => s.dupError)
  const selected = useExplorer((s) => s.dupSelected)
  const indexed = useExplorer((s) => s.index.exists)
  const startScan = useExplorer((s) => s.startDuplicateScan)
  const cancelScan = useExplorer((s) => s.cancelDuplicateScan)
  const closeFinder = useExplorer((s) => s.closeDuplicateFinder)
  const setSelection = useExplorer((s) => s.setDupSelection)
  const trash = useExplorer((s) => s.trashDuplicates)

  const [shown, setShown] = useState(PAGE_SIZE)
  const [confirming, setConfirming] = useState(false)

  const totals = selectionTotals(scan, selected)
  const where = scope === 'index' ? 'Every file in the search index' : root || here
  const progressText = progress
    ? `${progress.phase === 'partial' ? 'Checking the start and end of' : 'Comparing the full contents of'} ${progress.done.toLocaleString()} of ${progress.total.toLocaleString()} files`
    : 'Collecting files…'

  return (
    <Dialog open={open} onOpenChange={(o) => !o && closeFinder()}>
      <DialogContent className="flex h-[86vh] max-w-none flex-col gap-0 p-0 sm:max-w-[min(1100px,94vw)]">
        <header className="flex flex-wrap items-center gap-x-6 gap-y-2 border-b px-5 py-3.5 pr-12">
          <div className="flex min-w-0 flex-col gap-1">
            <DialogTitle className="flex items-center gap-2 text-lg font-semibold">
              <Files className="size-4 text-primary" /> Find duplicates
            </DialogTitle>
            <DialogDescription className="truncate font-mono text-xs">{where}</DialogDescription>
          </div>
          <div className="ml-auto flex items-center gap-2">
            <div role="group" aria-label="Search scope" className="flex rounded-md border border-input p-0.5 text-xs">
              {SCOPES.map((s) => {
                const disabled = status === 'scanning' || (s.value === 'index' && !indexed)
                return (
                  <button
                    key={s.value}
                    aria-pressed={scope === s.value}
                    disabled={disabled}
                    title={s.value === 'index' && !indexed ? 'The search index is not built yet' : undefined}
                    onClick={() => set({ dupScope: s.value })}
                    className={cn(
                      'h-6 rounded px-2.5 disabled:opacity-40',
                      scope === s.value ? 'bg-accent text-foreground' : 'text-muted-foreground hover:text-foreground',
                    )}
                  >
                    {s.label}
                  </button>
                )
              })}
            </div>
            {status === 'scanning' ? (
              <Button variant="outline" size="sm" onClick={cancelScan}>
                Cancel
              </Button>
            ) : (
              <Button size="sm" onClick={() => { setShown(PAGE_SIZE); void startScan(scope) }}>
                {status === 'done' ? 'Rescan' : 'Scan'}
              </Button>
            )}
          </div>
        </header>

        <main className="flex min-h-0 flex-1 flex-col gap-3 overflow-hidden p-4">
          {error && <p className="m-0 text-sm text-destructive">{error}</p>}
          {status === 'idle' && !error && (
            <p className="m-auto max-w-md text-center text-sm text-muted-foreground">
              Files are grouped by size, then by a hash of their first and last 64 KB, then by a hash of the whole file. Only
              files with identical contents are reported. Symlinks and empty files are ignored.
            </p>
          )}
          {status === 'scanning' && (
            <div className="m-auto flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" /> {progressText}
            </div>
          )}
          {status === 'done' && scan && (
            <>
              <div className="flex flex-wrap items-center gap-3">
                <p className="m-0 text-sm">
                  <span className="font-semibold text-primary">{formatSize(scan.reclaimable)}</span> reclaimable in{' '}
                  {scan.groups.length.toLocaleString()} set{scan.groups.length === 1 ? '' : 's'} ·{' '}
                  {scan.files_checked.toLocaleString()} files checked
                </p>
                {scan.groups.length > 0 && (
                  <div className="ml-auto flex flex-wrap items-center gap-1.5">
                    {RULES.map(({ label, rule }) => (
                      <Button key={label} variant="outline" size="sm" onClick={() => setSelection(selectionForScan(scan, rule))}>
                        {label}
                      </Button>
                    ))}
                    <KeepInFolder groups={scan.groups} onPick={(folder) => setSelection(selectionForScan(scan, { kind: 'folder', folder }))} />
                    <Button variant="ghost" size="sm" onClick={() => setSelection(new Set())} disabled={selected.size === 0}>
                      Clear
                    </Button>
                  </div>
                )}
              </div>
              {scan.groups.length === 0 ? (
                <p className="m-auto text-sm text-muted-foreground">No duplicates found here.</p>
              ) : (
                <ul className="m-0 flex min-h-0 flex-1 list-none flex-col gap-3 overflow-y-auto p-0 pr-1">
                  {scan.groups.slice(0, shown).map((g) => (
                    <GroupCard key={g.members[0].path} group={g} selected={selected} onPick={(next) => setSelection(next)} />
                  ))}
                  {shown < scan.groups.length && (
                    <li className="flex justify-center pt-1">
                      <Button variant="outline" size="sm" onClick={() => setShown((n) => n + PAGE_SIZE)}>
                        Show {Math.min(PAGE_SIZE, scan.groups.length - shown).toLocaleString()} more sets
                      </Button>
                    </li>
                  )}
                </ul>
              )}
            </>
          )}
        </main>

        <footer className="flex items-center gap-3 border-t px-5 py-3">
          <span className="text-sm text-muted-foreground">
            {totals.count.toLocaleString()} selected · {formatSize(totals.bytes)} to free
          </span>
          <Button
            variant="destructive"
            size="sm"
            className="ml-auto"
            disabled={status !== 'done' || totals.count === 0}
            onClick={() => setConfirming(true)}
          >
            <Trash2 className="size-3.5" /> Move selected to Trash…
          </Button>
        </footer>
      </DialogContent>

      <AlertDialog open={confirming} onOpenChange={(o) => !o && setConfirming(false)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              Move {totals.count.toLocaleString()} cop{totals.count === 1 ? 'y' : 'ies'} to the Trash?
            </AlertDialogTitle>
            <AlertDialogDescription>
              {formatSize(totals.bytes)} will be freed. Every set keeps at least one copy. You can undo this from Undo.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                setConfirming(false)
                void trash()
              }}
            >
              Move to Trash
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Dialog>
  )
}
