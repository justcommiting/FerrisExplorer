import { useEffect, useMemo, useState } from 'react'
import {
  ChevronDown,
  ChevronRight,
  Download,
  FileText,
  Folder,
  Home,
  LayoutGrid,
  Monitor,
  PieChart,
  Search,
  Star,
  X,
} from 'lucide-react'

import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from '@/components/ui/context-menu'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { api, type DriveUsage, type SmartCount, type Workspace } from '@/lib/api'
import { basename, formatSize, sepOf } from '@/lib/format'
import { cn } from '@/lib/utils'
import { useExplorer, type WorkspaceDialog } from '@/store/explorer'

const DRIVE_COLORS = ['bg-primary', 'bg-chart-2', 'bg-chart-3', 'bg-chart-4']

const norm = (p: string) => p.replace(/[\\/]+$/, '').toLowerCase()

function Section({ title, children, className }: { title: string; children: React.ReactNode; className?: string }) {
  return (
    <div className={cn('flex flex-col gap-0.5', className)}>
      <div className="px-2.5 pb-1 text-[11px] tracking-[.08em] text-muted-foreground uppercase">{title}</div>
      {children}
    </div>
  )
}

function Item({
  icon,
  label,
  title,
  active,
  mono,
  badge,
  onClick,
  onRemove,
}: {
  icon?: React.ReactNode
  label: string
  title?: string
  active?: boolean
  mono?: boolean
  badge?: string
  onClick: () => void
  onRemove?: () => void
}) {
  return (
    <div
      role="button"
      tabIndex={-1}
      title={title}
      aria-current={active ? 'page' : undefined}
      onClick={onClick}
      className={cn(
        'group flex h-7.5 cursor-default items-center gap-2.5 rounded-md px-2.5 transition-colors',
        active ? 'bg-sidebar-accent shadow-[inset_2px_0_0_var(--primary)]' : 'hover:bg-accent',
        mono && 'font-mono text-xs',
      )}
    >
      {icon && <span className="shrink-0 text-muted-foreground">{icon}</span>}
      <span className="truncate">{label}</span>
      {badge && <span className="ml-auto font-mono text-[11px] text-muted-foreground">{badge}</span>}
      {onRemove && (
        <button
          aria-label={`Remove ${label}`}
          onClick={(e) => {
            e.stopPropagation()
            onRemove()
          }}
          className="ml-auto hidden rounded p-0.5 text-muted-foreground group-hover:block hover:text-foreground"
        >
          <X className="size-3" />
        </button>
      )}
    </div>
  )
}

/** Saved workspaces. Clicking one opens it; the list folds away with its header. */
function WorkspacesSection() {
  const workspaces = useExplorer((s) => s.workspaces)
  const current = useExplorer((s) => s.currentWorkspace)
  const requestOpenWorkspace = useExplorer((s) => s.requestOpenWorkspace)
  const [expanded, setExpanded] = useState(true)
  return (
    <div className="flex flex-col gap-0.5">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((e) => !e)}
        className="flex items-center gap-1 px-2.5 pb-1 text-[11px] tracking-[.08em] text-muted-foreground uppercase hover:text-foreground"
      >
        {expanded ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        Workspaces
      </button>
      {expanded && workspaces.length === 0 && (
        <p className="px-2.5 text-[11.5px] leading-snug text-dim">Save the open tabs from the command palette.</p>
      )}
      {expanded &&
        workspaces.map((w) => (
          <Item
            key={w.name}
            icon={<LayoutGrid className="size-4" strokeWidth={1.5} />}
            label={w.name}
            title={`${w.tabs.length} ${w.tabs.length === 1 ? 'tab' : 'tabs'}`}
            active={current === w.name}
            onClick={() => requestOpenWorkspace(w.name)}
          />
        ))}
    </div>
  )
}

export function Sidebar() {
  const home = useExplorer((s) => s.home)
  const favorites = useExplorer((s) => s.favorites)
  const saved = useExplorer((s) => s.savedSearches)
  const dir = useExplorer((s) => s.activeTab().dir)
  const activeQuery = useExplorer((s) => s.activeTab().activeQuery)
  const navigate = useExplorer((s) => s.navigate)
  const runSearch = useExplorer((s) => s.runSearch)
  const removeFavorite = useExplorer((s) => s.removeFavorite)
  const removeSavedSearch = useExplorer((s) => s.removeSavedSearch)
  const setSmartPinned = useExplorer((s) => s.setSmartPinned)
  const openSmartFolder = useExplorer((s) => s.openSmartFolder)
  const activeSmart = useExplorer((s) => s.activeTab().smart)
  const smartTick = useExplorer((s) => s.smartTick)
  const set = useExplorer((s) => s.set)
  const [drives, setDrives] = useState<DriveUsage[]>([])
  const [counts, setCounts] = useState<Record<string, SmartCount | null>>({})
  const pinned = useMemo(() => saved.filter((s) => s.pinned), [saved])
  const pinnedQueries = useMemo(() => pinned.map((s) => s.query), [pinned])

  // Match counts come from the index, so they refresh with the open folders.
  useEffect(() => {
    let live = true
    void Promise.all(
      pinnedQueries.map((q) => api.countSmartFolder(q).catch(() => null).then((c) => [q, c] as const)),
    ).then((entries) => {
      if (live) setCounts(Object.fromEntries(entries))
    })
    return () => {
      live = false
    }
  }, [pinnedQueries, smartTick])

  useEffect(() => {
    const load = () => api.driveUsage().then(setDrives).catch(() => {})
    void load()
    const t = setInterval(load, 60_000)
    return () => clearInterval(t)
  }, [])

  const sep = sepOf(home)
  const places = [
    { label: 'Home', path: home, icon: <Home className="size-4" strokeWidth={1.5} /> },
    { label: 'Desktop', path: home + sep + 'Desktop', icon: <Monitor className="size-4" strokeWidth={1.5} /> },
    { label: 'Documents', path: home + sep + 'Documents', icon: <FileText className="size-4" strokeWidth={1.5} /> },
    { label: 'Downloads', path: home + sep + 'Downloads', icon: <Download className="size-4" strokeWidth={1.5} /> },
  ]
  const here = (p: string) => activeQuery === null && norm(dir) === norm(p)

  return (
    <nav
      aria-label="Places"
      className="flex w-56 shrink-0 flex-col gap-4.5 overflow-y-auto border-r bg-sidebar px-2 py-3"
      data-pin-zone
    >
      <Section title="Places">
        {places.map((p) => (
          <Item key={p.label} icon={p.icon} label={p.label} title={p.path} active={here(p.path)} onClick={() => void navigate(p.path)} />
        ))}
      </Section>

      <Section title="Favorites">
        {favorites.length === 0 && (
          <p className="px-2.5 text-[11.5px] leading-snug text-dim">Drag a folder here to pin it.</p>
        )}
        {favorites.map((f) => (
          <Item
            key={f}
            icon={here(f) ? <Star className="size-4 fill-primary/30 text-primary" strokeWidth={1.5} /> : <Folder className="size-4" strokeWidth={1.5} />}
            label={basename(f)}
            title={f}
            active={here(f)}
            onClick={() => void navigate(f)}
            onRemove={() => removeFavorite(f)}
          />
        ))}
      </Section>

      <WorkspacesSection />

      <Section title="Smart folders">
        {pinned.length === 0 && (
          <p className="px-2.5 text-[11.5px] leading-snug text-dim">Pin a saved search to keep it here.</p>
        )}
        {pinned.map((s) => {
          const c = counts[s.query]
          return (
            <ContextMenu key={s.name}>
              <ContextMenuTrigger render={<div />}>
                <Item
                  icon={<Search className="size-4" strokeWidth={1.5} />}
                  label={s.name}
                  title={s.query}
                  active={activeSmart?.name === s.name && activeSmart.query === s.query}
                  badge={c ? `${c.count}${c.capped ? '+' : ''}` : undefined}
                  onClick={() => void openSmartFolder(s)}
                />
              </ContextMenuTrigger>
              <ContextMenuContent className="w-44">
                <ContextMenuItem className="text-[12.5px]" onClick={() => set({ prompt: { kind: 'renameSmart', name: s.name, initial: s.name } })}>
                  Rename…
                </ContextMenuItem>
                <ContextMenuItem className="text-[12.5px]" onClick={() => set({ prompt: { kind: 'editSmartQuery', name: s.name, initial: s.query } })}>
                  Edit Query…
                </ContextMenuItem>
                <ContextMenuSeparator />
                <ContextMenuItem className="text-[12.5px]" onClick={() => setSmartPinned(s.name, false)}>Unpin</ContextMenuItem>
                <ContextMenuItem className="text-[12.5px] text-destructive" onClick={() => removeSavedSearch(s.name)}>
                  Delete
                </ContextMenuItem>
              </ContextMenuContent>
            </ContextMenu>
          )
        })}
      </Section>

      <Section title="Saved searches">
        {saved.map((s) => (
          <Item
            key={s.name}
            mono
            label={s.name}
            title={s.query}
            active={activeQuery === s.query}
            onClick={() => void runSearch(s.query)}
            onRemove={() => removeSavedSearch(s.name)}
          />
        ))}
      </Section>

      <Section title="Tools">
        <Item icon={<PieChart className="size-4" strokeWidth={1.5} />} label="Space usage" onClick={() => set({ spaceFinderOpen: true })} />
      </Section>

      <Section title="Devices" className="mt-auto gap-1">
        {drives.map((d, i) => {
          const used = d.total - d.free
          const pct = Math.round((used / d.total) * 100)
          return (
            <button
              key={d.path}
              onClick={() => void navigate(d.path)}
              title={`${formatSize(d.free)} free of ${formatSize(d.total)}`}
              className={cn('flex flex-col gap-1.5 rounded-md px-2.5 py-2 text-left hover:bg-accent', here(d.path) && 'bg-sidebar-accent')}
            >
              <span className="flex justify-between">
                <span>{d.path}</span>
                <span className={cn('font-mono text-xs', pct >= 90 ? 'text-destructive' : 'text-muted-foreground')}>{pct}%</span>
              </span>
              <span aria-hidden className="h-1 overflow-hidden rounded-sm bg-border">
                <span className={cn('block h-full', pct >= 90 ? 'bg-destructive' : DRIVE_COLORS[i % DRIVE_COLORS.length])} style={{ width: `${pct}%` }} />
              </span>
            </button>
          )
        })}
      </Section>
    </nav>
  )
}

// ── Workspace dialogs ─────────────────────────────────────────────────────

/** Save, open and delete workspaces, and confirm before a workspace replaces the open tabs. */
export function WorkspaceDialogs() {
  const dialog = useExplorer((s) => s.workspaceDialog)
  const set = useExplorer((s) => s.set)
  return (
    <Dialog open={!!dialog} onOpenChange={(o) => !o && set({ workspaceDialog: null })}>
      <DialogContent className="sm:max-w-md">{dialog && <WorkspaceDialogBody key={JSON.stringify(dialog)} dialog={dialog} />}</DialogContent>
    </Dialog>
  )
}

function WorkspaceDialogBody({ dialog }: { dialog: WorkspaceDialog }) {
  const workspaces = useExplorer((s) => s.workspaces)
  const current = useExplorer((s) => s.currentWorkspace)
  const tabCount = useExplorer((s) => s.tabs.filter((t) => !t.split).length)
  const set = useExplorer((s) => s.set)
  const saveWorkspace = useExplorer((s) => s.saveWorkspace)
  const requestOpenWorkspace = useExplorer((s) => s.requestOpenWorkspace)
  const openWorkspace = useExplorer((s) => s.openWorkspace)
  const deleteWorkspace = useExplorer((s) => s.deleteWorkspace)
  const [name, setName] = useState(current ?? '')
  /** A name waiting for the user to confirm it: a save that would replace, or a delete. */
  const [pending, setPending] = useState<string | null>(null)
  const close = () => set({ workspaceDialog: null })

  if (dialog.mode === 'save') {
    const clean = name.trim()
    const exists = workspaces.find((w) => w.name.trim().toLowerCase() === clean.toLowerCase())
    const submit = () => {
      if (!clean) return
      if (exists && pending !== clean) {
        setPending(clean)
        return
      }
      void saveWorkspace(clean)
    }
    return (
      <>
        <DialogHeader>
          <DialogTitle>Save Workspace</DialogTitle>
          <DialogDescription>Keeps the tabs, the split pane and the selections in each folder.</DialogDescription>
        </DialogHeader>
        {pending ? (
          <>
            <p className="text-sm">A workspace named “{exists?.name ?? pending}” already exists. Replace it?</p>
            <DialogFooter>
              <Button variant="outline" onClick={() => setPending(null)}>
                Back
              </Button>
              <Button variant="destructive" onClick={() => void saveWorkspace(clean)}>
                Replace
              </Button>
            </DialogFooter>
          </>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault()
              submit()
            }}
            className="grid gap-4"
          >
            <Input
              autoFocus
              value={name}
              placeholder="Workspace name"
              spellCheck={false}
              aria-label="Workspace name"
              onChange={(e) => setName(e.target.value)}
            />
            <DialogFooter>
              <Button type="button" variant="outline" onClick={close}>
                Cancel
              </Button>
              <Button type="submit" disabled={!clean}>
                Save
              </Button>
            </DialogFooter>
          </form>
        )}
      </>
    )
  }

  if (dialog.mode === 'confirmOpen') {
    return (
      <>
        <DialogHeader>
          <DialogTitle>Open “{dialog.name}”?</DialogTitle>
          <DialogDescription>
            This replaces your {tabCount} open {tabCount === 1 ? 'tab' : 'tabs'} in this window.
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button onClick={() => void openWorkspace(dialog.name)}>Open</Button>
        </DialogFooter>
      </>
    )
  }

  // Picker for opening or deleting.
  const deleting = dialog.mode === 'delete'
  return (
    <>
      <DialogHeader>
        <DialogTitle>{deleting ? 'Delete Workspace' : 'Open Workspace'}</DialogTitle>
        <DialogDescription>{deleting ? 'Choose a workspace to delete.' : 'Choose a workspace to open.'}</DialogDescription>
      </DialogHeader>
      {pending ? (
        <>
          <p className="text-sm">Delete “{pending}”? This can’t be undone.</p>
          <DialogFooter>
            <Button variant="outline" onClick={() => setPending(null)}>
              Back
            </Button>
            <Button variant="destructive" onClick={() => void deleteWorkspace(pending)}>
              Delete
            </Button>
          </DialogFooter>
        </>
      ) : workspaces.length === 0 ? (
        <p className="text-sm text-muted-foreground">No workspaces saved yet.</p>
      ) : (
        <div className="flex max-h-72 flex-col gap-0.5 overflow-y-auto">
          {workspaces.map((w: Workspace) => (
            <Button
              key={w.name}
              variant="ghost"
              className="justify-start font-normal"
              onClick={() => (deleting ? setPending(w.name) : requestOpenWorkspace(w.name))}
            >
              <span className="truncate">{w.name}</span>
              <span className="ml-auto font-mono text-[11px] text-muted-foreground">{w.tabs.length}</span>
            </Button>
          ))}
        </div>
      )}
    </>
  )
}
