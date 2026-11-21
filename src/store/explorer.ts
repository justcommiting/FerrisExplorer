import { create } from 'zustand'
import { toast } from 'sonner'

import {
  api,
  errorText,
  newRequestId,
  type ArchiveLocation,
  type CompareBulk,
  type CompareBy,
  type CompareDone,
  type CompareProgress,
  type ComparePick,
  type Comparison,
  type DupDone,
  type DupProgress,
  type DupScan,
  type GitStatus,
  type IndexState,
  type ListingChunk,
  type OpProgress,
  type PluginPlan,
  type PluginRunRef,
  type PluginReport,
  type RenameOutcome,
  type SavedSearch,
  type SearchEvent,
  type ShellDoneEvent,
  type ShellExitEvent,
  type ShellLine,
  type ShellLinesEvent,
  type SyncDone,
  type SyncOutcome,
  type SyncProgress,
  type SyncSummary,
  type VNode,
  type Workspace,
  type WorkspaceTab,
} from '@/lib/api'
import { isArchiveName } from '@/lib/archive'
import { allowedSelection, fullySelectedSets } from '@/lib/duplicates'
import { basename, parentOf } from '@/lib/format'
import { activeListings, nextListingRequest, sizeRequests, type ActiveListing } from '@/lib/listing'
import { isRedoable, PartialChangeError, reverseChange, replayChange, type UndoEntry } from '@/lib/undo'

export type SortColumn = 'name' | 'size' | 'modified' | 'kind'
export type SortDir = 'asc' | 'desc'
/** How a folder's items are laid out: one list, or Finder-style columns. */
export type ViewMode = 'list' | 'columns'

/** A saved search opened as a folder. It stays live while it is open. */
export interface SmartFolder {
  name: string
  query: string
  /** The folder it was opened from: the scope for free-text queries, and Up. */
  dir: string
}

export interface Tab {
  id: number
  /** The right-hand pane of a split; not shown in the tab bar. */
  split?: boolean
  dir: string
  items: VNode[]
  loading: boolean
  /** Entries received so far while the folder is still streaming in; null once it is complete. */
  listing: number | null
  listingError: string | null
  history: string[]
  historyIndex: number

  /** Text in the search box. */
  query: string
  /** The query the current results answer; null when not searching. */
  activeQuery: string | null
  results: VNode[]
  searching: boolean
  searchError: string | null
  interpretedAs: string | null
  queryErrors: string[]

  selected: string[]
  anchor: string | null
  cursor: string | null
  sort: { col: SortColumn; dir: SortDir }
  git: GitStatus | null
  /** Paths in the current listing that git ignores. Refreshed with the listing. */
  ignored: string[]
  /** Set while `dir` is inside an archive (or is its root). Archives are read-only. */
  archive: ArchiveLocation | null
  /** Set while the tab shows a smart folder rather than a search or a listing. */
  smart: SmartFolder | null
  /** The quick filter's text while its box is open, else null. Narrows the listing by name. */
  filter: string | null
  /** The folder layout. Smart folders and searches always show the list. */
  view: ViewMode
  /**
   * Columns view: the folders opened after `dir`. Column k lists `dir` for k=0,
   * else `colPath[k-1]`; the row for `colPath[k]` is selected in column k.
   */
  colPath: string[]
}

/** The workspace dialog's step: a picker, a name to save under, or a confirmation. */
export type WorkspaceDialog =
  | { mode: 'save' }
  | { mode: 'open' }
  | { mode: 'delete' }
  | { mode: 'confirmOpen'; name: string }

export type Prompt =
  | { kind: 'rename'; path: string; initial: string }
  | { kind: 'newFolder'; initial: string }
  | { kind: 'moveTo'; initial: string }
  | { kind: 'saveSearch'; initial: string }
  | { kind: 'extractTo'; archive: string; initial: string }
  | { kind: 'saveSmart'; initial: string; query: string }
  | { kind: 'renameSmart'; name: string; initial: string }
  | { kind: 'editSmartQuery'; name: string; initial: string }

export type DirSize = number | 'pending' | 'failed'
export type Pane = 'primary' | 'split'
export type FocusTarget = 'location' | 'search' | 'list' | 'filter'

/** Run Command on Selection: what the input was opened for. */
/** A plugin action that asks first. The dialog shows `plan.lines`, then runs them through the shell. */
export interface PluginRequest extends PluginRunRef {
  title: string
  plan: PluginPlan
  tabId: number
  dir: string
  paths: string[]
}

export interface ShellTarget {
  tabId: number
  dir: string
  paths: string[]
  /** Counts openings, so the dialog starts fresh each time it opens. */
  opened: number
}

export interface ShellInvocation {
  command: string
  /** 'pending' runs have not started yet; 'skipped' ones never ran. */
  status: 'pending' | 'running' | 'ok' | 'failed' | 'cancelled' | 'skipped'
  code: number | null
}

export interface ShellRunView {
  id: number
  tabId: number
  stopOnError: boolean
  status: 'running' | 'done' | 'cancelled' | 'stopped'
  invocations: ShellInvocation[]
  /** Output in arrival order. `inv` is the invocation index the line came from. */
  lines: (ShellLine & { inv: number })[]
  /** Lines below this sequence number were dropped to keep the output small. */
  firstSeq: number
}

interface State {
  ready: boolean
  home: string
  drives: string[]
  tabs: Tab[]
  activeTabId: number

  favorites: string[]
  savedSearches: SavedSearch[]
  /** Bumped after each live refresh of the open smart folders. */
  smartTick: number
  history: string[]
  index: IndexState
  ops: Record<number, OpProgress>
  /** What each operation is doing, in words, keyed by op id. */
  opsMeta: Record<number, { title: string; started: number }>
  opsPanelOpen: boolean
  splitTabId: number | null
  focusedPane: Pane
  dirSizes: Record<string, DirSize>
  clipboard: { cut: boolean; paths: string[] } | null
  undoStack: UndoEntry[]
  redoStack: UndoEntry[]
  /** Finished operations whose result arrived before the start call returned. */
  doneEarly: Record<number, OpProgress>

  showHidden: boolean
  showDetails: boolean
  paletteOpen: boolean
  quickOpenOpen: boolean
  /** Vim-style list keys. Off by default; saved in the key preferences. */
  vimMode: boolean
  jumpOpen: boolean
  shortcutsOpen: boolean
  pluginsOpen: boolean
  /** The last load of the plugin folder. Null until the first load. */
  pluginReport: PluginReport | null
  /** A confirmed plugin action, waiting for the user to read its command lines. */
  pluginRequest: PluginRequest | null
  spaceFinderOpen: boolean
  /** Which view the space finder opens on. */
  spaceFinderView: 'map' | 'changes'
  duplicateFinderOpen: boolean
  /** What the duplicate finder searched, and the folder it started from. */
  dupScope: 'folder' | 'index'
  dupRoot: string
  dupStatus: 'idle' | 'scanning' | 'done'
  /** Id of the running scan, once the backend has returned it. */
  dupScanId: number | null
  dupProgress: DupProgress | null
  dupScan: DupScan | null
  dupError: string | null
  /** Copies picked for the Trash. Never every copy of a set. */
  dupSelected: Set<string>
  /** Folder compare: the two folders, how to compare them, and the result. */
  compareOpen: boolean
  compareLeft: string
  compareRight: string
  compareBy: CompareBy
  compareStatus: 'idle' | 'running' | 'done'
  compareId: number | null
  compareProgress: CompareProgress | null
  comparison: Comparison | null
  compareError: string | null
  compareHideIdentical: boolean
  /** The action chosen for each row, by relative path. A row without one is left alone. */
  comparePicks: Record<string, ComparePick>
  /** A reviewed sync, waiting for the user to confirm it. */
  comparePlan: SyncSummary | null
  /** The two folders' names, for the undo entry of the running sync. */
  syncLabel: string
  syncStatus: 'idle' | 'running' | 'done'
  syncId: number | null
  syncProgress: SyncProgress | null
  syncOutcome: SyncOutcome | null
  syncError: string | null
  /** Run Command on Selection: the target while its input is open, saved commands, and the output panel. */
  shellTarget: ShellTarget | null
  shellHistory: string[]
  shellPanelOpen: boolean
  shellRun: ShellRunView | null
  prompt: Prompt | null
  confirmDelete: string[] | null
  /** The files a Discard Changes confirmation is open for. */
  confirmDiscard: string[] | null
  /** Commit dialog: open while true. It commits the active repository's index. */
  commitOpen: boolean
  /** View toggle: hide git-ignored files. Saved in this browser. */
  hideIgnored: boolean
  /** The items the bulk rename dialog is open for. */
  bulkRename: string[] | null
  /** Saved workspaces, without the hidden last session. */
  workspaces: Workspace[]
  /** Reopen the last session's tabs at startup, and autosave them as they change. */
  restoreOnLaunch: boolean
  /** The workspace last opened or saved in this window, for Update Workspace. */
  currentWorkspace: string | null
  workspaceDialog: WorkspaceDialog | null
  focusRequest: { target: FocusTarget; n: number }
  /** The pane and folder row an external drag is over, for the drop highlight. */
  dropHover: { tabId: number; dir: string | null } | null
}

interface Actions {
  init(): Promise<void>
  activeTab(): Tab
  focusedTabId(): number
  visibleTabs(): Tab[]
  toggleSplit(): void
  focusPane(pane: Pane): void
  transferToOtherPane(cut: boolean): Promise<void>
  startTransfer(paths: string[], dest: string, cut: boolean): Promise<void>
  clearFinishedOps(): void
  updateTab(id: number, patch: Partial<Tab> | ((t: Tab) => Partial<Tab>)): void

  newTab(dir?: string): void
  closeTab(id: number): void
  setActiveTab(id: number): void
  switchToTabIndex(i: number): void

  navigate(path: string, opts?: { tabId?: number; record?: boolean }): Promise<boolean>
  goBack(): void
  goForward(): void
  goUp(): void
  refresh(tabId?: number): Promise<boolean>
  openNode(node: VNode): Promise<void>
  openSelection(): Promise<void>

  setQuery(q: string): void
  runSearch(query?: string): Promise<void>
  clearSearch(): void
  onSearchEvent(e: SearchEvent): void

  setSelection(paths: string[], anchor?: string | null, cursor?: string | null): void
  setSort(col: SortColumn): void

  requestFocus(target: FocusTarget): void
  set(patch: Partial<State>): void

  copySelection(cut: boolean): void
  paste(): Promise<void>
  duplicateSelection(): Promise<void>
  compressSelection(): Promise<void>
  /** Extract a whole archive into `destDir`, as a folder named after it. */
  extractArchive(archive: string, destDir: string): Promise<void>
  trashPaths(paths: string[]): Promise<void>
  deletePermanently(paths: string[]): Promise<void>
  submitPrompt(value: string): Promise<void>
  /** Rename `[item, newName]` pairs as one batch and record it as one undo entry. Resolves true when all were renamed. */
  renameBatch(renames: [string, string][]): Promise<boolean>
  pushUndo(entry: UndoEntry): void
  undo(): Promise<void>
  redo(): Promise<void>
  /** Record an operation's outcomes as one undoable change. */
  recordOp(p: OpProgress): void
  /** Take ownership of an operation that started in this window. */
  claimOp(id: number): void

  addFavorite(path: string): void
  removeFavorite(path: string): void
  saveSearch(name: string, query: string): void
  removeSavedSearch(name: string): void
  saveSmartFolder(name: string, query: string): void
  /** Pin a saved search with this name, or save it pinned if there is none. */
  pinSmartFolder(name: string, query: string): void
  setSmartPinned(name: string, pinned: boolean): void
  renameSmartFolder(name: string, to: string): void
  editSmartQuery(name: string, query: string): void
  openSmartFolder(search: SavedSearch): Promise<void>
  onIndexChanged(): void

  calculateDirSizes(paths: string[], fresh?: boolean, background?: boolean): void
  /** A streamed batch for `tab`. Batches for a superseded request are ignored. */
  onListingChunk(tab: number, chunk: ListingChunk): void
  /** Stops waiting for the folder and goes back to where the tab was. */
  abandonListing(tab: number): void
  openWindow(dir?: string): void
  moveTabToWindow(id?: number): void
  /** Move a tab to insertion point `to` (0..n) among the visible tabs. */
  moveTab(id: number, to: number): void
  onDirSize(path: string, size: number | null): void
  onDirChanged(dir: string): void
  onOpProgress(p: OpProgress): void
  setIndex(s: IndexState): void
  openDuplicateFinder(): void
  closeDuplicateFinder(): void
  startDuplicateScan(scope: 'folder' | 'index'): Promise<void>
  cancelDuplicateScan(): void
  onDuplicatesProgress(p: DupProgress): void
  onDuplicatesDone(d: DupDone): void
  /** Replace the Trash selection. Refused, and not applied, if it would leave a set with no copy. */
  setDupSelection(next: Set<string>): boolean
  trashDuplicates(): Promise<void>
  /** Open the compare dialog on the primary folder and the split pane's folder (if any). */
  openCompare(): void
  closeCompare(): void
  startCompare(): Promise<void>
  cancelCompare(): void
  onCompareProgress(p: CompareProgress): void
  onCompareDone(d: CompareDone): void
  setComparePick(rel: string, pick: ComparePick | null): void
  applyCompareBulk(bulk: CompareBulk): Promise<void>
  /** Plan the sync from the picks. Nothing changes on disk until it is run. */
  reviewCompareSync(): Promise<void>
  runCompareSync(): Promise<void>
  cancelCompareSync(): void
  onSyncProgress(p: SyncProgress): void
  openShell(target: Omit<ShellTarget, 'opened'>): void
  closeShell(): void
  /** Starts the run. Throws, leaving the input open, if the command can't run. A plugin run is not added to history. */
  runShell(template: string, stopOnError: boolean, plugin?: PluginRunRef): Promise<void>
  cancelShell(): void
  closeShellPanel(): void
  onShellLines(e: ShellLinesEvent): void
  onShellExit(e: ShellExitEvent): void
  onShellDone(e: ShellDoneEvent): void
  onSyncDone(d: SyncDone): void
  refreshGit(tabId?: number): Promise<void>
  refreshIgnored(tabId: number, items: VNode[]): Promise<void>
  gitStage(paths: string[]): Promise<void>
  gitUnstage(paths: string[]): Promise<void>
  gitDiscard(paths: string[]): Promise<void>
  gitCommit(message: string): Promise<boolean>
  setHideIgnored(on: boolean): void
  syncWatches(): void
  /** Turn Vim-style keys on or off and save the choice. */
  setVimMode(on: boolean): void
  /** Open the quick filter on the focused folder and move focus into its box. */
  openQuickFilter(): void
  /** Switch a tab between the list and the columns view. Defaults to the focused tab. */
  setView(view: ViewMode, tabId?: number): void
  /** Columns view: select a row in column `col`. Opening a folder opens the column after it; a file closes the columns after its own. */
  selectColumnRow(tabId: number, col: number, path: string, isDir: boolean): void
  /** Save the window's layout under a name, replacing a workspace of that name. */
  saveWorkspace(name: string): Promise<void>
  /** Re-save the layout to the workspace last opened or saved in this window. */
  updateWorkspace(): Promise<void>
  /** Open a workspace now, or ask first when it would replace more than one tab. */
  requestOpenWorkspace(name: string): void
  /** Replace this window's tabs with a workspace's. */
  openWorkspace(name: string): Promise<void>
  deleteWorkspace(name: string): Promise<void>
  setRestoreOnLaunch(on: boolean): void
}

// Every window has its own store, but search and cancel are keyed by tab id
// in the shared backend, so each window starts its ids somewhere random.
let nextTabId = Math.floor(Math.random() * 2 ** 40) * 1000 + 1

declare global {
  interface Window {
    /** Set by the backend when it opens an extra window at a folder. */
    __FERRIS_START_DIR__?: string
  }
}

/** Folder sizes kept before old entries are dropped. */
const DIR_SIZE_CAP = 5000

/** Run ids are picked here, so output that arrives before start_shell returns still matches. */
let shellOpenSeq = 0

function setInvocation(list: ShellInvocation[], inv: number, patch: Partial<ShellInvocation>): ShellInvocation[] {
  const next = [...list]
  while (next.length <= inv) next.push({ command: '', status: 'pending', code: null })
  next[inv] = { ...next[inv], ...patch }
  return next
}
/** Changes kept for undo. */
const UNDO_LIMIT = 100

/** The Hide Git-Ignored Files switch. Storage can be blocked, so every access is guarded. */
const HIDE_IGNORED_KEY = 'ferris.hideIgnored'
function loadHideIgnored(): boolean {
  try {
    return localStorage.getItem(HIDE_IGNORED_KEY) === '1'
  } catch {
    return false
  }
}
function saveHideIgnored(on: boolean) {
  try {
    localStorage.setItem(HIDE_IGNORED_KEY, on ? '1' : '0')
  } catch {
    // Not remembered next time; the toggle still works for this session.
  }
}
/** Serialises undo and redo: each one reads and rewrites the stacks. */
let undoBusy = false
/** Smart folders re-run this long after the last index change, so a burst of changes costs one search. */
const SMART_REFRESH_MS = 500
let smartTimer: ReturnType<typeof setTimeout> | null = null

/**
 * Run a tab's smart folder query. `reset` clears the list first; a live
 * refresh keeps the old results on screen until new ones arrive.
 */
async function runSmart(id: number, reset: boolean) {
  const st = useExplorer.getState()
  const t = st.tabs.find((x) => x.id === id)
  if (!t?.smart) return
  const q = t.smart.query
  st.updateTab(
    id,
    reset
      ? {
          query: q,
          activeQuery: q,
          results: [],
          searching: true,
          searchError: null,
          loading: false,
          listingError: null,
          selected: [],
          anchor: null,
          cursor: null,
        }
      : { searching: true },
  )
  try {
    const started = await api.startSearch(id, q, t.dir)
    useExplorer.getState().updateTab(id, { interpretedAs: started.interpreted_as, queryErrors: started.errors })
  } catch (e) {
    useExplorer.getState().updateTab(id, { searching: false, searchError: errorText(e) })
  }
}

function makeTab(dir: string): Tab {
  return {
    id: nextTabId++,
    dir,
    items: [],
    loading: true,
    listing: null,
    listingError: null,
    history: [dir],
    historyIndex: 0,
    query: '',
    activeQuery: null,
    results: [],
    searching: false,
    searchError: null,
    interpretedAs: null,
    queryErrors: [],
    selected: [],
    anchor: null,
    cursor: null,
    sort: { col: 'name', dir: 'asc' },
    git: null,
    ignored: [],
    archive: null,
    smart: null,
    filter: null,
    view: 'list',
    colPath: [],
  }
}

// A smart folder's history entry is its JSON behind this prefix, so it never
// reads as a real path.
const SMART_PREFIX = 'smart:'

function encodeSmart(sf: SmartFolder): string {
  return SMART_PREFIX + JSON.stringify(sf)
}

/** The smart folder a history entry or path names, or null for a real folder. */
function decodeSmart(path: string): SmartFolder | null {
  if (!path.startsWith(SMART_PREFIX)) return null
  try {
    const v = JSON.parse(path.slice(SMART_PREFIX.length)) as SmartFolder
    return typeof v.name === 'string' && typeof v.query === 'string' && typeof v.dir === 'string' ? v : null
  } catch {
    return null
  }
}

function reportError(title: string, e: unknown) {
  toast.error(title, { description: errorText(e) })
}

/** Archives are read-only: say so and stop the change before it is asked of the backend. */
function refuseInArchive(t: Tab, what: string): boolean {
  if (!t.archive) return false
  toast.error(`${what} isn't available inside an archive`, {
    description: 'Archives are read-only. Copy the items out to change them.',
  })
  return true
}

/** "“a.txt”" for one path, "3 items" for several. */
function describeItems(paths: string[]): string {
  return paths.length === 1 ? `“${basename(paths[0])}”` : `${paths.length} items`
}

/** Sort columns a saved workspace may name. */
const SORT_COLS: SortColumn[] = ['name', 'size', 'modified', 'kind']
/** The hidden workspace behind "Restore tabs on launch". Must match persistence::LAST_SESSION. */
const LAST_SESSION = '__last__'
/** A layout change is autosaved this long after the last one. */
const SESSION_SAVE_MS = 2000

/**
 * The window's layout as a workspace: tabs in order, the active tab, and the
 * split pane. Smart folders are saved as their reference, and selections by name.
 */
export function captureWorkspace(name: string, s: State = useExplorer.getState()): Workspace {
  const tabs: WorkspaceTab[] = s.tabs.map((t) => ({
    path: t.smart ? encodeSmart(t.smart) : t.dir,
    view: t.view,
    sort_col: t.sort.col,
    sort_dir: t.sort.dir,
    col_path: t.view === 'columns' && !t.smart ? t.colPath : [],
    selected: t.selected.map(basename),
    split: !!t.split,
  }))
  return {
    name,
    tabs,
    active: Math.max(0, s.tabs.findIndex((t) => t.id === s.activeTabId)),
    focused_split: s.focusedPane === 'split' && s.splitTabId !== null,
  }
}

/** Where a restored tab's state goes once its listing has loaded. */
interface Restore {
  colPath: string[]
  names: string[]
}

interface Layout {
  tabs: Tab[]
  restores: Map<number, Restore>
  activeId: number
  splitId: number | null
  focusedPane: Pane
  /** Tabs whose folder was gone, so opened somewhere else. */
  moved: number
}

/**
 * Rebuild a workspace's tabs. A path that no longer exists opens in its nearest
 * existing ancestor, and is counted in `moved`. Returns null when nothing is left to open.
 */
async function layoutFrom(ws: Workspace): Promise<Layout | null> {
  const plan: { wt: WorkspaceTab; smart: SmartFolder | null; dir: string; colPath: string[] }[] = []
  for (const wt of ws.tabs) {
    const isRef = wt.path.startsWith(SMART_PREFIX)
    const smart = isRef ? decodeSmart(wt.path) : null
    if (isRef && !smart) continue
    const dir = smart ? smart.dir : wt.path
    if (!dir) continue
    const colPath = !smart && wt.view === 'columns' ? wt.col_path.filter(Boolean) : []
    plan.push({ wt, smart, dir, colPath })
  }
  if (plan.length === 0) return null

  const resolved = await api.resolveRestoreDirs(plan.flatMap((p) => [p.dir, ...p.colPath]))
  let at = 0
  let moved = 0
  let splitSeen = false
  const tabs: Tab[] = []
  const restores = new Map<number, Restore>()
  for (const p of plan) {
    const dir = resolved[at++] ?? p.dir
    // Column folders are kept only up to the first one that is gone.
    const colPath: string[] = []
    let intact = dir === p.dir
    for (const want of p.colPath) {
      const got = resolved[at++]
      if (intact && got === want) colPath.push(want)
      else intact = false
    }
    if (dir !== p.dir || colPath.length !== p.colPath.length) moved++
    const smart = p.smart ? { ...p.smart, dir } : null
    const split: boolean = !splitSeen && p.wt.split
    splitSeen ||= split
    const tab: Tab = {
      ...makeTab(dir),
      history: [smart ? encodeSmart(smart) : dir],
      smart,
      sort: {
        col: SORT_COLS.find((c) => c === p.wt.sort_col) ?? 'name',
        dir: p.wt.sort_dir === 'desc' ? 'desc' : 'asc',
      },
      view: !smart && p.wt.view === 'columns' ? 'columns' : 'list',
      split,
    }
    tabs.push(tab)
    restores.set(tab.id, { colPath, names: p.wt.selected ?? [] })
  }
  // The split pane is never the only tab, and the active one is never the split pane.
  if (tabs.every((t) => t.split)) tabs[0].split = false
  const active = tabs[ws.active] && !tabs[ws.active].split ? tabs[ws.active] : tabs.find((t) => !t.split)!
  const split = tabs.find((t) => t.split) ?? null
  return {
    tabs,
    restores,
    activeId: active.id,
    splitId: split?.id ?? null,
    focusedPane: ws.focused_split && split ? 'split' : 'primary',
    moved,
  }
}

/** Load each restored tab's listing, then put back its column path and selection. */
async function loadLayout(tabs: Tab[], restores: Map<number, Restore>) {
  await Promise.all(
    tabs.map(async (t) => {
      await useExplorer.getState().navigate(t.smart ? encodeSmart(t.smart) : t.dir, { tabId: t.id, record: false })
      const r = restores.get(t.id)
      if (!r) return
      useExplorer.getState().updateTab(t.id, (cur) => {
        const picked = cur.smart ? [] : cur.items.filter((n) => r.names.includes(n.name))
        return {
          colPath: r.colPath,
          selected: picked.map((n) => n.path),
          anchor: picked[0]?.path ?? null,
          cursor: picked[0]?.path ?? null,
        }
      })
    }),
  )
}

function reportMoved(name: string, moved: number) {
  if (moved === 0) return
  toast(`Some folders in “${name}” no longer exist`, {
    description: 'Opened the nearest folder that still exists.',
  })
}

let sessionWatching = false
let sessionTimer: ReturnType<typeof setTimeout> | null = null
let sessionKey = ''

/**
 * In the main window, when "Restore tabs on launch" is on, autosave the layout
 * as the hidden last session `SESSION_SAVE_MS` after it last changed. Windows
 * opened at a folder don't save, or they would overwrite the main window's session.
 */
function watchSession() {
  if (sessionWatching || window.__FERRIS_START_DIR__ !== undefined) return
  sessionWatching = true
  useExplorer.subscribe((s, prev) => {
    if (!s.restoreOnLaunch) return
    if (
      s.restoreOnLaunch === prev.restoreOnLaunch &&
      s.tabs === prev.tabs &&
      s.activeTabId === prev.activeTabId &&
      s.splitTabId === prev.splitTabId &&
      s.focusedPane === prev.focusedPane
    ) {
      return
    }
    const key = JSON.stringify(captureWorkspace(LAST_SESSION, s))
    if (key === sessionKey) return
    sessionKey = key
    if (sessionTimer !== null) clearTimeout(sessionTimer)
    sessionTimer = setTimeout(() => {
      sessionTimer = null
      void api.saveLastSession(captureWorkspace(LAST_SESSION)).catch(() => {})
    }, SESSION_SAVE_MS)
  })
}

export const useExplorer = create<State & Actions>()((set, get) => ({
  ready: false,
  home: '',
  drives: [],
  tabs: [],
  activeTabId: 0,
  favorites: [],
  savedSearches: [],
  smartTick: 0,
  history: [],
  index: { message: '', indexing: false, syncing: false, exists: false },
  ops: {},
  opsMeta: {},
  opsPanelOpen: false,
  splitTabId: null,
  focusedPane: 'primary',
  dirSizes: {},
  clipboard: null,
  undoStack: [],
  redoStack: [],
  doneEarly: {},
  showHidden: false,
  showDetails: true,
  paletteOpen: false,
  quickOpenOpen: false,
  vimMode: false,
  jumpOpen: false,
  shortcutsOpen: false,
  pluginsOpen: false,
  pluginReport: null,
  pluginRequest: null,
  spaceFinderOpen: false,
  spaceFinderView: 'map',
  duplicateFinderOpen: false,
  dupScope: 'folder',
  dupRoot: '',
  dupStatus: 'idle',
  dupScanId: null,
  dupProgress: null,
  dupScan: null,
  dupError: null,
  dupSelected: new Set(),
  compareOpen: false,
  compareLeft: '',
  compareRight: '',
  compareBy: 'metadata',
  compareStatus: 'idle',
  compareId: null,
  compareProgress: null,
  comparison: null,
  compareError: null,
  compareHideIdentical: true,
  comparePicks: {},
  comparePlan: null,
  syncLabel: '',
  syncStatus: 'idle',
  syncId: null,
  syncProgress: null,
  syncOutcome: null,
  syncError: null,
  shellTarget: null,
  shellHistory: [],
  shellPanelOpen: false,
  shellRun: null,
  prompt: null,
  confirmDelete: null,
  confirmDiscard: null,
  commitOpen: false,
  hideIgnored: loadHideIgnored(),
  bulkRename: null,
  workspaces: [],
  restoreOnLaunch: false,
  currentWorkspace: null,
  workspaceDialog: null,
  focusRequest: { target: 'list', n: 0 },
  dropHover: null,

  async init() {
    const [places, settings, index, keys, workspaces] = await Promise.all([
      api.places(),
      api.loadSettings(),
      api.indexStatus(),
      api.loadKeyPrefs(),
      api.loadWorkspaces(),
    ])
    const start = window.__FERRIS_START_DIR__ ?? places.home
    const base = {
      ready: true,
      home: places.home,
      drives: places.drives,
      favorites: settings.favorites,
      savedSearches: settings.saved_searches,
      history: settings.history,
      vimMode: keys.vim_mode,
      index,
      workspaces: workspaces.workspaces,
      restoreOnLaunch: workspaces.restore_on_launch,
    }
    // A window opened at a folder shows that folder; the main window restores the last session.
    let restored: Layout | null = null
    if (workspaces.restore_on_launch && window.__FERRIS_START_DIR__ === undefined) {
      try {
        const last = await api.loadLastSession()
        restored = last ? await layoutFrom(last) : null
      } catch {
        restored = null
      }
    }
    if (restored) {
      set({
        ...base,
        tabs: restored.tabs,
        activeTabId: restored.activeId,
        splitTabId: restored.splitId,
        focusedPane: restored.focusedPane,
      })
      await loadLayout(restored.tabs, restored.restores)
      reportMoved('your last session', restored.moved)
    } else {
      const tab = makeTab(start)
      set({ ...base, tabs: [tab], activeTabId: tab.id })
      await get().navigate(start, { tabId: tab.id, record: false })
    }
    watchSession()
  },

  /** The tab in the focused pane: the active tab, or the split pane's. */
  activeTab() {
    const { tabs, activeTabId, splitTabId, focusedPane } = get()
    const id = focusedPane === 'split' && splitTabId !== null ? splitTabId : activeTabId
    return tabs.find((t) => t.id === id) ?? tabs[0]
  },

  focusedTabId() {
    return get().activeTab().id
  },

  visibleTabs() {
    return get().tabs.filter((t) => !t.split)
  },

  toggleSplit() {
    const { splitTabId, tabs, activeTabId } = get()
    if (splitTabId !== null) {
      void api.cancelSearch(splitTabId)
      set({ tabs: tabs.filter((t) => t.id !== splitTabId), splitTabId: null, focusedPane: 'primary' })
      get().syncWatches()
      return
    }
    const primary = tabs.find((t) => t.id === activeTabId) ?? tabs[0]
    const tab = { ...makeTab(primary.dir), split: true }
    set({ tabs: [...tabs, tab], splitTabId: tab.id, focusedPane: 'split' })
    void get().navigate(tab.dir, { tabId: tab.id, record: false })
  },

  focusPane(pane) {
    if (pane === 'split' && get().splitTabId === null) return
    if (get().focusedPane !== pane) set({ focusedPane: pane })
  },

  async transferToOtherPane(cut) {
    const { splitTabId, activeTabId, focusedPane, tabs } = get()
    if (splitTabId === null) return
    const from = get().activeTab()
    const otherId = focusedPane === 'split' ? activeTabId : splitTabId
    const other = tabs.find((t) => t.id === otherId)
    if (!other || from.selected.length === 0) return
    if (refuseInArchive(other, cut ? 'Moving into it' : 'Copying into it')) return
    await get().startTransfer(from.selected, other.dir, cut)
  },

  async startTransfer(paths, dest, cut) {
    if (paths.length === 0) return
    if (cut && refuseInArchive(get().activeTab(), 'Moving')) return
    try {
      const id = await api.transferPaths(paths, dest, cut)
      const what = paths.length === 1 ? basename(paths[0]) : `${paths.length} items`
      set((s) => ({
        opsMeta: {
          ...s.opsMeta,
          [id]: { title: `${cut ? 'Move' : 'Copy'} ${what} to ${basename(dest) || dest}`, started: Date.now() },
        },
      }))
      get().claimOp(id)
    } catch (e) {
      reportError(cut ? 'Move failed' : 'Copy failed', e)
    }
  },

  clearFinishedOps() {
    set((s) => {
      const ops: typeof s.ops = {}
      for (const [id, op] of Object.entries(s.ops)) if (!op.done) ops[Number(id)] = op
      return { ops }
    })
  },

  updateTab(id, patch) {
    set((s) => ({
      tabs: s.tabs.map((t) =>
        t.id === id ? { ...t, ...(typeof patch === 'function' ? patch(t) : patch) } : t,
      ),
    }))
  },

  newTab(dir) {
    const tab = makeTab(dir ?? get().activeTab()?.dir ?? get().home)
    set((s) => ({ tabs: [...s.tabs, tab], activeTabId: tab.id }))
    void get().navigate(tab.dir, { tabId: tab.id, record: false })
  },

  closeTab(id) {
    const { activeTabId, tabs } = get()
    const visible = get().visibleTabs()
    if (visible.length <= 1 || id === get().splitTabId) return
    void api.cancelSearch(id)
    const idx = visible.findIndex((t) => t.id === id)
    const rest = visible.filter((t) => t.id !== id)
    const nextActive =
      activeTabId === id ? rest[Math.min(idx, rest.length - 1)].id : activeTabId
    set({ tabs: tabs.filter((t) => t.id !== id), activeTabId: nextActive, focusedPane: 'primary' })
    get().syncWatches()
  },

  setActiveTab(id) {
    set({ activeTabId: id, focusedPane: 'primary' })
  },

  switchToTabIndex(i) {
    const tab = get().visibleTabs()[i]
    if (tab) set({ activeTabId: tab.id, focusedPane: 'primary' })
  },

  /** Resolves false when a newer navigation or refresh superseded this one before its listing finished. */
  async navigate(path, opts = {}) {
    const tabId = opts.tabId ?? get().focusedTabId()
    const record = opts.record ?? true
    const smart = decodeSmart(path)
    void api.cancelSearch(tabId)
    get().updateTab(tabId, (t) => {
      let history = t.history
      let historyIndex = t.historyIndex
      if (record && t.history[t.historyIndex] !== path) {
        history = [...t.history.slice(0, t.historyIndex + 1), path]
        historyIndex = history.length - 1
      }
      return {
        dir: smart ? smart.dir : path,
        smart,
        history,
        historyIndex,
        loading: true,
        listing: null,
        listingError: null,
        query: '',
        activeQuery: null,
        results: [],
        searching: false,
        searchError: null,
        interpretedAs: null,
        queryErrors: [],
        selected: [],
        anchor: null,
        cursor: null,
        filter: null,
        colPath: [],
        ignored: [],
      }
    })
    get().syncWatches()
    void get().refreshGit(tabId)
    const completed = await get().refresh(tabId)
    if (!completed) return false
    const visited = get().tabs.find((x) => x.id === tabId)
    if (visited && !visited.smart && !visited.archive && !visited.listingError) {
      void api.recordVisit(visited.dir).catch(() => {})
    }
    return true
  },

  goBack() {
    const t = get().activeTab()
    if (t.historyIndex <= 0) return
    const historyIndex = t.historyIndex - 1
    get().updateTab(t.id, { historyIndex })
    void get().navigate(t.history[historyIndex], { record: false })
  },

  goForward() {
    const t = get().activeTab()
    if (t.historyIndex >= t.history.length - 1) return
    const historyIndex = t.historyIndex + 1
    get().updateTab(t.id, { historyIndex })
    void get().navigate(t.history[historyIndex], { record: false })
  },

  goUp() {
    const t = get().activeTab()
    if (t.smart) {
      void get().navigate(t.smart.dir)
      return
    }
    const parent = parentOf(t.dir)
    if (parent) {
      const from = t.dir
      void get()
        .navigate(parent)
        .then((completed) => {
          // A newer navigation took over: its selection must not be replaced by this one.
          if (completed) get().setSelection([from], from, from)
        })
    }
  },

  /** Resolves true when the tab's listing finished, false when it was superseded or abandoned. */
  refresh(tabId) {
    const id = tabId ?? get().focusedTabId()
    const tab = get().tabs.find((t) => t.id === id)
    if (!tab) return Promise.resolve(false)
    if (tab.smart) return runSmart(id, true).then(() => true)
    const dir = tab.dir
    const request = nextListingRequest()
    // A new listing supersedes the tab's previous one: that one is cancelled
    // on the Rust side and its waiters are released now, not left hanging.
    const previous = activeListings.get(id)
    if (previous) {
      void api.cancelListing(id, previous.request).catch(() => {})
      previous.finish(false)
    }
    const done = new Promise<boolean>((finish) => {
      activeListings.set(id, { request, dir, batches: 0, finish, archive: api.archiveContext(dir).catch(() => null) })
    })
    get().updateTab(id, { loading: true, listing: null })
    api
      .listDirStream(id, request, dir, (chunk) => get().onListingChunk(id, chunk))
      .catch((e) => get().onListingChunk(id, { tab: id, request, entries: [], done: true, error: errorText(e) }))
    return done
  },

  onListingChunk(tabId, chunk) {
    const active = activeListings.get(tabId)
    if (!active || active.request !== chunk.request) return
    const { dir } = active
    const first = active.batches === 0
    active.batches += 1
    if (chunk.error !== null) {
      activeListings.delete(tabId)
      const error = chunk.error
      void active.archive.then((archive) =>
        get().updateTab(tabId, (t) =>
          t.dir === dir ? { items: [], loading: false, listing: null, listingError: error, archive } : {},
        ),
      )
      active.finish(true)
      return
    }
    // Rows keep arriving in folder order while streaming; sorting waits for the end.
    // The first batch replaces the rows the tab was showing.
    get().updateTab(tabId, (t) => {
      if (t.dir !== dir) return {}
      const items = first ? chunk.entries : t.items.concat(chunk.entries)
      return chunk.done ? { items } : { items, listing: items.length }
    })
    if (chunk.done) void finishListing(tabId, active)
  },

  abandonListing(tabId) {
    const active = activeListings.get(tabId)
    if (active) {
      activeListings.delete(tabId)
      void api.cancelListing(tabId, active.request).catch(() => {})
      active.finish(false)
    }
    const t = get().tabs.find((x) => x.id === tabId)
    if (!t) return
    if (t.historyIndex > 0) {
      const historyIndex = t.historyIndex - 1
      get().updateTab(tabId, { historyIndex })
      void get().navigate(t.history[historyIndex], { tabId, record: false })
      return
    }
    const parent = parentOf(t.dir)
    if (parent) {
      void get().navigate(parent, { tabId })
      return
    }
    get().updateTab(tabId, { loading: false, listing: null, listingError: 'Stopped waiting for this folder.' })
  },

  async openNode(node) {
    if (get().activeTab().archive) {
      // Entries inside an archive are not files on disk; folders still open.
      if (node.is_dir) {
        await get().navigate(node.path)
        return
      }
      toast('Copy it out of the archive to open it')
      return
    }
    const navigable = node.is_dir || (node.is_symlink && (await api.isNavigable(node.path)))
    if (navigable) {
      await get().navigate(node.path)
    } else if (!node.is_symlink && isArchiveName(node.name)) {
      // An archive opens as a folder; its contents are read in place.
      await get().navigate(node.path)
    } else {
      api.openPath(node.path).catch((e) => reportError('Could not open file', e))
    }
  },

  async openSelection() {
    const t = get().activeTab()
    const pool = t.activeQuery ? t.results : t.items
    const nodes = pool.filter((n) => t.selected.includes(n.path))
    if (nodes.length === 1) return get().openNode(nodes[0])
    if (t.archive) {
      toast('Copy items out of the archive to open them')
      return
    }
    for (const n of nodes) {
      if (!n.is_dir) api.openPath(n.path).catch((e) => reportError('Could not open file', e))
    }
  },

  setQuery(q) {
    const t = get().activeTab()
    get().updateTab(t.id, { query: q })
    if (!q.trim() && t.activeQuery) get().clearSearch()
  },

  async runSearch(query) {
    const t = get().activeTab()
    if (t.archive) {
      toast.error("Search isn't available inside an archive")
      return
    }
    const q = (query ?? t.query).trim()
    if (!q) {
      get().clearSearch()
      return
    }
    get().updateTab(t.id, {
      query: q,
      activeQuery: q,
      searching: true,
      results: [],
      searchError: null,
      selected: [],
      anchor: null,
      cursor: null,
      // A typed search replaces the smart folder, so live refreshes stop.
      smart: null,
    })
    try {
      const started = await api.startSearch(t.id, q, t.dir)
      get().updateTab(t.id, {
        interpretedAs: started.interpreted_as,
        queryErrors: started.errors,
      })
      api.pushHistory(q).then((history) => set({ history }))
    } catch (e) {
      get().updateTab(t.id, { searching: false, searchError: errorText(e) })
    }
  },

  clearSearch() {
    const t = get().activeTab()
    void api.cancelSearch(t.id)
    get().updateTab(t.id, {
      query: '',
      activeQuery: null,
      smart: null,
      results: [],
      searching: false,
      searchError: null,
      interpretedAs: null,
      queryErrors: [],
      selected: [],
    })
  },

  onSearchEvent(e) {
    get().updateTab(e.tab, (t) => {
      if (t.activeQuery !== e.query) return {}
      return {
        results: e.results,
        searching: !e.complete,
        searchError: e.error,
      }
    })
  },

  setSelection(paths, anchor, cursor) {
    const t = get().activeTab()
    get().updateTab(t.id, {
      selected: paths,
      anchor: anchor === undefined ? t.anchor : anchor,
      cursor: cursor === undefined ? (paths[paths.length - 1] ?? null) : cursor,
    })
  },

  setSort(col) {
    const t = get().activeTab()
    const dir: SortDir = t.sort.col === col && t.sort.dir === 'asc' ? 'desc' : 'asc'
    get().updateTab(t.id, { sort: { col, dir } })
    // Sorting by size needs every folder's size; the other sorts do not.
    if (col === 'size' && !t.archive && !t.smart) {
      const { dirSizes } = get()
      const wanted = t.items.filter((n) => n.is_dir && typeof dirSizes[n.path] !== 'number' && dirSizes[n.path] !== 'pending').map((n) => n.path)
      get().calculateDirSizes(wanted, false, true)
    }
  },

  requestFocus(target) {
    set((s) => ({ focusRequest: { target, n: s.focusRequest.n + 1 } }))
  },

  set(patch) {
    set(patch)
  },

  copySelection(cut) {
    const t = get().activeTab()
    const paths = t.selected
    if (paths.length === 0) return
    if (cut && refuseInArchive(t, 'Cutting')) return
    set({ clipboard: { cut, paths } })
    toast(`${cut ? 'Cut' : 'Copied'} ${paths.length} item${paths.length === 1 ? '' : 's'}`)
  },

  async paste() {
    const clip = get().clipboard
    if (!clip) return
    if (refuseInArchive(get().activeTab(), 'Pasting')) return
    await get().startTransfer(clip.paths, get().activeTab().dir, clip.cut)
    if (clip.cut) set({ clipboard: null })
  },

  async duplicateSelection() {
    const paths = get().activeTab().selected
    if (!paths.length) return
    if (refuseInArchive(get().activeTab(), 'Duplicating')) return
    try {
      const id = await api.duplicatePaths(paths)
      set((s) => ({
        opsMeta: { ...s.opsMeta, [id]: { title: `Duplicate ${describeItems(paths)}`, started: Date.now() } },
      }))
      get().claimOp(id)
    } catch (e) {
      reportError('Duplicate failed', e)
    }
  },

  async compressSelection() {
    const t = get().activeTab()
    if (refuseInArchive(t, 'Compressing')) return
    const first = t.selected[0]
    if (first) await api.compressPath(first).catch((e) => reportError('Compress failed', e))
  },

  async trashPaths(paths) {
    if (paths.length === 0) return
    const t = get().activeTab()
    if (refuseInArchive(t, 'Deleting')) return
    try {
      const out = await api.trashPaths(paths)
      if (out.failures.length) {
        toast.error(`Could not move ${out.failures.length} item${out.failures.length === 1 ? '' : 's'} to the Trash`, {
          description: out.failures.map(([p, err]) => `${basename(p)}: ${err}`).join('\n'),
        })
      }
      if (out.done.length) {
        const what = describeItems(out.done)
        get().pushUndo({ label: `move ${what} to the Trash`, change: { kind: 'trash', paths: out.done } })
        toast(`Moved ${what} to the Trash`, { action: { label: 'Undo', onClick: () => void get().undo() } })
      }
    } catch (e) {
      reportError('Move to Trash failed', e)
    }
    get().updateTab(t.id, { selected: [], anchor: null })
    await get().refresh(t.id)
    if (t.activeQuery) void get().runSearch(t.activeQuery)
  },

  pushUndo(entry) {
    set((s) => ({ undoStack: [...s.undoStack, entry].slice(-UNDO_LIMIT), redoStack: [] }))
  },

  async undo() {
    const { undoStack } = get()
    const entry = undoStack[undoStack.length - 1]
    if (!entry || undoBusy) return
    undoBusy = true
    set({ undoStack: undoStack.slice(0, -1) })
    try {
      await reverseChange(entry.change)
      if (isRedoable(entry.change)) set((s) => ({ redoStack: [...s.redoStack, entry] }))
      toast(`Undid ${entry.label}`)
    } catch (e) {
      if (e instanceof PartialChangeError) {
        // What did get undone can be redone; what is left stays to be undone.
        set((s) => ({
          undoStack: [...s.undoStack, { ...entry, change: e.rest }],
          redoStack: isRedoable(e.done) ? [...s.redoStack, { ...entry, change: e.done }] : s.redoStack,
        }))
        toast.error(`Only part of ${entry.label} was undone`, { description: e.message })
      } else {
        // Nothing was undone, so the entry goes back whole and can be retried.
        set((s) => ({ undoStack: [...s.undoStack, entry] }))
        toast.error(`Can't undo ${entry.label}`, { description: errorText(e) })
      }
    } finally {
      undoBusy = false
      for (const t of get().tabs) void get().refresh(t.id)
    }
  },

  async redo() {
    const { redoStack } = get()
    const entry = redoStack[redoStack.length - 1]
    if (!entry || undoBusy) return
    undoBusy = true
    set({ redoStack: redoStack.slice(0, -1) })
    try {
      await replayChange(entry.change)
      set((s) => ({ undoStack: [...s.undoStack, entry] }))
      toast(`Redid ${entry.label}`)
    } catch (e) {
      if (e instanceof PartialChangeError) {
        // What did get redone can be undone; what is left stays to be redone.
        set((s) => ({
          redoStack: [...s.redoStack, { ...entry, change: e.rest }],
          undoStack: [...s.undoStack, { ...entry, change: e.done }],
        }))
        toast.error(`Only part of ${entry.label} was redone`, { description: e.message })
      } else {
        set((s) => ({ redoStack: [...s.redoStack, entry] }))
        toast.error(`Can't redo ${entry.label}`, { description: errorText(e) })
      }
    } finally {
      undoBusy = false
      for (const t of get().tabs) void get().refresh(t.id)
    }
  },

  recordOp(p) {
    if (p.outcomes.length === 0) return
    const sources = p.outcomes.map(([src]) => src)
    if (p.kind === 'move') {
      get().pushUndo({ label: `move ${describeItems(sources)}`, change: { kind: 'move', pairs: p.outcomes } })
    } else {
      const verb = p.kind === 'duplicate' ? 'duplicate' : p.kind === 'extract' ? 'extract' : 'copy'
      get().pushUndo({
        label: `${verb} ${describeItems(sources)}`,
        change: { kind: 'copy', created: p.outcomes.map(([, dest]) => dest) },
      })
    }
  },

  claimOp(id) {
    const early = get().doneEarly[id]
    if (!early) return
    set((s) => {
      const doneEarly = { ...s.doneEarly }
      delete doneEarly[id]
      return { doneEarly }
    })
    get().recordOp(early)
  },

  async deletePermanently(paths) {
    if (refuseInArchive(get().activeTab(), 'Deleting')) return
    try {
      const failures = await api.deletePermanently(paths)
      if (failures.length) {
        toast.error(`Could not delete ${failures.length} item${failures.length === 1 ? '' : 's'}`, {
          description: failures.map(([p, err]) => `${basename(p)}: ${err}`).join('\n'),
        })
      }
    } catch (e) {
      reportError('Delete failed', e)
    }
    const t = get().activeTab()
    get().updateTab(t.id, { selected: [], anchor: null })
    await get().refresh(t.id)
    if (t.activeQuery) void get().runSearch(t.activeQuery)
  },

  async renameBatch(renames) {
    const t = get().activeTab()
    if (refuseInArchive(t, 'Renaming')) return false
    let out: RenameOutcome
    try {
      out = await api.renameBatch(renames)
    } catch (e) {
      reportError('Rename failed', e)
      return false
    }
    const done = out.done
    if (done.length) {
      const label = done.length === 1 ? `rename to “${basename(done[0][1])}”` : `rename ${done.length} items`
      get().pushUndo({ label, change: { kind: 'batchRename', pairs: done } })
    }
    await get().refresh(t.id)
    if (done.length) get().setSelection(done.map(([, to]) => to), done[0][1], done[0][1])
    if (out.failure) {
      const [at, why] = out.failure
      const stuck = out.stranded.map(([now, was]) => `${basename(was)} is now at ${basename(now)}`)
      toast.error(stuck.length ? 'Rename stopped and could not be fully rolled back' : 'Rename stopped. Nothing was renamed', {
        description: [`${basename(at)}: ${why}`, ...stuck].join('\n'),
      })
      return false
    }
    return true
  },

  async submitPrompt(value) {
    const prompt = get().prompt
    set({ prompt: null })
    if (!prompt) return
    const t = get().activeTab()
    try {
      switch (prompt.kind) {
        case 'rename': {
          if (refuseInArchive(t, 'Renaming')) return
          if (!value.trim() || value === basename(prompt.path)) return
          const dest = await api.renamePath(prompt.path, value)
          get().pushUndo({ label: `rename to “${basename(dest)}”`, change: { kind: 'rename', from: prompt.path, to: dest } })
          await get().refresh(t.id)
          get().setSelection([dest], dest, dest)
          break
        }
        case 'newFolder': {
          if (refuseInArchive(t, 'Creating a folder')) return
          if (!value.trim()) return
          const dest = await api.createFolder(t.dir, value)
          get().pushUndo({ label: `create “${basename(dest)}”`, change: { kind: 'createFolder', path: dest } })
          await get().refresh(t.id)
          get().setSelection([dest], dest, dest)
          break
        }
        case 'moveTo': {
          const dest = await api.resolveLocation(value)
          await get().startTransfer(t.selected, dest, true)
          break
        }
        case 'extractTo': {
          const dest = await api.resolveLocation(value)
          await get().extractArchive(prompt.archive, dest)
          break
        }
        case 'saveSearch': {
          const query = t.activeQuery ?? t.query
          get().saveSearch(value.trim() || query, query)
          break
        }
        case 'saveSmart': {
          get().saveSmartFolder(value.trim() || prompt.query, prompt.query)
          break
        }
        case 'renameSmart': {
          get().renameSmartFolder(prompt.name, value.trim())
          break
        }
        case 'editSmartQuery': {
          get().editSmartQuery(prompt.name, value.trim())
          break
        }
      }
    } catch (e) {
      reportError(
        {
          rename: 'Rename failed',
          newFolder: 'New folder failed',
          moveTo: 'Move failed',
          extractTo: 'Extract failed',
          saveSearch: 'Save failed',
          saveSmart: 'Save failed',
          renameSmart: 'Rename failed',
          editSmartQuery: 'Edit failed',
        }[prompt.kind],
        e,
      )
    }
  },

  async extractArchive(archive, destDir) {
    try {
      const id = await api.extractArchive(archive, destDir)
      set((s) => ({
        opsMeta: {
          ...s.opsMeta,
          [id]: { title: `Extract ${basename(archive)} to ${basename(destDir) || destDir}`, started: Date.now() },
        },
      }))
      get().claimOp(id)
    } catch (e) {
      reportError('Extract failed', e)
    }
  },

  openQuickFilter() {
    const t = get().activeTab()
    get().updateTab(t.id, { filter: t.filter ?? '' })
    get().requestFocus('filter')
  },

  setVimMode(on) {
    set({ vimMode: on })
    void api.saveKeyPrefs({ vim_mode: on }).catch((e) => reportError('Could not save key settings', e))
  },

  addFavorite(path) {
    const favorites = get().favorites
    if (favorites.includes(path)) return
    const next = [...favorites, path]
    set({ favorites: next })
    void api.saveFavorites(next)
  },

  removeFavorite(path) {
    const next = get().favorites.filter((p) => p !== path)
    set({ favorites: next })
    void api.saveFavorites(next)
  },

  saveSearch(name, query) {
    if (!query) return
    const list = get().savedSearches
    const next = list.some((s) => s.name === name)
      ? list.map((s) => (s.name === name ? { ...s, query } : s))
      : [...list, { name, query, pinned: false }]
    set({ savedSearches: next })
    void api.saveSearches(next)
    toast(`Saved search “${name}”`)
  },

  saveSmartFolder(name, query) {
    if (!query) return
    const list = get().savedSearches
    const entry = { name, query, pinned: true }
    const next = list.some((s) => s.name === name)
      ? list.map((s) => (s.name === name ? entry : s))
      : [...list, entry]
    set({ savedSearches: next })
    void api.saveSearches(next)
    toast(`Smart folder “${name}” pinned to the sidebar`)
  },

  pinSmartFolder(name, query) {
    if (get().savedSearches.some((s) => s.name === name)) get().setSmartPinned(name, true)
    else get().saveSmartFolder(name, query)
  },

  setSmartPinned(name, pinned) {
    const next = get().savedSearches.map((s) => (s.name === name ? { ...s, pinned } : s))
    set({ savedSearches: next })
    void api.saveSearches(next)
  },

  renameSmartFolder(name, to) {
    if (!to || to === name) return
    if (get().savedSearches.some((s) => s.name === to)) {
      toast.error('A saved search already has that name')
      return
    }
    const next = get().savedSearches.map((s) => (s.name === name ? { ...s, name: to } : s))
    set({
      savedSearches: next,
      tabs: get().tabs.map((t) => (t.smart?.name === name ? { ...t, smart: { ...t.smart, name: to } } : t)),
    })
    void api.saveSearches(next)
  },

  editSmartQuery(name, query) {
    if (!query) return
    const next = get().savedSearches.map((s) => (s.name === name ? { ...s, query } : s))
    set({
      savedSearches: next,
      tabs: get().tabs.map((t) => (t.smart?.name === name ? { ...t, smart: { ...t.smart, query } } : t)),
    })
    void api.saveSearches(next)
    for (const t of get().tabs) if (t.smart?.name === name) void runSmart(t.id, true)
  },

  async openSmartFolder(search) {
    const t = get().activeTab()
    if (t.archive) {
      toast.error("Smart folders aren't available inside an archive")
      return
    }
    await get().navigate(encodeSmart({ name: search.name, query: search.query, dir: t.dir }))
  },

  onIndexChanged() {
    if (!get().tabs.some((t) => t.smart)) return
    if (smartTimer !== null) clearTimeout(smartTimer)
    smartTimer = setTimeout(() => {
      smartTimer = null
      for (const t of get().tabs) if (t.smart) void runSmart(t.id, false)
      set((s) => ({ smartTick: s.smartTick + 1 }))
    }, SMART_REFRESH_MS)
  },

  removeSavedSearch(name) {
    const next = get().savedSearches.filter((s) => s.name !== name)
    set({ savedSearches: next })
    void api.saveSearches(next)
  },

  calculateDirSizes(paths, fresh = false, background = false) {
    if (!paths.length) return
    set((s) => {
      let dirSizes = { ...s.dirSizes }
      for (const p of paths) dirSizes[p] = 'pending'
      if (Object.keys(dirSizes).length > DIR_SIZE_CAP) dirSizes = pruneSizes(dirSizes, s.tabs)
      return { dirSizes }
    })
    api
      .folderSizes(paths, fresh, background)
      .then((known) => {
        if (!known.length) return
        set((s) => {
          const dirSizes = { ...s.dirSizes }
          for (const [p, size] of known) dirSizes[p] = size
          return { dirSizes }
        })
      })
      .catch(() => {
        set((s) => {
          const dirSizes = { ...s.dirSizes }
          for (const p of paths) if (dirSizes[p] === 'pending') dirSizes[p] = 'failed'
          return { dirSizes }
        })
      })
  },

  openWindow(dir) {
    api.openWindow(dir ?? get().activeTab().dir).catch((e) => reportError('Could not open a window', e))
  },

  moveTab(id, to) {
    const visible = get().visibleTabs()
    const from = visible.findIndex((t) => t.id === id)
    if (from < 0) return
    const next = [...visible]
    const [tab] = next.splice(from, 1)
    next.splice(to > from ? to - 1 : to, 0, tab)
    set({ tabs: [...next, ...get().tabs.filter((t) => t.split)] })
  },

  moveTabToWindow(id) {
    const tab = get().tabs.find((t) => t.id === (id ?? get().activeTabId))
    if (!tab || tab.split) return
    get().openWindow(tab.dir)
    if (get().visibleTabs().length > 1) get().closeTab(tab.id)
  },

  async saveWorkspace(name) {
    try {
      const clean = name.trim()
      await api.saveWorkspace(captureWorkspace(clean))
      const { workspaces } = await api.loadWorkspaces()
      set({ workspaces, currentWorkspace: clean, workspaceDialog: null })
      toast(`Saved workspace “${clean}”`)
    } catch (e) {
      reportError('Could not save the workspace', e)
    }
  },

  async updateWorkspace() {
    const name = get().currentWorkspace
    if (!name) {
      toast('Open or save a workspace first')
      return
    }
    await get().saveWorkspace(name)
  },

  requestOpenWorkspace(name) {
    if (get().visibleTabs().length > 1) set({ workspaceDialog: { mode: 'confirmOpen', name } })
    else void get().openWorkspace(name)
  },

  async openWorkspace(name) {
    const ws = get().workspaces.find((w) => w.name === name)
    if (!ws) {
      toast.error(`No workspace named “${name}”`)
      return
    }
    try {
      const layout = await layoutFrom(ws)
      if (!layout) {
        toast.error(`“${name}” has no folders to open`)
        return
      }
      for (const t of get().tabs) void api.cancelSearch(t.id)
      set({
        tabs: layout.tabs,
        activeTabId: layout.activeId,
        splitTabId: layout.splitId,
        focusedPane: layout.focusedPane,
        currentWorkspace: name,
        workspaceDialog: null,
      })
      get().syncWatches()
      await loadLayout(layout.tabs, layout.restores)
      reportMoved(name, layout.moved)
    } catch (e) {
      reportError('Could not open the workspace', e)
    }
  },

  async deleteWorkspace(name) {
    try {
      await api.deleteWorkspace(name)
      const { workspaces } = await api.loadWorkspaces()
      set({
        workspaces,
        workspaceDialog: null,
        currentWorkspace: get().currentWorkspace === name ? null : get().currentWorkspace,
      })
      toast(`Deleted workspace “${name}”`)
    } catch (e) {
      reportError('Could not delete the workspace', e)
    }
  },

  setRestoreOnLaunch(on) {
    set({ restoreOnLaunch: on })
    void api.setRestoreOnLaunch(on).catch((e) => reportError('Could not save the setting', e))
  },

  onDirSize(path, size) {
    set((s) => ({ dirSizes: { ...s.dirSizes, [path]: size ?? 'failed' } }))
  },

  onDirChanged(dir) {
    const norm = (p: string) => p.replace(/[\\/]+$/, '').toLowerCase()
    set((s) => {
      if (!(dir in s.dirSizes)) return {}
      const dirSizes = { ...s.dirSizes }
      delete dirSizes[dir]
      return { dirSizes }
    })
    for (const t of get().tabs) {
      // A smart folder re-runs on index changes instead, see onIndexChanged.
      if (!t.smart && norm(t.dir) === norm(dir)) {
        void get().refresh(t.id)
        void get().refreshGit(t.id)
      }
    }
  },

  onOpProgress(p) {
    set((s) => ({ ops: { ...s.ops, [p.id]: p } }))
    if (!p.done) return
    // Every window hears every operation; only the one that started it reports.
    const mine = p.id in get().opsMeta
    for (const t of get().tabs) void get().refresh(t.id)
    if (!mine) {
      // This window may be hearing a finish that its own start call has not
      // returned for yet. Keep it briefly so `claimOp` can still find it.
      if (p.outcomes.length) {
        set((s) => ({ doneEarly: { ...s.doneEarly, [p.id]: p } }))
        setTimeout(() => {
          set((s) => {
            const doneEarly = { ...s.doneEarly }
            delete doneEarly[p.id]
            return { doneEarly }
          })
        }, 10_000)
      }
      return
    }
    get().recordOp(p)
    const title = get().opsMeta[p.id]?.title
    if (p.error === 'cancelled') {
      toast(title ? `Cancelled: ${title}` : 'Cancelled')
    } else if (p.error) {
      toast.error(p.message, { description: p.error })
    } else {
      toast.success(p.message, title ? { description: title } : undefined)
    }
  },

  openDuplicateFinder() {
    set({ duplicateFinderOpen: true })
  },

  closeDuplicateFinder() {
    void api.cancelDuplicateScan()
    set({
      duplicateFinderOpen: false,
      dupStatus: 'idle',
      dupScanId: null,
      dupProgress: null,
      dupScan: null,
      dupError: null,
      dupSelected: new Set(),
    })
  },

  async startDuplicateScan(scope) {
    const root = get().activeTab().dir
    set({
      dupScope: scope,
      dupRoot: root,
      dupStatus: 'scanning',
      dupScanId: null,
      dupProgress: null,
      dupScan: null,
      dupError: null,
      dupSelected: new Set(),
    })
    try {
      const id = await api.startDuplicateScan(scope, root)
      // The result may already have arrived; only record the id if this scan is still the one running.
      if (get().dupStatus === 'scanning') set({ dupScanId: id })
    } catch (e) {
      set({ dupStatus: 'idle', dupError: errorText(e) })
    }
  },

  cancelDuplicateScan() {
    void api.cancelDuplicateScan()
    set({ dupStatus: 'idle', dupProgress: null })
  },

  onDuplicatesProgress(p) {
    const { dupStatus, dupScanId } = get()
    if (dupStatus !== 'scanning' || (dupScanId !== null && p.id !== dupScanId)) return
    set({ dupProgress: p })
  },

  onDuplicatesDone(d) {
    const { dupStatus, dupScanId } = get()
    if (dupStatus !== 'scanning' || (dupScanId !== null && d.id !== dupScanId)) return
    if (d.error) {
      set({ dupStatus: 'idle', dupError: d.error, dupProgress: null })
    } else if (d.scan.cancelled) {
      set({ dupStatus: 'idle', dupProgress: null })
    } else {
      set({ dupStatus: 'done', dupScan: d.scan, dupScanId: d.id, dupProgress: null, dupSelected: new Set() })
    }
  },

  setDupSelection(next) {
    const scan = get().dupScan
    if (!scan) return false
    if (!allowedSelection(scan, next)) {
      const sets = fullySelectedSets(scan, next)
      const name = sets[0]?.members[0]?.path.split(/[\\/]/).pop() ?? 'a set'
      toast.error(`Keep one copy in every set`, {
        description: `That would remove every copy of ${name}${sets.length > 1 ? ` and ${sets.length - 1} more` : ''}.`,
      })
      return false
    }
    set({ dupSelected: next })
    return true
  },

  async trashDuplicates() {
    const { dupScan, dupSelected } = get()
    if (!dupScan || dupSelected.size === 0) return
    const t = get().activeTab()
    try {
      const out = await api.trashDuplicates(dupScan.id, [...dupSelected])
      if (out.skipped.length) {
        toast.warning(`Left ${out.skipped.length} alone`, {
          description: out.skipped.map(([p, why]) => `${basename(p)}: ${why}`).join('\n'),
        })
      }
      if (out.removed.failures.length) {
        toast.error(`Could not move ${out.removed.failures.length} item${out.removed.failures.length === 1 ? '' : 's'} to the Trash`, {
          description: out.removed.failures.map(([p, err]) => `${basename(p)}: ${err}`).join('\n'),
        })
      }
      if (out.removed.done.length) {
        const what = describeItems(out.removed.done)
        get().pushUndo({ label: `move ${what} to the Trash`, change: { kind: 'trash', paths: out.removed.done } })
        toast(`Moved ${what} to the Trash`, { action: { label: 'Undo', onClick: () => void get().undo() } })
      }
      const remaining = new Set(out.scan.groups.flatMap((g) => g.members.map((m) => m.path)))
      set({
        dupScan: out.scan,
        dupSelected: new Set([...dupSelected].filter((p) => remaining.has(p))),
      })
    } catch (e) {
      reportError('Move to Trash failed', e)
    }
    await get().refresh(t.id)
  },

  openShell(target) {
    shellOpenSeq += 1
    set({ shellTarget: { ...target, opened: shellOpenSeq } })
    api
      .loadShellHistory()
      .then((shellHistory) => set({ shellHistory }))
      .catch(() => undefined)
  },

  closeShell() {
    set({ shellTarget: null })
  },

  async runShell(template, stopOnError, plugin) {
    const target = get().shellTarget
    if (!target) return
    const prev = get().shellRun
    if (prev?.status === 'running') throw new Error('A command is already running. Cancel it first.')
    // Unique across windows: one ShellRunner serves them all, and its events reach every window.
    const id = newRequestId()
    set({
      shellRun: { id, tabId: target.tabId, stopOnError, status: 'running', invocations: [], lines: [], firstSeq: 0 },
      shellPanelOpen: true,
    })
    try {
      // A plugin action is re-checked and planned by the backend, and never recorded in history.
      const commands = plugin
        ? await api.runPluginAction(id, plugin, target.paths, target.dir)
        : await api.startShell(id, template, target.paths, target.dir, stopOnError)
      set((st) => {
        if (st.shellRun?.id !== id) return {}
        let invocations = st.shellRun.invocations
        commands.forEach((command, i) => {
          invocations = setInvocation(invocations, i, { command })
        })
        return { shellRun: { ...st.shellRun, invocations } }
      })
    } catch (e) {
      // Nothing started, so the panel goes back to whatever it showed before.
      set({ shellRun: prev, shellPanelOpen: prev !== null })
      throw e
    }
    set({ shellTarget: null })
    if (plugin) return
    api
      .pushShellHistory(template)
      .then((shellHistory) => set({ shellHistory }))
      .catch(() => undefined)
  },

  cancelShell() {
    void api.cancelShell()
  },

  closeShellPanel() {
    set({ shellPanelOpen: false })
  },

  onShellLines(e) {
    const run = get().shellRun
    if (!run || run.id !== e.run) return
    // Lines below first_seq were dropped on the Rust side; drop them here too.
    const base = e.first_seq > run.firstSeq ? run.lines.filter((l) => l.seq >= e.first_seq) : run.lines
    let invocations = run.invocations
    if (invocations[e.inv]?.status !== 'running' && e.lines.length > 0) {
      invocations = setInvocation(invocations, e.inv, { status: 'running' })
    }
    set({
      shellRun: {
        ...run,
        invocations,
        firstSeq: Math.max(run.firstSeq, e.first_seq),
        lines: [...base, ...e.lines.filter((l) => l.seq >= e.first_seq).map((l) => ({ ...l, inv: e.inv }))],
      },
    })
  },

  onShellExit(e) {
    const run = get().shellRun
    if (!run || run.id !== e.run) return
    const status = e.cancelled ? 'cancelled' : e.code === 0 ? 'ok' : 'failed'
    set({ shellRun: { ...run, invocations: setInvocation(run.invocations, e.inv, { status, code: e.code }) } })
  },

  onShellDone(e) {
    const run = get().shellRun
    if (!run || run.id !== e.run) return
    const status = e.cancelled ? 'cancelled' : e.stopped ? 'stopped' : 'done'
    // Commands that never started are marked, so the list doesn't show them as still waiting.
    const invocations = run.invocations.map((i): ShellInvocation =>
      i.status === 'pending' ? { ...i, status: 'skipped' } : i,
    )
    set({ shellRun: { ...run, status, invocations } })
    // A command may have changed files, so show the folder as it is now.
    void get().refresh(run.tabId)
  },

  openCompare() {
    // A sync still running keeps its dialog state; only its result is shown on reopen.
    if (get().syncStatus === 'running') {
      set({ compareOpen: true })
      return
    }
    const { tabs, activeTabId, splitTabId } = get()
    const primary = tabs.find((t) => t.id === activeTabId) ?? tabs[0]
    const other = splitTabId !== null ? tabs.find((t) => t.id === splitTabId) : undefined
    set({
      compareOpen: true,
      compareLeft: primary.dir,
      compareRight: other?.dir ?? '',
      compareStatus: 'idle',
      compareId: null,
      compareProgress: null,
      comparison: null,
      compareError: null,
      comparePicks: {},
      comparePlan: null,
      syncStatus: 'idle',
      syncId: null,
      syncProgress: null,
      syncOutcome: null,
      syncError: null,
    })
  },

  closeCompare() {
    void api.cancelCompare()
    // Closing stops a running sync between files. Its result still reaches undo.
    if (get().syncStatus === 'running') void api.cancelSync()
    set({
      compareOpen: false,
      compareStatus: 'idle',
      compareId: null,
      compareProgress: null,
      comparison: null,
      compareError: null,
      comparePicks: {},
      comparePlan: null,
      syncStatus: 'idle',
      syncId: null,
      syncProgress: null,
      syncOutcome: null,
      syncError: null,
    })
  },

  async startCompare() {
    const { compareLeft, compareRight, compareBy } = get()
    set({
      compareStatus: 'running',
      compareId: null,
      compareProgress: null,
      comparison: null,
      compareError: null,
      comparePicks: {},
      comparePlan: null,
    })
    try {
      const id = await api.startCompare(compareLeft, compareRight, compareBy)
      // The result may already have arrived; only record the id if this comparison is still the one running.
      if (get().compareStatus === 'running') set({ compareId: id })
    } catch (e) {
      set({ compareStatus: 'idle', compareError: errorText(e) })
    }
  },

  cancelCompare() {
    void api.cancelCompare()
    set({ compareStatus: 'idle', compareProgress: null })
  },

  onCompareProgress(p) {
    const { compareStatus, compareId } = get()
    if (compareStatus !== 'running' || (compareId !== null && p.id !== compareId)) return
    set({ compareProgress: p })
  },

  onCompareDone(d) {
    const { compareStatus, compareId } = get()
    if (compareStatus !== 'running' || (compareId !== null && d.id !== compareId)) return
    if (d.error) {
      set({ compareStatus: 'idle', compareError: d.error, compareProgress: null })
    } else if (!d.comparison) {
      // Cancelled.
      set({ compareStatus: 'idle', compareProgress: null })
    } else {
      set({
        compareStatus: 'done',
        comparison: d.comparison,
        compareId: d.id,
        compareProgress: null,
        comparePicks: {},
        comparePlan: null,
      })
    }
  },

  setComparePick(rel, pick) {
    const picks = { ...get().comparePicks }
    if (pick) picks[rel] = pick
    else delete picks[rel]
    set({ comparePicks: picks, comparePlan: null })
  },

  async applyCompareBulk(bulk) {
    const c = get().comparison
    if (!c) return
    try {
      const list = await api.compareBulk(c.id, bulk)
      set({ comparePicks: Object.fromEntries(list), comparePlan: null })
    } catch (e) {
      toast.error('Could not apply that', { description: errorText(e) })
    }
  },

  async reviewCompareSync() {
    const c = get().comparison
    const picks = Object.entries(get().comparePicks)
    if (!c || picks.length === 0) return
    try {
      set({ comparePlan: await api.planSync(c.id, picks) })
    } catch (e) {
      toast.error('Could not plan the sync', { description: errorText(e) })
    }
  },

  async runCompareSync() {
    const { comparePlan, comparison } = get()
    if (!comparePlan || !comparison) return
    const label = `${basename(comparison.left)} and ${basename(comparison.right)}`
    set({
      syncStatus: 'running',
      syncLabel: label,
      syncId: null,
      syncProgress: null,
      syncOutcome: null,
      syncError: null,
      comparePlan: null,
      comparePicks: {},
    })
    try {
      const id = await api.runSync(comparePlan.id)
      if (get().syncStatus === 'running') set({ syncId: id })
    } catch (e) {
      set({ syncStatus: 'idle', syncError: errorText(e) })
    }
  },

  cancelCompareSync() {
    void api.cancelSync()
  },

  onSyncProgress(p) {
    const { syncStatus, syncId } = get()
    if (syncStatus !== 'running' || (syncId !== null && p.id !== syncId)) return
    set({ syncProgress: p })
  },

  onSyncDone(d) {
    // Checked first: a late or foreign done event must not record undo or toast twice.
    // A null syncId means runSync has not returned yet; that run is the only one that can be running here.
    const { syncStatus, syncId } = get()
    if (syncStatus !== 'running' || (syncId !== null && d.id !== syncId)) return
    const o = d.outcome
    if (o) {
      // Recorded whether or not the dialog is still open: the files did change.
      if (o.created.length || o.trashed.length) {
        get().pushUndo({
          label: `sync ${get().syncLabel}`,
          change: { kind: 'sync', created: o.created, written: o.written, trashed: o.trashed, setAside: o.set_aside },
        })
      }
      const parts = `${o.copied} copied, ${o.overwritten} overwritten, ${o.removed} removed`
      if (o.failures.length) {
        toast.error(`Sync finished with ${o.failures.length} failed`, {
          description: o.failures.map(([p, why]) => `${basename(p)}: ${why}`).join('\n'),
        })
      } else if (o.created.length || o.trashed.length) {
        toast(`Sync ${o.cancelled ? 'stopped' : 'finished'}: ${parts}`, {
          action: { label: 'Undo', onClick: () => void get().undo() },
        })
      }
      for (const t of get().tabs) void get().refresh(t.id)
    } else if (d.error) {
      toast.error('Sync failed', { description: d.error })
    }
    set({ syncStatus: 'done', syncId: d.id, syncProgress: null, syncOutcome: o, syncError: d.error })
  },

  setIndex(index) {
    const before = get().index
    set({ index })
    // Sizes read while the index was building are wrong: drop them, and ask
    // again for what the open tabs show.
    if (before.indexing && !index.indexing) {
      set({ dirSizes: {} })
      for (const t of get().tabs) void get().refresh(t.id)
    }
  },

  async refreshGit(tabId) {
    const id = tabId ?? get().focusedTabId()
    const tab = get().tabs.find((t) => t.id === id)
    if (!tab) return
    const dir = tab.dir
    const git = await api.gitStatus(dir).catch(() => null)
    get().updateTab(id, (t) => (t.dir === dir ? { git } : {}))
  },

  async refreshIgnored(tabId, items) {
    const tab = get().tabs.find((t) => t.id === tabId)
    if (!tab || tab.archive) return
    const dir = tab.dir
    const ignored = await api.gitIgnored(dir, items.map((n) => n.path)).catch(() => [])
    // Keep the answer only if the listing it was computed for is still shown.
    get().updateTab(tabId, (t) => (t.dir === dir && t.items === items ? { ignored } : {}))
  },

  async gitStage(paths) {
    await runGitAction(get, paths, api.gitStage, 'Could not stage')
  },

  async gitUnstage(paths) {
    await runGitAction(get, paths, api.gitUnstage, 'Could not unstage')
  },

  async gitDiscard(paths) {
    const t = get().activeTab()
    if (refuseInArchive(t, 'Discarding changes')) return
    try {
      const out = await api.gitDiscard(paths)
      if (out.failures.length) {
        toast.error(`Could not discard ${out.failures.length} file${out.failures.length === 1 ? '' : 's'}`, {
          description: out.failures.map(([p, err]) => `${basename(p)}: ${err}`).join('\n'),
        })
      }
      if (out.done.length) {
        toast(`Discarded changes to ${out.done.length === 1 ? basename(out.done[0]) : `${out.done.length} files`}. A copy is in the Trash.`)
      }
    } catch (e) {
      reportError('Could not discard changes', e)
    }
    await get().refreshGit(t.id)
    await get().refresh(t.id)
  },

  async gitCommit(message) {
    const t = get().activeTab()
    const root = t.git?.root
    if (!root) return false
    try {
      const summary = await api.gitCommit(root, message)
      toast(summary || 'Committed')
      await get().refreshGit(t.id)
      await get().refresh(t.id)
      return true
    } catch (e) {
      reportError('Commit failed', e)
      return false
    }
  },

  setHideIgnored(on) {
    set({ hideIgnored: on })
    saveHideIgnored(on)
  },

  syncWatches() {
    void api.watchDirs(get().tabs.flatMap((t) => [t.dir, ...(t.view === 'columns' ? t.colPath : [])]))
  },

  setView(view, tabId) {
    get().updateTab(tabId ?? get().focusedTabId(), { view })
    get().syncWatches()
  },

  selectColumnRow(tabId, col, path, isDir) {
    const t = get().tabs.find((x) => x.id === tabId)
    if (!t) return
    const colPath = isDir ? [...t.colPath.slice(0, col), path] : t.colPath.slice(0, col)
    get().updateTab(tabId, { colPath, selected: [path], anchor: path, cursor: path })
    get().syncWatches()
  },
}))

/** Runs a git path action on the active tab's repository, then refreshes its marks and listing. */
async function runGitAction(
  get: () => State & Actions,
  paths: string[],
  call: (paths: string[]) => Promise<void>,
  failure: string,
) {
  if (paths.length === 0) return
  const t = get().activeTab()
  if (refuseInArchive(t, 'Changing git state')) return
  try {
    await call(paths)
  } catch (e) {
    reportError(failure, e)
  }
  await get().refreshGit(t.id)
  await get().refresh(t.id)
}

/**
 * Completes a streamed listing: the archive check, the selection, git's
 * ignored marks and folder sizes. Runs once the last batch has arrived.
 */
async function finishListing(tabId: number, active: ActiveListing) {
  const { dir } = active
  const archive = await active.archive
  // Superseded while the archive check ran: the newer listing takes over.
  if (activeListings.get(tabId) !== active) return
  activeListings.delete(tabId)
  const store = useExplorer.getState()
  store.updateTab(tabId, (t) => {
    if (t.dir !== dir) return {}
    const alive = new Set(t.items.map((n) => n.path))
    return {
      loading: false,
      listing: null,
      listingError: null,
      archive,
      selected: t.activeQuery ? t.selected : t.selected.filter((p) => alive.has(p)),
    }
  })
  active.finish(true)
  if (archive) return
  const tab = useExplorer.getState().tabs.find((t) => t.id === tabId)
  if (!tab || tab.dir !== dir) return
  void store.refreshIgnored(tabId, tab.items)
  // Fill in folder sizes without being asked. This folder's own total is only
  // shown when the index can answer it: walking all of it (say, the home
  // folder) on every visit would be far too slow. Subfolder sizes are needed
  // only to sort by size, and then they queue behind visible rows.
  const req = sizeRequests(dir, tab.items, tab.sort.col === 'size', store.dirSizes, store.index.exists)
  if (req.visible.length) store.calculateDirSizes(req.visible)
  if (req.background.length) store.calculateDirSizes(req.background, false, true)
}

/** Keep sizes for folders some open tab is showing; drop the rest. */
function pruneSizes(sizes: Record<string, DirSize>, tabs: Tab[]): Record<string, DirSize> {
  const live = new Set<string>()
  for (const t of tabs) {
    live.add(t.dir)
    for (const n of t.items) if (n.is_dir) live.add(n.path)
    for (const n of t.results) if (n.is_dir) live.add(n.path)
  }
  const out: Record<string, DirSize> = {}
  for (const [p, v] of Object.entries(sizes)) if (live.has(p)) out[p] = v
  return out
}

/** The layout a tab really shows. Smart folders and searches have no columns, so they show the list. */
export function effectiveView(tab: Tab): ViewMode {
  return tab.view === 'columns' && !tab.smart && tab.activeQuery === null ? 'columns' : 'list'
}

/** Rows a tab should show: search results in rank order, or the sorted listing. */
export function displayItems(
  tab: Tab,
  showHidden: boolean,
  dirSizes: Record<string, DirSize>,
  hideIgnored = false,
): VNode[] {
  const ignored = new Set(tab.ignored)
  const isIgnored = (n: VNode) => ignored.has(n.path) || tab.git?.marks[n.path] === 'I'
  const hideGit = (n: VNode) => !(hideIgnored && isIgnored(n))
  if (tab.activeQuery !== null) return tab.results.filter(hideGit)
  const visible = (showHidden ? tab.items : tab.items.filter((n) => !n.hidden)).filter(hideGit)
  // A folder still streaming in keeps the order it arrived in; it is sorted once complete.
  if (tab.listing !== null) return visible
  const sizeOf = (n: VNode) => {
    const d = n.is_dir ? dirSizes[n.path] : undefined
    return typeof d === 'number' ? d : n.is_dir ? -1 : n.size
  }
  const { col, dir } = tab.sort
  const sign = dir === 'asc' ? 1 : -1
  const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' })
  const kindKey = (n: VNode) => (n.is_dir ? '' : n.name.split('.').pop()?.toLowerCase() ?? '')
  return [...visible].sort((a, b) => {
    // Folders stay above files whichever way the column is sorted.
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1
    let c = 0
    if (col === 'name') c = collator.compare(a.name, b.name)
    else if (col === 'size') c = sizeOf(a) - sizeOf(b)
    else if (col === 'modified') c = a.modified_at - b.modified_at
    else c = collator.compare(kindKey(a), kindKey(b)) || collator.compare(a.name, b.name)
    return c * sign
  })
}
