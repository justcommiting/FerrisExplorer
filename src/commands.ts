// Command catalog shared by the command palette, context menu and shortcuts.
import { toast } from 'sonner'

import { api, errorText, type PluginAction, type PluginPlan, type PluginReport, type PluginView, type PluginWhen, type VNode } from '@/lib/api'
import { isArchiveName } from '@/lib/archive'
import { basename, parentOf, relativeTo } from '@/lib/format'
import { type PluginRequest, useExplorer } from '@/store/explorer'

export const isMac = navigator.userAgent.includes('Mac')
const mod = isMac ? '⌘' : 'Ctrl+'

export interface Command {
  id: string
  label: string
  keywords?: string
  shortcut?: string
  group: 'Navigate' | 'File' | 'Clipboard' | 'View' | 'Search' | 'Index' | 'Git' | 'Plugins'
  /** Hidden from the palette when false. */
  enabled?: () => boolean
  /** For switches: the palette shows whether this is on. */
  toggled?: () => boolean
  run: () => void | Promise<void>
}

const s = () => useExplorer.getState()
const tab = () => s().activeTab()
const hasSelection = () => tab().selected.length > 0
const single = () => tab().selected.length === 1
/** Archives are read-only, so anything that changes a file is off inside one. */
const writable = () => !tab().archive
const singleArchive = () => single() && !tab().archive && isArchiveName(basename(tab().selected[0]))
/** Creating or pasting needs a real folder, so it is off in a smart folder too. */
const inFolder = () => writable() && !tab().smart
/** Git actions need a repository around the active folder, and a writable one. */
const inRepo = () => !!tab().git && writable()

/** Presets the user can pin as smart folders. Each uses only filters search.rs supports. */
const SMART_PRESETS = [
  { name: 'Large files', query: 'size:>1gb' },
  { name: 'Recent files', query: 'modified:<7d' },
  { name: 'Modified today', query: 'modified:today' },
]

/** Does a plugin's `when` rule fit the selection? Mirrors `When::matches` in plugins.rs. */
function whenMatches(w: PluginWhen, items: { name: string; is_dir: boolean }[]): boolean {
  if (items.length < w.min || (w.max !== null && items.length > w.max)) return false
  if (w.kind === 'files' && items.some((i) => i.is_dir)) return false
  if (w.kind === 'dirs' && items.some((i) => !i.is_dir)) return false
  if (w.extensions.length === 0) return true
  return items.every((i) => {
    const dot = i.name.lastIndexOf('.')
    const ext = dot > 0 ? i.name.slice(dot + 1).toLowerCase() : ''
    return !i.is_dir && w.extensions.includes(ext)
  })
}

/** The selection as `whenMatches` sees it. Paths not in the listing count as files. */
function selectionEntries(): { name: string; is_dir: boolean }[] {
  const t = tab()
  const pool: VNode[] = t.activeQuery !== null ? t.results : t.items
  return t.selected.map((p) => {
    const node = pool.find((n) => n.path === p)
    return { name: basename(p), is_dir: node?.is_dir ?? false }
  })
}

/** Runs a confirmed plugin action through the Run Command panel, which streams its output. */
export async function startPluginRun(req: PluginRequest) {
  const st = s()
  st.openShell({ tabId: req.tabId, dir: req.dir, paths: req.paths })
  try {
    await st.runShell(req.plan.template, false, { file: req.file, hash: req.hash, actionId: req.actionId })
  } catch (e) {
    toast.error(errorText(e))
  }
}

/** Plans an action on the selection. Confirmed actions show their command lines first. */
export async function runPluginAction(p: PluginView, a: PluginAction) {
  const t = tab()
  if (t.archive) {
    toast.error("Commands can't run inside an archive, which is read-only")
    return
  }
  if (t.selected.length === 0) {
    toast('Select one or more files first')
    return
  }
  const paths = [...t.selected]
  let plan: PluginPlan
  try {
    plan = await api.planPluginAction(p.file, a.id, paths, t.dir)
  } catch (e) {
    toast.error(errorText(e))
    return
  }
  const req = { title: a.label, plan, tabId: t.id, dir: t.dir, paths, file: p.file, hash: p.hash, actionId: a.id }
  if (plan.confirm) {
    s().set({ pluginRequest: req })
    return
  }
  await startPluginRun(req)
}

/** Re-reads the plugin folder. Invalid files are reported with a toast, never thrown. */
export async function reloadPlugins(quiet = false) {
  try {
    const report: PluginReport = await api.loadPlugins()
    s().set({ pluginReport: report })
    if (report.errors.length > 0) {
      toast.error(`${report.errors.length} plugin file(s) could not load. Open Plugins to see why.`)
    } else if (!quiet) {
      toast(`Loaded ${report.plugins.length} plugin file(s)`)
    }
  } catch (e) {
    toast.error(errorText(e))
  }
}

/**
 * Commands for the enabled plugins' actions. Each is enabled only when there is a
 * selection that its `when` fits, and the folder is not an archive.
 */
export function pluginCommands(report: PluginReport | null): Command[] {
  if (!report) return []
  return report.plugins
    .filter((p) => p.enabled)
    .flatMap((p) =>
      p.actions.map((a): Command => ({
        id: `plugin:${p.file}:${a.id}`,
        label: `${p.name}: ${a.label}`,
        keywords: `plugin ${p.name}`,
        group: 'Plugins',
        enabled: () => hasSelection() && writable() && whenMatches(a.when, selectionEntries()),
        run: () => runPluginAction(p, a),
      })),
    )
}

async function copyText(text: string, what: string) {
  if (!text) return
  try {
    await navigator.clipboard.writeText(text)
    toast(`Copied ${what}`)
  } catch {
    toast.error('Clipboard unavailable')
  }
}

/** Opens Run Command on Selection for the active tab's selection. */
function openShellInput() {
  const t = tab()
  if (t.archive) {
    toast.error("Commands can't run inside an archive, which is read-only")
    return
  }
  if (t.selected.length === 0) {
    toast('Select one or more files first')
    return
  }
  s().openShell({ tabId: t.id, dir: t.dir, paths: [...t.selected] })
}

export const commands: Command[] = [
  // Navigate
  { id: 'location', label: 'Go to Path…', keywords: 'location address bar', shortcut: `${mod}L`, group: 'Navigate', run: () => s().requestFocus('location') },
  { id: 'jump', label: 'Jump to Folder…', keywords: 'frecency recent frequent visited zoxide go folder', shortcut: `${mod}J`, group: 'Navigate', run: () => s().set({ jumpOpen: true }) },
  { id: 'quick-open', label: 'Go to File…', keywords: 'find anywhere index quick open', shortcut: `${mod}P`, group: 'Navigate', run: () => s().set({ quickOpenOpen: true }) },
  { id: 'back', label: 'Back', keywords: 'history previous', shortcut: `${mod}[`, group: 'Navigate', run: () => s().goBack() },
  { id: 'forward', label: 'Forward', keywords: 'history next', shortcut: `${mod}]`, group: 'Navigate', run: () => s().goForward() },
  { id: 'parent', label: 'Go to Parent Folder', keywords: 'up', shortcut: 'Alt+↑', group: 'Navigate', run: () => s().goUp() },
  { id: 'home', label: 'Go Home', keywords: 'home user', group: 'Navigate', run: () => void s().navigate(s().home) },
  { id: 'new-window', label: 'New Window', keywords: 'window open', shortcut: `${mod}N`, group: 'Navigate', run: () => s().openWindow() },
  { id: 'tab-to-window', label: 'Move Tab to New Window', keywords: 'detach tear off pop out', group: 'Navigate', run: () => s().moveTabToWindow() },
  { id: 'new-tab', label: 'New Tab', shortcut: `${mod}T`, group: 'Navigate', run: () => s().newTab() },
  { id: 'close-tab', label: 'Close Tab', shortcut: `${mod}W`, group: 'Navigate', enabled: () => s().visibleTabs().length > 1, run: () => s().closeTab(s().activeTabId) },
  { id: 'split', label: 'Toggle Split Pane', keywords: 'dual pane columns side by side', shortcut: 'F3', group: 'View', run: () => s().toggleSplit() },
  { id: 'switch-pane', label: 'Switch Pane', keywords: 'other pane focus', shortcut: 'Tab', group: 'View', enabled: () => s().splitTabId !== null, run: () => s().focusPane(s().focusedPane === 'split' ? 'primary' : 'split') },
  { id: 'copy-to-pane', label: 'Copy to Other Pane', keywords: 'copy across', shortcut: 'F5', group: 'File', enabled: () => s().splitTabId !== null && hasSelection(), run: () => void s().transferToOtherPane(false) },
  { id: 'move-to-pane', label: 'Move to Other Pane', keywords: 'move across', shortcut: 'F6', group: 'File', enabled: () => s().splitTabId !== null && hasSelection(), run: () => void s().transferToOtherPane(true) },
  { id: 'refresh', label: 'Refresh', keywords: 'reload', shortcut: `${mod}R`, group: 'Navigate', run: () => void s().refresh() },
  { id: 'favorite', label: 'Add Folder to Favorites', keywords: 'bookmark pin star', group: 'Navigate', run: () => s().addFavorite(tab().dir) },

  // File
  { id: 'open', label: 'Open', shortcut: 'Enter', group: 'File', enabled: hasSelection, run: () => void s().openSelection() },
  { id: 'rename', label: 'Rename…', shortcut: 'F2', group: 'File', enabled: () => single() && writable(), run: () => {
      const p = tab().selected[0]
      s().set({ prompt: { kind: 'rename', path: p, initial: basename(p) } })
    } },
  { id: 'bulk-rename', label: 'Bulk Rename…', keywords: 'rename many batch template regex find replace case', group: 'File', enabled: () => hasSelection() && writable(), run: () => s().set({ bulkRename: [...tab().selected] }) },
  { id: 'new-folder', label: 'New Folder…', keywords: 'mkdir create directory', shortcut: `${mod}⇧N`, group: 'File', enabled: inFolder, run: () => s().set({ prompt: { kind: 'newFolder', initial: 'New folder' } }) },
  { id: 'move-to', label: 'Move to…', keywords: 'move relocate', group: 'File', enabled: () => hasSelection() && writable(), run: () => s().set({ prompt: { kind: 'moveTo', initial: tab().dir } }) },
  { id: 'duplicate', label: 'Duplicate', keywords: 'copy clone', shortcut: `${mod}⇧D`, group: 'File', enabled: () => hasSelection() && writable(), run: () => void s().duplicateSelection() },
  { id: 'compress', label: 'Compress to ZIP', keywords: 'zip archive', group: 'File', enabled: () => single() && writable(), run: () => void s().compressSelection() },
  { id: 'undo', label: 'Undo', keywords: 'revert reverse back take back', shortcut: `${mod}Z`, group: 'File', enabled: () => s().undoStack.length > 0, run: () => void s().undo() },
  { id: 'redo', label: 'Redo', keywords: 'reapply again', shortcut: isMac ? '⇧⌘Z' : 'Ctrl+Y', group: 'File', enabled: () => s().redoStack.length > 0, run: () => void s().redo() },
  { id: 'delete', label: 'Move to Trash', keywords: 'remove delete recycle bin', shortcut: 'Del', group: 'File', enabled: () => hasSelection() && writable(), run: () => void s().trashPaths(tab().selected) },
  { id: 'delete-permanently', label: 'Delete Permanently…', keywords: 'remove forever erase no trash', shortcut: isMac ? '⇧⌘⌫' : 'Shift+Del', group: 'File', enabled: () => hasSelection() && writable(), run: () => s().set({ confirmDelete: tab().selected }) },
  { id: 'reveal', label: isMac ? 'Show in Finder' : 'Show in Explorer', keywords: 'reveal file manager', group: 'File', enabled: writable, run: () => void api.revealPath(tab().selected[0] ?? tab().dir) },
  { id: 'terminal', label: 'Open Terminal Here', keywords: 'shell console', shortcut: `${mod}⇧T`, group: 'File', enabled: writable, run: () => void api.openTerminal(tab().dir) },
  { id: 'editor', label: 'Open in Editor', keywords: 'edit code', group: 'File', enabled: () => single() && writable(), run: () => void api.openInEditor(tab().selected[0]) },
  { id: 'shell-run', label: 'Run Command on Selection…', keywords: 'shell command script exec execute each file batch xargs pipe', shortcut: `${mod}⇧R`, group: 'File', run: () => openShellInput() },
  { id: 'shell-output', label: 'Show Command Output', keywords: 'shell command panel log last run', group: 'File', enabled: () => s().shellRun !== null, run: () => s().set({ shellPanelOpen: true }) },

  // Clipboard
  { id: 'extract-here', label: 'Extract Here', keywords: 'unzip decompress archive', group: 'File', enabled: singleArchive, run: () => {
      const p = tab().selected[0]
      void s().extractArchive(p, parentOf(p) ?? tab().dir)
    } },
  { id: 'extract-to', label: 'Extract To…', keywords: 'unzip decompress archive folder', group: 'File', enabled: singleArchive, run: () => {
      const p = tab().selected[0]
      s().set({ prompt: { kind: 'extractTo', archive: p, initial: parentOf(p) ?? tab().dir } })
    } },
  { id: 'copy', label: 'Copy', shortcut: `${mod}C`, group: 'Clipboard', enabled: hasSelection, run: () => s().copySelection(false) },
  { id: 'cut', label: 'Cut', shortcut: `${mod}X`, group: 'Clipboard', enabled: () => hasSelection() && writable(), run: () => s().copySelection(true) },
  { id: 'paste', label: 'Paste', shortcut: `${mod}V`, group: 'Clipboard', enabled: () => !!s().clipboard && inFolder(), run: () => void s().paste() },
  { id: 'copy-path', label: 'Copy Path', shortcut: `${mod}⇧C`, group: 'Clipboard', run: () => void copyText((hasSelection() ? tab().selected : [tab().dir]).join('\n'), 'path') },
  { id: 'copy-rel', label: 'Copy Relative Path', group: 'Clipboard', enabled: hasSelection, run: () => void copyText(tab().selected.map((p) => relativeTo(p, tab().dir)).join('\n'), 'relative path') },
  { id: 'copy-name', label: 'Copy Filename', group: 'Clipboard', enabled: hasSelection, run: () => void copyText(tab().selected.map(basename).join('\n'), 'filename') },

  // View
  { id: 'select-all', label: 'Select All', shortcut: `${mod}A`, group: 'View', run: () => {
      const st = s()
      const t = tab()
      const pool = t.activeQuery !== null ? t.results : t.items.filter((n) => st.showHidden || !n.hidden)
      st.setSelection(pool.map((n) => n.path), null)
    } },
  { id: 'view-list', label: 'Show as List', keywords: 'list view rows table layout', group: 'View', enabled: () => tab().view === 'columns', run: () => s().setView('list') },
  { id: 'view-columns', label: 'Show as Columns', keywords: 'columns miller finder browse layout tree', group: 'View', enabled: () => tab().view !== 'columns' && !tab().smart && tab().activeQuery === null, run: () => s().setView('columns') },
  { id: 'view-toggle', label: 'Toggle List / Columns', keywords: 'switch view layout list columns', shortcut: `${mod}⇧V`, group: 'View', enabled: () => tab().view === 'columns' || (!tab().smart && tab().activeQuery === null), run: () => s().setView(tab().view === 'columns' ? 'list' : 'columns') },
  { id: 'toggle-hidden', label: 'Toggle Hidden Files', keywords: 'dotfiles show hide', shortcut: `${mod}⇧.`, group: 'View', run: () => s().set({ showHidden: !s().showHidden }) },
  { id: 'hide-ignored', label: 'Hide Git-Ignored Files', keywords: 'gitignore ignored dimmed ignore view', group: 'View', toggled: () => s().hideIgnored, run: () => s().setHideIgnored(!s().hideIgnored) },
  { id: 'toggle-details', label: 'Toggle Details Panel', keywords: 'preview info sidebar', shortcut: `${mod}I`, group: 'View', run: () => s().set({ showDetails: !s().showDetails }) },
  { id: 'dir-size', label: 'Recalculate Folder Sizes', keywords: 'size disk usage refresh fresh', group: 'View', enabled: () => !tab().archive, run: () => {
      const t = tab()
      const pool = t.activeQuery !== null ? t.results : t.items
      const sel = new Set(t.selected)
      const dirs = pool.filter((n) => n.is_dir && (sel.size === 0 || sel.has(n.path))).map((n) => n.path)
      s().calculateDirSizes(dirs, true)
    } },
  { id: 'sort-name', label: 'Sort by Name', group: 'View', run: () => s().setSort('name') },
  { id: 'sort-size', label: 'Sort by Size', group: 'View', run: () => s().setSort('size') },
  { id: 'sort-modified', label: 'Sort by Date Modified', group: 'View', run: () => s().setSort('modified') },
  { id: 'sort-kind', label: 'Sort by Kind', group: 'View', run: () => s().setSort('kind') },

  // Search
  { id: 'search', label: 'Search', keywords: 'find filter query', shortcut: `${mod}F`, group: 'Search', enabled: () => !tab().archive, run: () => s().requestFocus('search') },
  { id: 'quick-filter', label: 'Quick Filter', keywords: 'filter narrow this folder by name', shortcut: `${mod}⇧F`, group: 'Search', run: () => s().openQuickFilter() },
  { id: 'save-search', label: 'Save Current Search…', group: 'Search', enabled: () => !!tab().activeQuery, run: () => s().set({ prompt: { kind: 'saveSearch', initial: tab().activeQuery ?? '' } }) },
  { id: 'save-smart', label: 'Save Search as Smart Folder…', keywords: 'pin sidebar live folder', group: 'Search', enabled: () => !!tab().activeQuery, run: () => {
      const q = tab().activeQuery ?? ''
      s().set({ prompt: { kind: 'saveSmart', initial: q, query: q } })
    } },
  ...SMART_PRESETS.map((p): Command => ({
    id: `smart-${p.name.toLowerCase().replace(/\s+/g, '-')}`,
    label: `Pin Smart Folder: ${p.name}`,
    keywords: `preset smart folder ${p.query}`,
    group: 'Search',
    enabled: () => !s().savedSearches.some((x) => x.name === p.name && x.pinned),
    run: () => s().pinSmartFolder(p.name, p.query),
  })),
  { id: 'vim', label: 'Vim-Style Keys', keywords: 'vim hjkl modal keyboard bindings motions', group: 'View', toggled: () => s().vimMode, run: () => s().setVimMode(!s().vimMode) },
  { id: 'shortcuts', label: 'Keyboard Shortcuts…', keywords: 'help keys bindings cheat sheet', shortcut: '?', group: 'View', run: () => s().set({ shortcutsOpen: true }) },
  { id: 'ops', label: 'Show Operations', keywords: 'progress copies moves queue jobs', group: 'View', run: () => s().set({ opsPanelOpen: true }) },
  { id: 'space', label: 'Find What Uses Space', keywords: 'space hogger disk usage large folders', group: 'Search', run: () => s().set({ spaceFinderOpen: true, spaceFinderView: 'map' }) },
  { id: 'space-changes', label: 'What Grew Recently…', keywords: 'space growth grew changes history week month disk usage snapshot shrank', group: 'Search', run: () => s().set({ spaceFinderOpen: true, spaceFinderView: 'changes' }) },
  { id: 'duplicates', label: 'Find Duplicates…', keywords: 'duplicate copies same identical clean up dedupe', group: 'Search', enabled: () => !tab().archive, run: () => s().openDuplicateFinder() },
  { id: 'compare', label: 'Compare Folders…', keywords: 'compare diff sync mirror update two folders side by side backup', group: 'File', enabled: () => !tab().archive, run: () => s().openCompare() },
  { id: 'workspace-save', label: 'Save Workspace…', keywords: 'session layout tabs snapshot store remember', group: 'Navigate', run: () => s().set({ workspaceDialog: { mode: 'save' } }) },
  { id: 'workspace-open', label: 'Open Workspace…', keywords: 'session layout tabs restore load switch', group: 'Navigate', enabled: () => s().workspaces.length > 0, run: () => s().set({ workspaceDialog: { mode: 'open' } }) },
  { id: 'workspace-update', label: 'Update Workspace', keywords: 'session layout save overwrite current', group: 'Navigate', enabled: () => s().currentWorkspace !== null, run: () => void s().updateWorkspace() },
  { id: 'workspace-delete', label: 'Delete Workspace…', keywords: 'session layout remove forget', group: 'Navigate', enabled: () => s().workspaces.length > 0, run: () => s().set({ workspaceDialog: { mode: 'delete' } }) },
  { id: 'restore-on-launch', label: 'Restore Tabs on Launch', keywords: 'session startup remember reopen last workspace', group: 'View', toggled: () => s().restoreOnLaunch, run: () => s().setRestoreOnLaunch(!s().restoreOnLaunch) },

  // Index
  { id: 'rebuild-index', label: 'Rebuild Search Index', keywords: 'reindex scan', group: 'Index', run: () => void api.rebuildIndex() },

  // Git
  { id: 'git-diff', label: 'Git: Show Diff', keywords: 'difftool changes', group: 'Git', enabled: () => single() && !!tab().git, run: () => void api.gitDiff(tab().selected[0]) },
  { id: 'git-refresh', label: 'Git: Refresh Status', group: 'Git', enabled: () => !!tab().git, run: () => void s().refreshGit() },
  { id: 'git-stage', label: 'Stage', keywords: 'git add index staging', group: 'Git', enabled: () => hasSelection() && inRepo(), run: () => void s().gitStage(tab().selected) },
  { id: 'git-unstage', label: 'Unstage', keywords: 'git reset index unstaging', group: 'Git', enabled: () => hasSelection() && inRepo(), run: () => void s().gitUnstage(tab().selected) },
  { id: 'git-discard', label: 'Discard Changes…', keywords: 'git revert restore undo working tree reset', group: 'Git', enabled: () => hasSelection() && inRepo(), run: () => s().set({ confirmDiscard: [...tab().selected] }) },
  { id: 'git-commit', label: 'Commit…', keywords: 'git commit staged message record', group: 'Git', enabled: inRepo, run: () => s().set({ commitOpen: true }) },

  // Plugins
  { id: 'plugins-open', label: 'Plugins…', keywords: 'plugin manifest extension enable trust window', group: 'Plugins', run: () => s().set({ pluginsOpen: true }) },
  { id: 'plugins-reload', label: 'Reload Plugins', keywords: 'plugin manifest refresh reload scan', group: 'Plugins', run: () => void reloadPlugins() },
]

export const commandById = Object.fromEntries(commands.map((c) => [c.id, c]))

export function runCommand(id: string) {
  const cmd = commandById[id]
  if (cmd && (!cmd.enabled || cmd.enabled())) void cmd.run()
}
