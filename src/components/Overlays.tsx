import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { Folder, Loader2 } from 'lucide-react'

import { FileIcon } from '@/components/FileIcon'
import { PluginRequestDialog, PluginsOverlay } from '@/components/PluginsOverlay'
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from '@/components/ui/command'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { commands, pluginCommands } from '@/commands'
import { api, errorText, type DeleteSummary, type JumpHit, type VNode } from '@/lib/api'
import {
  DEFAULT_OPTIONS,
  folderName,
  planRenames,
  splitPath,
  type CaseMode,
  type Item,
  type RenameOptions,
} from '@/lib/bulkRename'
import { basename, formatSize, parentOf } from '@/lib/format'
import { isMac, isWindows } from '@/lib/platform'
import { VIM_BINDINGS } from '@/lib/vim'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

// ── Command palette (Ctrl+K) ──────────────────────────────────────────────

export function CommandPalette() {
  const open = useExplorer((s) => s.paletteOpen)
  const set = useExplorer((s) => s.set)
  const pluginReport = useExplorer((s) => s.pluginReport)
  const all = useMemo(() => [...commands, ...pluginCommands(pluginReport)], [pluginReport])
  const groups = [...new Set(all.map((c) => c.group))]

  // The jump list and the shortcut help open from the palette's commands, so they mount here.
  return (
    <>
    <CommandDialog open={open} onOpenChange={(o) => set({ paletteOpen: o })} className="sm:max-w-lg">
      <Command>
      <CommandInput placeholder="Type a command…" />
      <CommandList className="max-h-96">
        <CommandEmpty>No matching command.</CommandEmpty>
        {open &&
          groups.map((g) => (
            <CommandGroup key={g} heading={g}>
              {all
                .filter((c) => c.group === g && (!c.enabled || c.enabled()))
                .map((c) => (
                  <CommandItem
                    key={c.id}
                    value={`${c.label} ${c.keywords ?? ''}`}
                    onSelect={() => {
                      set({ paletteOpen: false })
                      // Let the dialog close (and release focus) before running.
                      setTimeout(() => void c.run(), 0)
                    }}
                    className="text-[13px]"
                  >
                    {c.label}
                    {c.toggled && (
                      <CommandShortcut className="font-mono text-[11px] tracking-normal">{c.toggled() ? 'On' : 'Off'}</CommandShortcut>
                    )}
                    {c.shortcut && (
                      <CommandShortcut className="rounded border border-input px-1.5 font-mono text-[11px] tracking-normal">
                        {c.shortcut}
                      </CommandShortcut>
                    )}
                  </CommandItem>
                ))}
            </CommandGroup>
          ))}
      </CommandList>
      <div className="flex items-center gap-4 border-t px-3 py-2 text-[11px] text-muted-foreground">
        <span><kbd className="font-mono">↑ ↓</kbd> move</span>
        <span><kbd className="font-mono">Enter</kbd> run</span>
        <span className="ml-auto"><kbd className="font-mono">Ctrl P</kbd> go to file</span>
      </div>
      </Command>
    </CommandDialog>
    <JumpToFolder />
    <ShortcutsHelp />
    <PluginsOverlay />
    <PluginRequestDialog />
    </>
  )
}

// ── Jump to folder (Ctrl+J): visited folders, ranked by frecency ───────────

export function JumpToFolder() {
  const open = useExplorer((s) => s.jumpOpen)
  const set = useExplorer((s) => s.set)
  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<JumpHit[]>([])
  const gen = useRef(0)

  useEffect(() => {
    if (!open) return
    const g = ++gen.current
    api
      .jumpFolders(query)
      .then((r) => g === gen.current && setHits(r))
      .catch(() => g === gen.current && setHits([]))
  }, [query, open])

  const choose = (path: string) => {
    set({ jumpOpen: false })
    void useExplorer.getState().navigate(path)
  }

  return (
    <CommandDialog
      open={open}
      onOpenChange={(o) => {
        if (!o) setQuery('')
        set({ jumpOpen: o })
      }}
      title="Jump to folder"
      description="Folders you visit most and most recently"
      className="sm:max-w-xl"
    >
      {/* Results are already ranked by Rust, so cmdk must not re-filter. */}
      <Command shouldFilter={false}>
      <CommandInput placeholder="Folder name or path fragments, e.g. proj fer" value={query} onValueChange={setQuery} />
      <CommandList className="max-h-[26rem]">
        {hits.length === 0 && (
          <CommandEmpty>{query ? 'No visited folder matches.' : 'Folders you visit will show up here.'}</CommandEmpty>
        )}
        {hits.map((h) => (
          <CommandItem key={h.path} value={h.path} onSelect={() => choose(h.path)} className="text-xs">
            <Folder className="size-3.5 shrink-0 text-muted-foreground" strokeWidth={1.5} />
            <span className="shrink-0">{basename(h.path)}</span>
            <span className="truncate text-[11px] text-muted-foreground">{parentOf(h.path)}</span>
          </CommandItem>
        ))}
      </CommandList>
      </Command>
    </CommandDialog>
  )
}

// ── Keyboard shortcuts (?): built from the command catalog and the vim map ──

export function ShortcutsHelp() {
  const open = useExplorer((s) => s.shortcutsOpen)
  const vimMode = useExplorer((s) => s.vimMode)
  const set = useExplorer((s) => s.set)
  const groups = [...new Set(commands.map((c) => c.group))]
  const bound = commands.filter((c) => c.shortcut)

  const row = (keys: string, does: string) => (
    <div key={keys} className="flex items-center justify-between gap-3 text-[12.5px]">
      <span>{does}</span>
      <kbd className="shrink-0 rounded border border-input px-1.5 font-mono text-[11px]">{keys}</kbd>
    </div>
  )

  return (
    <Dialog open={open} onOpenChange={(o) => set({ shortcutsOpen: o })}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Keyboard shortcuts</DialogTitle>
          <DialogDescription>
            Vim-style keys are {vimMode ? 'on' : 'off'}. Toggle them with “Vim-Style Keys” in the command palette.
          </DialogDescription>
        </DialogHeader>
        <div className="grid max-h-[65vh] gap-x-8 gap-y-5 overflow-y-auto sm:grid-cols-2">
          {groups.map((g) => {
            const rows = bound.filter((c) => c.group === g)
            if (rows.length === 0) return null
            return (
              <section key={g} className="grid content-start gap-1.5">
                <h3 className="text-[11px] tracking-[.06em] text-muted-foreground uppercase">{g}</h3>
                {rows.map((c) => row(c.shortcut ?? '', c.label))}
              </section>
            )
          })}
          <section className="grid content-start gap-1.5">
            <h3 className="text-[11px] tracking-[.06em] text-muted-foreground uppercase">List</h3>
            {row('A–Z', 'Type-ahead: jump to a name (repeat a letter to cycle)')}
            {row('↑ ↓ Home End', 'Move the cursor')}
          </section>
          {vimMode && (
            <section className="grid content-start gap-1.5">
              <h3 className="text-[11px] tracking-[.06em] text-muted-foreground uppercase">Vim-style keys</h3>
              {VIM_BINDINGS.map((b) => row(b.keys, b.does))}
            </section>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}

// ── Go to file (Ctrl+P): index-wide lookup ─────────────────────────────────

export function QuickOpen() {
  const open = useExplorer((s) => s.quickOpenOpen)
  const indexExists = useExplorer((s) => s.index.exists)
  const set = useExplorer((s) => s.set)
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<VNode[]>([])
  const [loading, setLoading] = useState(false)
  const gen = useRef(0)
  const [wasOpen, setWasOpen] = useState(open)

  const onQuery = (q: string) => {
    setQuery(q)
    setLoading(!!q.trim())
    if (!q.trim()) setResults([])
  }

  if (open !== wasOpen) {
    setWasOpen(open)
    if (!open) onQuery('')
  }

  useEffect(() => {
    // Bumping gen also drops any in-flight search for an older query.
    const g = ++gen.current
    if (!query.trim()) return
    const t = setTimeout(() => {
      api
        .quickSearch(query)
        .then((r) => g === gen.current && setResults(r))
        .catch(() => g === gen.current && setResults([]))
        .finally(() => g === gen.current && setLoading(false))
    }, 120)
    return () => clearTimeout(t)
  }, [query])

  const choose = async (node: VNode) => {
    set({ quickOpenOpen: false })
    const st = useExplorer.getState()
    if (node.is_dir) {
      await st.navigate(node.path)
    } else {
      const parent = parentOf(node.path)
      if (parent) {
        await st.navigate(parent)
        st.setSelection([node.path], node.path, node.path)
      }
    }
  }

  return (
    <CommandDialog
      open={open}
      onOpenChange={(o) => set({ quickOpenOpen: o })}
      title="Go to file"
      description="Search the whole index"
      className="sm:max-w-xl"
    >
      {/* Results are already ranked by Rust, so cmdk must not re-filter. */}
      <Command shouldFilter={false}>
      <CommandInput placeholder={indexExists ? 'Go to file or folder anywhere…' : 'Build the index first (sidebar ↙)'} value={query} onValueChange={onQuery} />
      <CommandList className="max-h-[26rem]">
        {query && !loading && <CommandEmpty>No indexed matches.</CommandEmpty>}
        {loading && results.length === 0 && (
          <div className="flex justify-center py-6">
            <Loader2 className="size-4 animate-spin text-muted-foreground" />
          </div>
        )}
        {results.map((n) => (
          <CommandItem key={n.path} value={n.path} onSelect={() => void choose(n)} className="text-xs">
            <FileIcon node={n} />
            <span className="shrink-0">{n.name}</span>
            <span className="truncate text-[11px] text-muted-foreground">{parentOf(n.path)}</span>
          </CommandItem>
        ))}
      </CommandList>
      </Command>
    </CommandDialog>
  )
}

// ── Text prompts (rename, new folder, move to, save search) ───────────────

const PROMPT_TEXT = {
  rename: { title: 'Rename', action: 'Rename', hint: 'Renaming never overwrites an existing file.' },
  newFolder: { title: 'New folder', action: 'Create', hint: 'Created in the current folder.' },
  moveTo: { title: 'Move to', action: 'Move', hint: 'Destination folder path. ~ expands to your home folder.' },
  saveSearch: { title: 'Save search', action: 'Save', hint: 'Name shown in the sidebar.' },
  extractTo: { title: 'Extract to', action: 'Extract', hint: 'Folder to extract into. The contents go in a new folder named after the archive.' },
  saveSmart: { title: 'Save smart folder', action: 'Save', hint: 'Pinned to the sidebar. Its results stay up to date as files change.' },
  renameSmart: { title: 'Rename smart folder', action: 'Rename', hint: 'Names must be unique among saved searches.' },
  editSmartQuery: { title: 'Edit query', action: 'Save', hint: 'Filters such as ext:rs or size:>1gb, as in the search box.' },
} as const

export function PromptDialog() {
  const prompt = useExplorer((s) => s.prompt)
  const set = useExplorer((s) => s.set)
  const submitPrompt = useExplorer((s) => s.submitPrompt)
  const [value, setValue] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)
  const [shownPrompt, setShownPrompt] = useState(prompt)

  if (prompt !== shownPrompt) {
    setShownPrompt(prompt)
    if (prompt) setValue(prompt.initial)
  }

  useEffect(() => {
    if (!prompt) return
    // Select the stem only, so typing replaces the name but keeps the extension.
    setTimeout(() => {
      const el = inputRef.current
      if (!el) return
      el.focus()
      const dot = prompt.kind === 'rename' ? prompt.initial.lastIndexOf('.') : -1
      el.setSelectionRange(0, dot > 0 ? dot : prompt.initial.length)
    }, 30)
  }, [prompt])

  const text = prompt ? PROMPT_TEXT[prompt.kind] : PROMPT_TEXT.rename
  return (
    <Dialog open={!!prompt} onOpenChange={(o) => !o && set({ prompt: null })}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{text.title}</DialogTitle>
          <DialogDescription>
            {prompt?.kind === 'rename' ? basename(prompt.path) : text.hint}
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            void submitPrompt(value)
          }}
          className="grid gap-4"
        >
          <Input ref={inputRef} value={value} spellCheck={false} onChange={(e) => setValue(e.target.value)} />
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => set({ prompt: null })}>
              Cancel
            </Button>
            <Button type="submit">{text.action}</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

// ── Bulk rename ───────────────────────────────────────────────────────────

const PLATFORM = { windows: isWindows, caseInsensitive: isWindows || isMac }
const TOKEN_HELP = '{name} stem · {ext} extension · {n} or {n:03} counter · {date} modified date · {parent} folder'

interface Listing {
  items: Item[]
  /** The names already in each folder, so a clash with a file outside the selection is caught. */
  listed: Map<string, Set<string>>
}

async function loadListing(paths: string[]): Promise<Listing> {
  const parents = [...new Set(paths.map((p) => splitPath(p).parent))]
  const folders = await Promise.all(parents.map(async (parent) => [parent, await api.listDir(parent)] as const))
  const listed = new Map<string, Set<string>>()
  const modified = new Map<string, number>()
  for (const [parent, nodes] of folders) {
    listed.set(parent, new Set(nodes.map((n) => n.name)))
    for (const n of nodes) modified.set(n.path, n.modified_at)
  }
  const items = paths.map((path): Item => {
    const { parent, name } = splitPath(path)
    return { path, name, parent, parentName: folderName(parent), modified: modified.get(path) ?? 0 }
  })
  return { items, listed }
}

export function BulkRenameDialog() {
  const paths = useExplorer((s) => s.bulkRename)
  const set = useExplorer((s) => s.set)
  return (
    <Dialog open={!!paths} onOpenChange={(o) => !o && set({ bulkRename: null })}>
      {paths && <BulkRenameContent paths={paths} />}
    </Dialog>
  )
}

function Check({ checked, onChange, children }: { checked: boolean; onChange: (v: boolean) => void; children: ReactNode }) {
  return (
    <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
      <input type="checkbox" className="size-3.5 accent-current" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      {children}
    </label>
  )
}

/** Mounted only while the dialog is open, so each opening starts from the defaults. */
function BulkRenameContent({ paths }: { paths: string[] }) {
  const set = useExplorer((s) => s.set)
  const renameBatch = useExplorer((s) => s.renameBatch)
  const [opts, setOpts] = useState<RenameOptions>(DEFAULT_OPTIONS)
  const [listing, setListing] = useState<Listing | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let live = true
    loadListing(paths)
      .then((l) => live && setListing(l))
      .catch((e) => live && setLoadError(errorText(e)))
    return () => {
      live = false
    }
  }, [paths])

  const plan = useMemo(
    () => (listing ? planRenames(listing.items, opts, listing.listed, PLATFORM) : null),
    [listing, opts],
  )
  const patch = (p: Partial<RenameOptions>) => setOpts((o) => ({ ...o, ...p }))
  const changes = plan ? plan.rows.filter((r) => !r.unchanged) : []
  const errors = plan ? plan.rows.filter((r) => r.error).length : 0
  const canApply = !!plan && !plan.error && changes.length > 0 && errors === 0 && !busy

  const apply = async () => {
    if (!canApply) return
    setBusy(true)
    // Closes whatever the outcome: the store's toast says what was renamed and what was not.
    await renameBatch(changes.map((r): [string, string] => [r.item.path, r.newName]))
    setBusy(false)
    set({ bulkRename: null })
  }

  const summary = !plan
    ? loadError ? '' : 'Reading folders…'
    : plan.error ? '' : errors ? `${errors} ${errors === 1 ? 'row needs' : 'rows need'} fixing` : changes.length ? `${changes.length} to rename` : 'Nothing to rename yet'

  return (
    <DialogContent className="sm:max-w-2xl">
      <DialogHeader>
        <DialogTitle>Bulk rename</DialogTitle>
        <DialogDescription>{paths.length} items. Nothing is renamed until you apply.</DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(e) => {
          e.preventDefault()
          void apply()
        }}
        className="grid gap-3"
      >
        <div className="flex gap-1.5">
          <Button type="button" size="sm" variant={opts.mode === 'template' ? 'default' : 'outline'} onClick={() => patch({ mode: 'template' })}>
            Template
          </Button>
          <Button type="button" size="sm" variant={opts.mode === 'replace' ? 'default' : 'outline'} onClick={() => patch({ mode: 'replace' })}>
            Find and replace
          </Button>
        </div>

        {opts.mode === 'template' ? (
          <>
            <label className="grid gap-1.5 text-xs text-muted-foreground">
              Template
              <Input value={opts.template} spellCheck={false} className="font-mono" onChange={(e) => patch({ template: e.target.value })} />
            </label>
            <div className="flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
              <label className="flex items-center gap-2">
                Start
                <Input type="number" value={opts.start} className="w-20 font-mono" onChange={(e) => patch({ start: Number(e.target.value) })} />
              </label>
              <label className="flex items-center gap-2">
                Step
                <Input type="number" value={opts.step} className="w-20 font-mono" onChange={(e) => patch({ step: Number(e.target.value) })} />
              </label>
            </div>
            <p className="text-[11px] text-muted-foreground">{TOKEN_HELP}. An empty {'{ext}'} drops the dot before it.</p>
          </>
        ) : (
          <>
            <div className="grid grid-cols-2 gap-3">
              <label className="grid gap-1.5 text-xs text-muted-foreground">
                Find
                <Input value={opts.find} spellCheck={false} className="font-mono" onChange={(e) => patch({ find: e.target.value })} />
              </label>
              <label className="grid gap-1.5 text-xs text-muted-foreground">
                Replace with
                <Input value={opts.replacement} spellCheck={false} className="font-mono" onChange={(e) => patch({ replacement: e.target.value })} />
              </label>
            </div>
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
              <Check checked={opts.regex} onChange={(v) => patch({ regex: v })}>Regular expression</Check>
              <Check checked={opts.caseSensitive} onChange={(v) => patch({ caseSensitive: v })}>Case-sensitive</Check>
              <Check checked={opts.stemOnly} onChange={(v) => patch({ stemOnly: v })}>Name only, keep extension</Check>
            </div>
          </>
        )}

        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          Case
          <select
            value={opts.caseMode}
            onChange={(e) => patch({ caseMode: e.target.value as CaseMode })}
            className="h-7 rounded-lg border border-input bg-transparent px-2 text-xs dark:bg-input/30"
          >
            <option value="keep">Keep</option>
            <option value="lower">lowercase</option>
            <option value="upper">UPPERCASE</option>
            <option value="title">Title Case</option>
          </select>
        </div>

        {(plan?.error || loadError) && <p className="text-xs text-destructive">{plan?.error ?? loadError}</p>}

        <div className="max-h-72 overflow-auto rounded-md border">
          <table className="w-full text-[12.5px]">
            <thead className="sticky top-0 bg-popover text-left text-xs text-muted-foreground">
              <tr>
                <th className="px-2 py-1 font-medium">Current name</th>
                <th className="px-2 py-1 font-medium">New name</th>
              </tr>
            </thead>
            <tbody>
              {plan?.rows.map((r) => (
                <tr key={r.item.path} className={cn('border-t', r.unchanged && 'text-muted-foreground opacity-50')}>
                  <td className="max-w-0 truncate px-2 py-1 font-mono">{r.item.name}</td>
                  <td className="max-w-0 px-2 py-1 font-mono">
                    <div className={cn('truncate', r.error && 'text-destructive')}>{r.newName || ' '}</div>
                    {r.error && <div className="font-sans text-[11px] text-destructive">{r.error}</div>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <DialogFooter>
          <span className="mr-auto text-xs text-muted-foreground">{summary}</span>
          <Button type="button" variant="outline" onClick={() => set({ bulkRename: null })}>
            Cancel
          </Button>
          <Button type="submit" disabled={!canApply}>
            {busy && <Loader2 className="animate-spin" />}
            {changes.length ? `Rename ${changes.length}` : 'Rename'}
          </Button>
        </DialogFooter>
      </form>
    </DialogContent>
  )
}

// ── Delete confirmation ───────────────────────────────────────────────────

/** Deletes at least this big must be confirmed by typing a name. */
const LARGE_FILES = 1000
const LARGE_BYTES = 1024 ** 3

export function DeleteDialog() {
  const paths = useExplorer((s) => s.confirmDelete)
  const set = useExplorer((s) => s.set)
  const deletePermanently = useExplorer((s) => s.deletePermanently)
  const [busy, setBusy] = useState(false)
  const [summary, setSummary] = useState<DeleteSummary | null>(null)
  const [typed, setTyped] = useState('')
  const [shownPaths, setShownPaths] = useState(paths)

  if (paths !== shownPaths) {
    setShownPaths(paths)
    setSummary(null)
    setTyped('')
  }

  useEffect(() => {
    if (!paths) return
    let live = true
    api
      .deleteSummary(paths)
      .then((s) => live && setSummary(s))
      .catch(() => live && setSummary({ files: 0, dirs: 0, bytes: 0, truncated: true }))
    return () => {
      live = false
    }
  }, [paths])

  const single = paths?.length === 1 ? basename(paths[0]) : null
  const large = !!summary && (summary.truncated || summary.files >= LARGE_FILES || summary.bytes >= LARGE_BYTES)
  const phrase = single ?? `delete ${paths?.length ?? 0} items`
  const confirmed = !large || typed.trim() === phrase
  const fileCount = summary ? `${summary.files.toLocaleString()}${summary.truncated ? '+' : ''}` : null

  const confirm = async () => {
    if (!paths || !summary || !confirmed) return
    setBusy(true)
    await deletePermanently(paths)
    setBusy(false)
    set({ confirmDelete: null })
  }

  return (
    <AlertDialog open={!!paths} onOpenChange={(o) => !o && !busy && set({ confirmDelete: null })}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Delete permanently?</AlertDialogTitle>
          <AlertDialogDescription>
            <span className="font-mono text-foreground">{single ?? `${paths?.length} items`}</span>
            {summary ? (
              <>
                {' · '}
                {formatSize(summary.bytes)}
                {summary.truncated && '+'} · {fileCount} file{summary.files === 1 ? '' : 's'}
              </>
            ) : (
              ' · counting…'
            )}
            . There is no Recycle Bin step, so this cannot be undone.
          </AlertDialogDescription>
        </AlertDialogHeader>
        {paths && paths.length > 1 && (
          <ul className="m-0 max-h-28 list-none overflow-y-auto rounded-md border bg-field p-2 font-mono text-[11px] text-muted-foreground">
            {paths.map((p) => (
              <li key={p} className="truncate">{p}</li>
            ))}
          </ul>
        )}
        {large && (
          <div className="flex flex-col gap-1.5">
            <label htmlFor="confirm-delete" className="text-xs text-muted-foreground">
              Type <code className="rounded bg-field px-1 font-mono text-foreground">{phrase}</code> to confirm
            </label>
            <Input
              id="confirm-delete"
              autoFocus
              value={typed}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') void confirm()
              }}
              className="font-mono"
            />
          </div>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={busy}>Cancel</AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={busy || !summary || !confirmed}
            onClick={() => void confirm()}
            autoFocus={!large}
          >
            {(busy || !summary) && <Loader2 className="animate-spin" />}
            {summary && summary.files > 1 ? `Delete ${fileCount} files` : 'Delete'}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
