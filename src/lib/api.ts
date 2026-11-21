// Typed wrappers around the Tauri commands in src-tauri/src/api.rs.
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

export interface VNode {
  name: string
  path: string
  size: number
  is_dir: boolean
  is_symlink: boolean
  modified_at: number
  created_at: number
  readonly: boolean
  hidden: boolean
  mime_type: string
}

/** A path inside an archive: the archive file and the path inside it ("" for its root). */
export interface ArchiveLocation {
  archive: string
  inner: string
}

export interface Places {
  home: string
  drives: string[]
  separator: string
}

export interface DriveUsage {
  path: string
  total: number
  free: number
}

export interface SavedSearch {
  name: string
  query: string
  /** Pinned searches are listed in the sidebar as smart folders. */
  pinned: boolean
}

export interface SmartCount {
  count: number
  /** True when the index hit its row limit, so `count` is a lower bound. */
  capped: boolean
}

/** A folder in the jump list, best first. */
export interface JumpHit {
  path: string
  score: number
}

export interface KeyPrefs {
  vim_mode: boolean
}

/** One tab in a saved workspace. Empty strings from older files mean the defaults. */
export interface WorkspaceTab {
  /** A folder or archive path, or a smart folder reference (`smart:` then JSON). */
  path: string
  view: string
  sort_col: string
  sort_dir: string
  col_path: string[]
  /** Names of the selected entries in this folder. */
  selected: string[]
  split: boolean
}

export interface Workspace {
  name: string
  tabs: WorkspaceTab[]
  active: number
  focused_split: boolean
}

export interface WorkspaceList {
  restore_on_launch: boolean
  workspaces: Workspace[]
}

/** One line of a command's output. `seq` orders lines across the whole run. */
export interface ShellLine {
  seq: number
  err: boolean
  text: string
}

export interface ShellLinesEvent {
  run: number
  inv: number
  /** Lines below this sequence number were dropped on the Rust side and should be dropped here too. */
  first_seq: number
  lines: ShellLine[]
}

export interface ShellExitEvent {
  run: number
  inv: number
  code: number | null
  cancelled: boolean
}

export interface ShellDoneEvent {
  run: number
  cancelled: boolean
  stopped: boolean
}

export interface Settings {
  favorites: string[]
  saved_searches: SavedSearch[]
  history: string[]
}

export interface IndexState {
  message: string
  indexing: boolean
  syncing: boolean
  exists: boolean
}

export interface GitStatus {
  root: string
  branch: string
  /** Commits the branch has that its upstream lacks, and the reverse. */
  ahead: number
  behind: number
  marks: Record<string, string>
  /** Paths with changes in the index. */
  staged: string[]
}

export interface GitDiscard {
  done: string[]
  failures: [string, string][]
}

export interface BlameLine {
  line: number
  sha: string
  author: string
  /** Author time, Unix seconds. */
  time: number
  text: string
}

export interface GitBlame {
  lines: BlameLine[]
  /** The first 2000 lines only. */
  truncated: boolean
}

export type Preview =
  | { kind: 'none' }
  | { kind: 'text'; lines: string[]; truncated: boolean }
  | { kind: 'code'; language: string; lines: string[]; truncated: boolean }
  | { kind: 'markdown'; text: string; truncated: boolean }
  | { kind: 'image'; data_url: string }
  | { kind: 'video'; data_url: string }
  | { kind: 'audio'; data_url: string }
  /** The first 4 KiB of a binary file. `size` is the whole file's size. */
  | { kind: 'hex'; bytes: number[]; size: number }

/** A user plugin's `when` rule, as the manifest states it. */
export interface PluginWhen {
  /** Lowercase, without the dot. Empty means any name. */
  extensions: string[]
  kind: 'any' | 'files' | 'dirs'
  min: number
  max: number | null
}

export interface PluginAction {
  id: string
  label: string
  command: string
  when: PluginWhen
  confirm: boolean
  shortcut: string | null
}

export interface PluginPreviewer {
  extensions: string[]
  command: string
  timeout_ms: number
  max_output: number
}

/** One plugin file that loaded. `enabled` holds only while its hash matches the enabled one. */
export interface PluginView {
  file: string
  hash: string
  enabled: boolean
  name: string
  version: string
  description: string
  actions: PluginAction[]
  previewers: PluginPreviewer[]
}

export interface PluginError {
  file: string
  error: string
}

export interface PluginReport {
  dir: string
  plugins: PluginView[]
  errors: PluginError[]
}

/** Which plugin action a run came from. The backend re-checks it against `hash`. */
export interface PluginRunRef {
  file: string
  hash: string
  actionId: string
}

/** The command lines an action would run, from shell.rs planning. */
export interface PluginPlan {
  template: string
  lines: string[]
  confirm: boolean
}

/** Plain text from a plugin previewer. Shown as text, never as HTML. */
export interface PluginPreviewResult {
  plugin: string
  text: string
  truncated: boolean
  error: string | null
  cancelled: boolean
}

/** One line that matched a `content:` search, split around the match. */
export interface ContentSnippet {
  line: number
  before: string
  matched: string
  after: string
}

export interface ContentMatches {
  /** Lines that match, across the whole file. */
  matching_lines: number
  /** The first 20 matching lines, in file order. */
  snippets: ContentSnippet[]
  /** Start of the extracted text. Only set for documents. */
  excerpt: string | null
}

export interface SearchStarted {
  interpreted_as: string | null
  errors: string[]
}

export interface OpProgress {
  id: number
  kind: 'copy' | 'move' | 'delete' | 'duplicate' | 'compress' | 'extract'
  message: string
  bytes_done: number
  bytes_total: number
  files_done: number
  files_total: number
  error: string | null
  done: boolean
  bytes_per_sec: number
  skipped: number
  conflict: Conflict | null
  /** [source, result] for each item that finished; set on the final message. */
  outcomes: [string, string][]
}

export interface Conflict {
  src: string
  dest: string
  src_is_dir: boolean
  src_size: number
  src_modified: number
  dest_size: number
  dest_modified: number
  remaining: number
}

export type Resolution = 'keep_both' | 'skip' | 'cancel'

export interface SpaceNode {
  name: string
  path: string
  size: number
  direct: number
  children: SpaceNode[]
  other_count: number
  other_size: number
}

export interface DeleteSummary {
  files: number
  dirs: number
  bytes: number
  truncated: boolean
}

export interface TrashOutcome {
  done: string[]
  failures: [string, string][]
}

export interface DupMember {
  path: string
  size: number
  modified_at: number
}

/** Files with identical contents. `wasted` is what keeping one copy would free. */
export interface DupGroup {
  size: number
  wasted: number
  members: DupMember[]
}

export interface DupScan {
  id: number
  groups: DupGroup[]
  reclaimable: number
  files_checked: number
  cancelled: boolean
}

export interface DupProgress {
  id: number
  phase: 'partial' | 'full'
  done: number
  total: number
}

export interface DupDone {
  id: number
  scan: DupScan
  error: string | null
}

export interface DupTrashOutcome {
  removed: TrashOutcome
  skipped: [string, string][]
  scan: DupScan
}

export type CompareBy = 'metadata' | 'contents'
export type CompareStatus =
  | 'only_left'
  | 'only_right'
  | 'identical'
  | 'newer_left'
  | 'newer_right'
  | 'same_time_size_differs'
  | 'content_differs'
  | 'type_conflict'
/** What a row action does: copy across, or remove on one side. */
export type ComparePick = 'copy_right' | 'copy_left' | 'delete_right' | 'delete_left'
export type CompareBulk = 'mirror_left_right' | 'mirror_right_left' | 'update_right' | 'update_left'

export interface CompareStat {
  kind: 'file' | 'dir'
  size: number
  modified_at: number
}

export interface CompareEntry {
  rel: string
  status: CompareStatus
  left: CompareStat | null
  right: CompareStat | null
}

export interface CompareSkip {
  rel: string
  side: 'left' | 'right'
  reason: string
}

export interface Comparison {
  id: number
  left: string
  right: string
  by: CompareBy
  entries: CompareEntry[]
  skipped: CompareSkip[]
}

export interface CompareProgress {
  id: number
  phase: 'scan' | 'hash'
  done: number
  total: number
}

export interface CompareDone {
  id: number
  comparison: Comparison | null
  error: string | null
}

export interface SyncSummary {
  id: number
  copies: number
  overwrites: number
  deletions: number
  bytes_written: number
  bytes_trashed: number
  skipped: [string, string][]
}

export interface SyncProgress {
  id: number
  done: number
  total: number
}

export interface SyncOutcome {
  id: number
  copied: number
  overwritten: number
  removed: number
  /** Items this run wrote, deepest first. */
  created: string[]
  /** The same items as they were left, for undo to check before trashing them. */
  written: CreatedItem[]
  /** Removed items now in the Trash. */
  trashed: string[]
  /** Old versions overwritten, each in the Trash under its aside name. */
  set_aside: SetAside[]
  failures: [string, string][]
  skipped: [string, string][]
  not_run: number
  cancelled: boolean
}

/** An item a sync wrote, as it was left. Undo trashes it only while it still matches. */
export interface CreatedItem {
  path: string
  dir: boolean
  size: number
  /** Full precision, as a string: a JSON number cannot hold it exactly. */
  mtime_ns: string
}

/** An old version a sync moved aside to overwrite `path`. Undo puts it back from `aside`. */
export interface SetAside {
  path: string
  aside: string
}

/** What undoing a sync's written items came to. */
export interface WrittenUndo {
  /** Written items trashed. */
  done: string[]
  /** Old versions put back at their real paths. */
  restored: string[]
  failures: [string, string][]
  /** Items left in place, with why. They changed since the sync wrote them. */
  skipped: [string, string][]
}

export interface SyncDone {
  id: number
  outcome: SyncOutcome | null
  error: string | null
}

/** A batch that stopped at its first failure: what went through, and the item that stopped it. */
export interface MoveOutcome {
  done: [string, string][]
  failure: [string, string] | null
}

/** A rename batch: what was renamed, what stopped it, and what a rollback could not put back. */
export interface RenameOutcome {
  done: [string, string][]
  failure: [string, string] | null
  /** Where each stranded item is now, and where it started. */
  stranded: [string, string][]
}

export interface RestoreOutcome {
  done: string[]
  failure: [string, string] | null
}

/** Where a drop from another app lands and which of its items get copied. */
export interface DropPlan {
  dest: string
  sources: string[]
}

export interface SearchEvent {
  tab: number
  query: string
  results: VNode[]
  error: string | null
  complete: boolean
}

export interface DirSizeEvent {
  path: string
  size: number | null
}

/** One batch of a streamed folder listing. */
export interface ListingChunk {
  tab: number
  request: number
  entries: VNode[]
  done: boolean
  error: string | null
}

type ChunkHandler = (chunk: ListingChunk) => void

/**
 * Request ids are shared by every window (the backend emits to all of them and
 * runs one registry), so each window starts from a random 31-bit base instead
 * of 1. The base is shifted left 20 bits so the counter stays a safe integer.
 */
const requestBase = Math.floor(Math.random() * 0x7fffffff) * 0x100000
let requestCounter = 0

/** A request id that is unique across windows, for listings, shell runs and previews. */
export function newRequestId(): number {
  requestCounter = (requestCounter + 1) % 0x100000
  return requestBase + requestCounter
}

/** Handlers for in-flight listings, keyed by request id; each remembers its tab. */
const chunkHandlers = new Map<number, { tab: number; handler: ChunkHandler }>()
let chunkListener: Promise<UnlistenFn> | null = null

/**
 * One window-wide listener for `listing-chunk`, attached before the first
 * listing starts so no batch can be missed. Each batch goes to its request's
 * handler when both the request and the tab match; the last batch of a
 * request removes the handler.
 */
function listenForChunks(): Promise<UnlistenFn> {
  chunkListener ??= listen<ListingChunk>('listing-chunk', (e) => {
    const entry = chunkHandlers.get(e.payload.request)
    if (!entry || entry.tab !== e.payload.tab) return
    if (e.payload.done) chunkHandlers.delete(e.payload.request)
    entry.handler(e.payload)
  })
  return chunkListener
}

/** A newer release found on the update endpoint. Snake_case, like the other Rust structs. */
export interface UpdateInfo {
  version: string
  current_version: string
  notes: string | null
  date: string | null
}

export const api = {
  places: () => invoke<Places>('places'),
  driveUsage: () => invoke<DriveUsage[]>('drive_usage'),
  listDir: (path: string) => invoke<VNode[]>('list_dir', { path }),
  /** Starts a streamed listing. Batches arrive through `onListingChunk` until one has `done` set. */
  listDirStream: async (tab: number, request: number, path: string, onChunk: ChunkHandler) => {
    await listenForChunks()
    chunkHandlers.set(request, { tab, handler: onChunk })
    try {
      await invoke<void>('list_dir_stream', { tab, request, path })
    } catch (e) {
      chunkHandlers.delete(request)
      throw e
    }
  },
  /** Stops a streamed listing; a request that is no longer current does nothing. */
  cancelListing: (tab: number, request: number) => {
    if (chunkHandlers.get(request)?.tab === tab) chunkHandlers.delete(request)
    return invoke<void>('cancel_listing', { tab, request })
  },
  archiveContext: (path: string) => invoke<ArchiveLocation | null>('archive_context', { path }),
  extractArchive: (archive: string, destDir: string) => invoke<number>('extract_archive', { archive, destDir }),
  resolveLocation: (raw: string) => invoke<string>('resolve_location', { raw }),
  isNavigable: (path: string) => invoke<boolean>('is_navigable', { path }),
  watchDirs: (dirs: string[]) => invoke<void>('watch_dirs', { dirs }),
  requestDirSizes: (paths: string[]) => invoke<void>('request_dir_sizes', { paths }),

  startSearch: (tab: number, query: string, currentDir: string) =>
    invoke<SearchStarted>('start_search', { tab, query, currentDir }),
  cancelSearch: (tab: number) => invoke<void>('cancel_search', { tab }),
  quickSearch: (query: string) => invoke<VNode[]>('quick_search', { query }),

  indexStatus: () => invoke<IndexState>('index_status'),
  rebuildIndex: () => invoke<void>('rebuild_index'),
  spaceHoggers: () => invoke<[string, number][]>('space_hoggers'),

  gitStatus: (dir: string) => invoke<GitStatus | null>('git_status', { dir }),
  gitDiff: (path: string) => invoke<void>('git_diff', { path }),
  gitStage: (paths: string[]) => invoke<void>('git_stage', { paths }),
  gitUnstage: (paths: string[]) => invoke<void>('git_unstage', { paths }),
  gitDiscard: (paths: string[]) => invoke<GitDiscard>('git_discard', { paths }),
  /** Resolves to the new commit's summary line. */
  gitCommit: (dir: string, message: string) => invoke<string>('git_commit', { dir, message }),
  /** The paths among `paths` that git ignores. */
  gitIgnored: (dir: string, paths: string[]) => invoke<string[]>('git_ignored', { dir, paths }),
  gitBlame: (path: string) => invoke<GitBlame>('git_blame', { path }),

  preview: (path: string) => invoke<Preview>('preview', { path }),
  contentMatches: (path: string, needle: string) => invoke<ContentMatches>('content_matches', { path, needle }),

  loadPlugins: () => invoke<PluginReport>('load_plugins'),
  setPluginEnabled: (file: string, hash: string, enabled: boolean) =>
    invoke<PluginReport>('set_plugin_enabled', { file, hash, enabled }),
  planPluginAction: (file: string, action: string, paths: string[], dir: string) =>
    invoke<PluginPlan>('plan_plugin_action', { file, action, paths, dir }),
  /** Runs a confirmed plugin action under `runId`. `plugin.hash` is the one the user saw. Not added to shell history. */
  runPluginAction: (runId: number, plugin: PluginRunRef, paths: string[], dir: string) =>
    invoke<string[]>('run_plugin_action', { runId, plugin, paths, dir }),
  /** Null when no enabled previewer matches the file. */
  pluginPreview: (path: string, request: number) =>
    invoke<PluginPreviewResult | null>('plugin_preview', { path, request }),
  cancelPluginPreview: (request: number) => invoke<void>('cancel_plugin_preview', { request }),

  checkForUpdate: () => invoke<UpdateInfo | null>('check_for_update'),
  /** Downloads, verifies and installs the update, then restarts the app. */
  installUpdate: (expectedVersion: string) => invoke<void>('install_update', { expectedVersion }),

  openPath: (path: string) => invoke<void>('open_path', { path }),
  revealPath: (path: string) => invoke<void>('reveal_path', { path }),
  openTerminal: (dir: string) => invoke<void>('open_terminal', { dir }),
  openInEditor: (path: string) => invoke<void>('open_in_editor', { path }),

  /** Records a webview error in the app log. Callers should swallow failures. */
  logFrontendError: (message: string, stack?: string) =>
    invoke<void>('log_frontend_error', { message, stack: stack ?? null }),
  /** Opens the app log folder in the OS file manager. */
  openLogDir: () => invoke<void>('open_log_dir'),

  renamePath: (path: string, newName: string) => invoke<string>('rename_path', { path, newName }),
  renameBatch: (renames: [string, string][]) => invoke<RenameOutcome>('rename_batch', { renames }),
  createFolder: (dir: string, name: string) => invoke<string>('create_folder', { dir, name }),
  dirIsEmpty: (path: string) => invoke<boolean>('dir_is_empty', { path }),
  trashPaths: (paths: string[]) => invoke<TrashOutcome>('trash_paths', { paths }),
  restoreFromTrash: (paths: string[]) => invoke<RestoreOutcome>('restore_from_trash', { paths }),
  trashUnchangedCreated: (items: CreatedItem[], setAside: SetAside[]) =>
    invoke<WrittenUndo>('trash_unchanged_created', { items, setAside }),
  deletePermanently: (paths: string[]) => invoke<[string, string][]>('delete_permanently', { paths }),
  moveExact: (pairs: [string, string][]) => invoke<MoveOutcome>('move_exact', { pairs }),
  transferPaths: (sources: string[], destDir: string, cut: boolean) =>
    invoke<number>('transfer_paths', { sources, destDir, cut }),
  resolveDrop: (sources: string[], hovered: string | null, paneDir: string) =>
    invoke<DropPlan>('resolve_drop', { sources, hovered, paneDir }),
  startDrag: (paths: string[]) => invoke<void>('start_drag', { paths }),
  cancelOp: (id: number) => invoke<void>('cancel_op', { id }),
  resolveConflict: (id: number, resolution: Resolution, applyToAll: boolean) =>
    invoke<void>('resolve_conflict', { id, resolution, applyToAll }),
  spaceTree: (root: string | null, refresh: boolean) =>
    invoke<{ tree: SpaceNode; from_index: boolean }>('space_tree', { root, refresh }),
  deleteSummary: (paths: string[]) => invoke<DeleteSummary>('delete_summary', { paths }),
  cancelSpaceScan: () => invoke<void>('cancel_space_scan'),
  startDuplicateScan: (scope: 'folder' | 'index', root: string) =>
    invoke<number>('start_duplicate_scan', { scope, root }),
  cancelDuplicateScan: () => invoke<void>('cancel_duplicate_scan'),
  trashDuplicates: (id: number, paths: string[]) => invoke<DupTrashOutcome>('trash_duplicates', { id, paths }),
  startCompare: (left: string, right: string, by: CompareBy) => invoke<number>('start_compare', { left, right, by }),
  cancelCompare: () => invoke<void>('cancel_compare'),
  compareBulk: (id: number, bulk: CompareBulk) => invoke<[string, ComparePick][]>('compare_bulk', { id, bulk }),
  planSync: (id: number, picks: [string, ComparePick][]) => invoke<SyncSummary>('plan_sync', { id, picks }),
  runSync: (planId: number) => invoke<number>('run_sync', { planId }),
  cancelSync: () => invoke<void>('cancel_sync'),
  /** `background` queues the folders behind visible rows (used when sorting by size). */
  folderSizes: (paths: string[], fresh: boolean, background = false) =>
    invoke<[string, number][]>('folder_sizes', { paths, fresh, background }),
  openWindow: (dir: string) => invoke<void>('open_window', { dir }),
  dropTab: (dir: string, x: number, y: number, allowNew: boolean) =>
    invoke<'merged' | 'opened' | 'none'>('drop_tab', { dir, x, y, allowNew }),
  duplicatePaths: (sources: string[]) => invoke<number>('duplicate_paths', { sources }),
  compressPath: (path: string) => invoke<number>('compress_path', { path }),

  loadSettings: () => invoke<Settings>('load_settings'),
  saveFavorites: (favorites: string[]) => invoke<void>('save_favorites', { favorites }),
  saveSearches: (searches: SavedSearch[]) => invoke<void>('save_searches', { searches }),
  countSmartFolder: (query: string) => invoke<SmartCount | null>('count_smart_folder', { query }),
  pushHistory: (query: string) => invoke<string[]>('push_history', { query }),
  recordVisit: (path: string) => invoke<void>('record_visit', { path }),
  jumpFolders: (query: string) => invoke<JumpHit[]>('jump_folders', { query }),
  loadKeyPrefs: () => invoke<KeyPrefs>('load_key_prefs'),
  saveKeyPrefs: (prefs: KeyPrefs) => invoke<void>('save_key_prefs', { prefs }),

  loadWorkspaces: () => invoke<WorkspaceList>('load_workspaces'),
  loadLastSession: () => invoke<Workspace | null>('load_last_session'),
  /** Resolves true when a workspace of the same name was replaced. */
  saveWorkspace: (workspace: Workspace) => invoke<boolean>('save_workspace', { workspace }),
  saveLastSession: (workspace: Workspace) => invoke<void>('save_last_session', { workspace }),
  deleteWorkspace: (name: string) => invoke<boolean>('delete_workspace', { name }),
  setRestoreOnLaunch: (on: boolean) => invoke<void>('set_restore_on_launch', { on }),
  resolveRestoreDirs: (paths: string[]) => invoke<string[]>('resolve_restore_dirs', { paths }),

  planShell: (template: string, paths: string[], dir: string) =>
    invoke<string[]>('plan_shell', { template, paths, dir }),
  /** Starts a run under the id the client picked. Resolves to the command lines that will run. */
  startShell: (runId: number, template: string, paths: string[], dir: string, stopOnError: boolean) =>
    invoke<string[]>('start_shell', { runId, template, paths, dir, stopOnError }),
  cancelShell: () => invoke<void>('cancel_shell'),
  loadShellHistory: () => invoke<string[]>('load_shell_history'),
  pushShellHistory: (command: string) => invoke<string[]>('push_shell_history', { command }),
}

export const events = {
  onSearch: (cb: (e: SearchEvent) => void): Promise<UnlistenFn> =>
    listen<SearchEvent>('search-update', (e) => cb(e.payload)),
  onOpProgress: (cb: (e: OpProgress) => void): Promise<UnlistenFn> =>
    listen<OpProgress>('op-progress', (e) => cb(e.payload)),
  onDirSize: (cb: (e: DirSizeEvent) => void): Promise<UnlistenFn> =>
    listen<DirSizeEvent>('dir-size', (e) => cb(e.payload)),
  onDirChanged: (cb: (dir: string) => void): Promise<UnlistenFn> =>
    listen<string>('dir-changed', (e) => cb(e.payload)),
  /** The index was written to. Throttled on the Rust side. */
  onIndexChanged: (cb: () => void): Promise<UnlistenFn> => listen('index-changed', () => cb()),
  onIndexStatus: (cb: (s: IndexState) => void): Promise<UnlistenFn> =>
    listen<IndexState>('index-status', (e) => cb(e.payload)),
  onDuplicatesProgress: (cb: (e: DupProgress) => void): Promise<UnlistenFn> =>
    listen<DupProgress>('duplicates-progress', (e) => cb(e.payload)),
  onDuplicatesDone: (cb: (e: DupDone) => void): Promise<UnlistenFn> =>
    listen<DupDone>('duplicates-done', (e) => cb(e.payload)),
  onCompareProgress: (cb: (e: CompareProgress) => void): Promise<UnlistenFn> =>
    listen<CompareProgress>('compare-progress', (e) => cb(e.payload)),
  onCompareDone: (cb: (e: CompareDone) => void): Promise<UnlistenFn> =>
    listen<CompareDone>('compare-done', (e) => cb(e.payload)),
  onSyncProgress: (cb: (e: SyncProgress) => void): Promise<UnlistenFn> =>
    listen<SyncProgress>('sync-progress', (e) => cb(e.payload)),
  onSyncDone: (cb: (e: SyncDone) => void): Promise<UnlistenFn> =>
    listen<SyncDone>('sync-done', (e) => cb(e.payload)),
  onShellLines: (cb: (e: ShellLinesEvent) => void): Promise<UnlistenFn> =>
    listen<ShellLinesEvent>('shell-lines', (e) => cb(e.payload)),
  onShellExit: (cb: (e: ShellExitEvent) => void): Promise<UnlistenFn> =>
    listen<ShellExitEvent>('shell-exit', (e) => cb(e.payload)),
  onShellDone: (cb: (e: ShellDoneEvent) => void): Promise<UnlistenFn> =>
    listen<ShellDoneEvent>('shell-done', (e) => cb(e.payload)),
}

/** Tauri rejects with the command's error string. */
export function errorText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e)
}

// ── Size history ("what grew") ───────────────────────────────────────────

export type SpacePeriod = 'day' | 'week' | 'month'

/** One folder's size then and now. `delta` is `new_size - old_size`. */
export interface SpaceChange {
  path: string
  old_size: number
  new_size: number
  delta: number
}

export interface SpaceChanges {
  period: SpacePeriod
  /** Unix seconds of the snapshot compared against, or null when none is old enough yet. */
  baseline_at: number | null
  /** Unix seconds of the oldest snapshot on record, or null when there are none. */
  first_snapshot_at: number | null
  now: number
  diff: {
    grown: SpaceChange[]
    shrunk: SpaceChange[]
    new: SpaceChange[]
    deleted: SpaceChange[]
  }
}

export const spaceHistory = {
  changes: (period: SpacePeriod) => invoke<SpaceChanges>('space_changes', { period }),
}
