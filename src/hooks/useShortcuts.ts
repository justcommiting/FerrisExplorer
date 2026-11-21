import { useEffect } from 'react'

import { type Command, isMac, pluginCommands, runCommand } from '@/commands'
import type { PluginReport } from '@/lib/api'
import { useExplorer } from '@/store/explorer'

function typingInField(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el) return false
  return el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName)
}

/** Keys bound to a command, in the form `combo` takes below. Plugin shortcuts never use these. */
const KEY_MAP: Record<string, string> = {
  'mod+[': 'back',
  'mod+]': 'forward',
  'alt+arrowleft': 'back',
  'alt+arrowright': 'forward',
  'alt+arrowup': 'parent',
  backspace: 'parent',
  'mod+r': 'refresh',
  f5: 'refresh',
  f6: 'move-to-pane',
  f3: 'split',
  'mod+d': 'split',
  enter: 'open',
  f2: 'rename',
  delete: 'delete',
  'mod+backspace': 'delete',
  'mod+delete': 'delete',
  'shift+delete': 'delete-permanently',
  'mod+shift+backspace': 'delete-permanently',
  'mod+a': 'select-all',
  'mod+c': 'copy',
  'mod+x': 'cut',
  'mod+v': 'paste',
  'mod+shift+c': 'copy-path',
  'mod+shift+n': 'new-folder',
  'mod+shift+d': 'duplicate',
  'mod+z': 'undo',
  'mod+shift+z': 'redo',
  'mod+y': 'redo',
  'mod+shift+t': 'terminal',
  'mod+shift+r': 'shell-run',
  'mod+shift+v': 'view-toggle',
  'mod+shift+.': 'toggle-hidden',
  'mod+shift+>': 'toggle-hidden',
  'mod+i': 'toggle-details',
  'mod+j': 'jump',
  'mod+shift+f': 'quick-filter',
  '/': 'search',
  ' ': 'toggle-details',
  '?': 'shortcuts',
  'shift+?': 'shortcuts',
}

/** Answered before the key map, and inside the search box too. */
const GLOBAL_KEYS: Record<string, string> = {
  'mod+k': '',
  'mod+p': 'quick-open',
  'mod+l': 'location',
  'mod+f': 'search',
  'mod+t': 'new-tab',
  'mod+w': 'close-tab',
  'mod+n': 'new-window',
}

const NAMED_KEYS: Record<string, string> = { enter: 'enter', space: ' ', delete: 'delete', backspace: 'backspace' }

/** Built-in keys always win, so a plugin shortcut must not use one of these. */
function isBuiltin(combo: string): boolean {
  return combo in KEY_MAP || combo in GLOBAL_KEYS || /^mod\+[1-9]$/.test(combo)
}

/**
 * A plugin's shortcut text, such as "Ctrl+Alt+H", in the form `combo` takes.
 * Ctrl, Cmd and Meta all mean the platform's command key, as the app's own labels do.
 * Null when the text is not a key the app can bind. A key needs Ctrl or ⌘, so it cannot
 * take typing or list navigation.
 */
export function shortcutCombo(text: string): string | null {
  const parts = text.split('+').map((p) => p.trim().toLowerCase())
  const key = parts.pop() ?? ''
  let shift = false
  let alt = false
  let mod = false
  for (const p of parts) {
    if (p === 'ctrl' || p === 'cmd' || p === 'meta' || p === 'mod') mod = true
    else if (p === 'shift') shift = true
    else if (p === 'alt') alt = true
    else return null
  }
  if (!mod) return null
  const named = NAMED_KEYS[key] ?? (/^([a-z0-9]|f([1-9]|1[0-2]))$/.test(key) ? key : null)
  if (!named) return null
  return `mod+${shift ? 'shift+' : ''}${alt ? 'alt+' : ''}${named}`
}

export type ShortcutCheck =
  | { state: 'bound'; combo: string }
  | { state: 'invalid' }
  | { state: 'builtin'; combo: string }
  | { state: 'taken'; combo: string; by: string }

/**
 * The shortcut of each enabled plugin action that has one. Built-in keys win. Of
 * plugins that want the same key, the first in file-name order keeps it.
 * `checks` is keyed by `file#actionId`, for the Plugins overlay to show.
 */
export function pluginShortcuts(report: PluginReport | null): { bound: Map<string, Command>; checks: Map<string, ShortcutCheck> } {
  const byId = new Map(pluginCommands(report).map((c) => [c.id, c]))
  const bound = new Map<string, Command>()
  const owner = new Map<string, string>()
  const checks = new Map<string, ShortcutCheck>()
  for (const p of report?.plugins ?? []) {
    if (!p.enabled) continue
    for (const a of p.actions) {
      if (!a.shortcut) continue
      const id = `${p.file}#${a.id}`
      const combo = shortcutCombo(a.shortcut)
      const cmd = byId.get(`plugin:${p.file}:${a.id}`)
      if (!combo) checks.set(id, { state: 'invalid' })
      else if (isBuiltin(combo)) checks.set(id, { state: 'builtin', combo })
      else if (owner.has(combo)) checks.set(id, { state: 'taken', combo, by: owner.get(combo) ?? '' })
      else if (cmd) {
        owner.set(combo, `${p.name}: ${a.label}`)
        bound.set(combo, cmd)
        checks.set(id, { state: 'bound', combo })
      }
    }
  }
  return { bound, checks }
}

/** Window-level shortcuts. Row navigation lives in FileList. */
export function useShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const st = useExplorer.getState()
      // Only this window's operations: each window has its own dialogs, and a conflict elsewhere is not ours to answer.
      const conflict = Object.values(st.ops).some((o) => o.conflict && !o.done && o.id in st.opsMeta)
      // Every dialog or overlay that takes keys. While one is open, window shortcuts stay off.
      const modal =
        st.paletteOpen ||
        st.quickOpenOpen ||
        st.jumpOpen ||
        st.shortcutsOpen ||
        st.pluginsOpen ||
        st.pluginRequest !== null ||
        st.prompt !== null ||
        st.confirmDelete !== null ||
        st.confirmDiscard !== null ||
        st.commitOpen ||
        st.bulkRename !== null ||
        st.workspaceDialog !== null ||
        st.spaceFinderOpen ||
        st.duplicateFinderOpen ||
        st.compareOpen ||
        st.shellTarget !== null ||
        st.opsPanelOpen ||
        conflict
      const cmd = isMac ? e.metaKey : e.ctrlKey
      const key = e.key.toLowerCase()
      const typing = typingInField(e.target)
      const combo = `${cmd ? 'mod+' : ''}${e.shiftKey ? 'shift+' : ''}${e.altKey ? 'alt+' : ''}${key}`

      // These work everywhere, including from inside the search box.
      if (combo in GLOBAL_KEYS) {
        e.preventDefault()
        if (combo === 'mod+k') st.set({ paletteOpen: !st.paletteOpen, quickOpenOpen: false })
        else if (!modal) runCommand(GLOBAL_KEYS[combo])
        return
      }
      if (/^mod\+[1-9]$/.test(combo) && !modal) {
        e.preventDefault()
        st.switchToTabIndex(Number(key) - 1)
        return
      }
      if (modal || typing) return

      // Vim-style keys take `/` for the quick filter, and Ctrl+d for half a page
      // down in the list (Windows and Linux would otherwise run Split).
      if (st.vimMode && combo === '/') {
        e.preventDefault()
        runCommand('quick-filter')
        return
      }
      if (st.vimMode && !isMac && combo === 'mod+d') return
      // F2 renames one item, or opens bulk rename when several are selected.
      // F5 copies to the other pane when the window is split.
      let id = KEY_MAP[combo]
      if (combo === 'f2' && st.activeTab().selected.length > 1) id = 'bulk-rename'
      if (combo === 'f5' && st.splitTabId !== null) id = 'copy-to-pane'
      if (id) {
        e.preventDefault()
        runCommand(id)
        return
      }
      // Plugin shortcuts come last, so a built-in key never reaches a plugin.
      const plugin = pluginShortcuts(st.pluginReport).bound.get(combo)
      if (plugin) {
        e.preventDefault()
        if (!plugin.enabled || plugin.enabled()) void plugin.run()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])
}
