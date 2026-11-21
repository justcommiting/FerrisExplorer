import { useEffect, useMemo, useRef, useState } from 'react'
import {
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Columns3,
  Command as CommandIcon,
  GitBranch,
  History,
  List,
  Loader2,
  RotateCw,
  Search,
  X,
} from 'lucide-react'
import { toast } from 'sonner'

import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { isMac } from '@/commands'
import { api, errorText } from '@/lib/api'
import { crumbs, parentOf, sepOf } from '@/lib/format'
import { cn } from '@/lib/utils'
import { effectiveView, useExplorer, type ViewMode } from '@/store/explorer'

const mod = isMac ? '⌘' : 'Ctrl '

export function Kbd({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <kbd className={cn('rounded border border-input px-1.5 py-px font-mono text-[11px] text-muted-foreground', className)}>
      {children}
    </kbd>
  )
}

function NavButton({
  label,
  shortcut,
  disabled,
  onClick,
  children,
}: {
  label: string
  shortcut?: string
  disabled?: boolean
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <button
            aria-label={label}
            disabled={disabled}
            onClick={onClick}
            className="grid size-8 place-items-center rounded-md text-foreground/90 transition-colors hover:bg-accent disabled:pointer-events-none disabled:text-dim/60"
          />
        }
      >
        {children}
      </TooltipTrigger>
      <TooltipContent>
        {label}
        {shortcut && <span className="ml-2 opacity-60">{shortcut}</span>}
      </TooltipContent>
    </Tooltip>
  )
}

function LocationBar() {
  const tab = useExplorer((s) => s.activeTab())
  const navigate = useExplorer((s) => s.navigate)
  const focusRequest = useExplorer((s) => s.focusRequest)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (focusRequest.target === 'location' && focusRequest.n > 0) {
      setDraft(tab.dir)
      setEditing(true)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest])

  useEffect(() => {
    if (editing) {
      inputRef.current?.focus()
      inputRef.current?.select()
    }
  }, [editing])

  const submit = async () => {
    try {
      const dir = await api.resolveLocation(draft)
      setEditing(false)
      await navigate(dir)
    } catch (e) {
      toast.error('Cannot go there', { description: errorText(e) })
    }
  }

  if (editing) {
    return (
      <input
        ref={inputRef}
        value={draft}
        spellCheck={false}
        aria-label="Path"
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => setEditing(false)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') void submit()
          if (e.key === 'Escape') setEditing(false)
        }}
        className="h-8 min-w-0 flex-1 rounded-md border border-ring bg-field px-2.5 font-mono text-[12.5px] outline-none"
      />
    )
  }

  const parts = crumbs(tab.dir)
  const sep = sepOf(tab.dir)
  return (
    <nav
      aria-label="Path"
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          setDraft(tab.dir)
          setEditing(true)
        }
      }}
      title="Click empty space to type a path"
      className="flex h-8 min-w-0 flex-1 cursor-text items-center overflow-hidden rounded-md border bg-field px-1.5 font-mono text-[12.5px] hover:border-input"
    >
      {tab.archive && (
        <span
          className="mr-1 shrink-0 rounded border border-input px-1.5 font-sans text-[10.5px] tracking-wide text-muted-foreground uppercase"
          title="Read-only archive"
        >
          archive
        </span>
      )}
      {tab.smart ? (
        <span className="flex min-w-0 items-center gap-2 px-1.5 font-sans text-[12.5px]" title={tab.smart.query}>
          <Search className="size-3.5 shrink-0 text-muted-foreground" strokeWidth={1.6} />
          <span className="truncate font-medium">{tab.smart.name}</span>
          <span className="truncate font-mono text-[11.5px] text-muted-foreground">{tab.smart.query}</span>
        </span>
      ) : parts.map(([label, path], i) => (
        <div key={path} className="flex min-w-0 shrink items-center">
          {i > 1 && <span aria-hidden className="px-0.5 text-dim">{sep}</span>}
          <button
            onClick={() => void navigate(path)}
            className={cn(
              'h-6 truncate rounded px-1.5 hover:bg-accent',
              i === parts.length - 1 ? 'font-medium text-foreground' : 'text-foreground/75',
            )}
          >
            {label}
          </button>
        </div>
      ))}
      {tab.git && (
        <span className="ml-auto flex shrink-0 items-center gap-1.5 pr-1 pl-3 text-git-modified" title={tab.git.root}>
          <GitBranch className="size-3.5" strokeWidth={1.6} />
          {tab.git.branch}
        </span>
      )}
    </nav>
  )
}

function SearchBox() {
  const tab = useExplorer((s) => s.activeTab())
  const history = useExplorer((s) => s.history)
  const saved = useExplorer((s) => s.savedSearches)
  const setQuery = useExplorer((s) => s.setQuery)
  const runSearch = useExplorer((s) => s.runSearch)
  const clearSearch = useExplorer((s) => s.clearSearch)
  const requestFocus = useExplorer((s) => s.requestFocus)
  const focusRequest = useExplorer((s) => s.focusRequest)
  const inputRef = useRef<HTMLInputElement>(null)
  const [open, setOpen] = useState(false)
  const [hi, setHi] = useState(-1)

  useEffect(() => {
    if (focusRequest.target === 'search' && focusRequest.n > 0) {
      inputRef.current?.focus()
      inputRef.current?.select()
    }
  }, [focusRequest])

  const suggestions = useMemo(() => {
    const q = tab.query.trim().toLowerCase()
    const fromSaved = saved
      .filter((s) => !q || s.name.toLowerCase().includes(q) || s.query.toLowerCase().includes(q))
      .map((s) => ({ label: s.name, query: s.query, saved: true }))
    const fromHistory = history
      .filter((h) => (!q || h.toLowerCase().includes(q)) && h !== tab.query)
      .slice(0, 6)
      .map((h) => ({ label: h, query: h, saved: false }))
    return [...fromSaved.slice(0, 4), ...fromHistory]
  }, [tab.query, history, saved])

  const pick = (query: string) => {
    setOpen(false)
    setHi(-1)
    void runSearch(query)
  }

  return (
    <div className="relative min-w-0 flex-[1_1_300px]">
      <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
      <input
        ref={inputRef}
        value={tab.query}
        spellCheck={false}
        aria-label="Search"
        placeholder="Search the index   ext:rs  size:>1mb  modified:<7d"
        onChange={(e) => {
          setQuery(e.target.value)
          setOpen(true)
          setHi(-1)
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setTimeout(() => setOpen(false), 120)}
        onKeyDown={(e) => {
          if (e.key === 'ArrowDown' && suggestions.length) {
            e.preventDefault()
            setOpen(true)
            setHi((h) => Math.min(h + 1, suggestions.length - 1))
          } else if (e.key === 'ArrowUp' && open) {
            e.preventDefault()
            setHi((h) => Math.max(h - 1, -1))
          } else if (e.key === 'Enter') {
            e.preventDefault()
            if (open && hi >= 0) pick(suggestions[hi].query)
            else pick(tab.query)
          } else if (e.key === 'Escape') {
            e.preventDefault()
            if (open && suggestions.length) setOpen(false)
            else {
              clearSearch()
              requestFocus('list')
              inputRef.current?.blur()
            }
          }
        }}
        className="h-8 w-full rounded-md border bg-field pr-9 pl-8 font-mono text-[12.5px] outline-none placeholder:text-dim focus:border-ring"
      />
      <div className="absolute top-1/2 right-2 -translate-y-1/2">
        {tab.searching ? (
          <Loader2 className="size-3.5 animate-spin text-primary" />
        ) : tab.query ? (
          <button aria-label="Clear search" onClick={clearSearch} className="rounded p-0.5 text-muted-foreground hover:text-foreground">
            <X className="size-3.5" />
          </button>
        ) : (
          <Kbd>/</Kbd>
        )}
      </div>
      {open && suggestions.length > 0 && (
        <div className="absolute top-9 right-0 left-0 z-30 overflow-hidden rounded-lg border border-input bg-popover p-1 shadow-2xl shadow-black/50">
          {suggestions.map((s, i) => (
            <button
              key={`${s.saved}-${s.label}`}
              onMouseDown={(e) => {
                e.preventDefault()
                pick(s.query)
              }}
              onMouseEnter={() => setHi(i)}
              className={cn(
                'flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs',
                i === hi ? 'bg-accent text-accent-foreground' : 'text-muted-foreground',
              )}
            >
              {s.saved ? <Search className="size-3 text-primary" /> : <History className="size-3" />}
              <span className="truncate font-mono text-foreground">{s.label}</span>
              {s.saved && s.label !== s.query && (
                <span className="ml-auto truncate font-mono text-[10px] opacity-70">{s.query}</span>
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/** List or columns layout for the focused tab. Columns are off while the tab shows a smart folder or a search. */
function ViewToggle() {
  const tab = useExplorer((s) => s.activeTab())
  const setView = useExplorer((s) => s.setView)
  const shown = effectiveView(tab)
  const views: { view: ViewMode; label: string; icon: React.ReactNode }[] = [
    { view: 'list', label: 'List view', icon: <List className="size-4" strokeWidth={1.6} /> },
    { view: 'columns', label: 'Columns view', icon: <Columns3 className="size-4" strokeWidth={1.6} /> },
  ]
  const noColumns = tab.smart !== null || tab.activeQuery !== null
  return (
    <div role="group" aria-label="View" className="flex gap-0.5">
      {views.map(({ view, label, icon }) => (
        <Tooltip key={view}>
          <TooltipTrigger
            render={
              <button
                aria-label={label}
                aria-pressed={shown === view}
                disabled={view === 'columns' && noColumns}
                onClick={() => setView(view, tab.id)}
                className={cn(
                  'grid size-8 place-items-center rounded-md text-foreground/90 transition-colors hover:bg-accent disabled:pointer-events-none disabled:text-dim/60',
                  shown === view && 'bg-accent text-foreground',
                )}
              />
            }
          >
            {icon}
          </TooltipTrigger>
          <TooltipContent>
            {label}
            {view === 'columns' && noColumns && <span className="ml-2 opacity-60">folders only</span>}
          </TooltipContent>
        </Tooltip>
      ))}
    </div>
  )
}

export function Toolbar() {
  const tab = useExplorer((s) => s.activeTab())
  const goBack = useExplorer((s) => s.goBack)
  const goForward = useExplorer((s) => s.goForward)
  const goUp = useExplorer((s) => s.goUp)
  const refresh = useExplorer((s) => s.refresh)
  const set = useExplorer((s) => s.set)

  return (
    <div className="flex h-12 shrink-0 items-center gap-3 border-b bg-card px-3">
      <div className="flex gap-0.5">
        <NavButton label="Back" shortcut="Alt+←" disabled={tab.historyIndex <= 0} onClick={goBack}>
          <ArrowLeft className="size-4" strokeWidth={1.6} />
        </NavButton>
        <NavButton
          label="Forward"
          shortcut="Alt+→"
          disabled={tab.historyIndex >= tab.history.length - 1}
          onClick={goForward}
        >
          <ArrowRight className="size-4" strokeWidth={1.6} />
        </NavButton>
        <NavButton label="Up one folder" shortcut="Alt+↑" disabled={!tab.smart && !parentOf(tab.dir)} onClick={goUp}>
          <ArrowUp className="size-4" strokeWidth={1.6} />
        </NavButton>
        <NavButton label="Refresh" shortcut={`${mod}R`} onClick={() => void refresh()}>
          <RotateCw className={cn('size-3.5', tab.loading && 'animate-spin')} strokeWidth={1.6} />
        </NavButton>
      </div>
      <div className="flex min-w-0 flex-[1_1_280px]">
        <LocationBar />
      </div>
      <SearchBox />
      <ViewToggle />
      <button
        onClick={() => set({ paletteOpen: true })}
        className="flex h-8 shrink-0 items-center gap-2 rounded-md border bg-muted px-2.5 hover:bg-accent"
      >
        <CommandIcon className="size-3.5 text-muted-foreground" strokeWidth={1.6} />
        Commands
        <Kbd>{mod}K</Kbd>
      </button>
    </div>
  )
}
