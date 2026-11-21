// Bulk rename, the pure part: turns the dialog's options into a new name for
// each item and flags the rows that cannot be applied. No DOM and no Tauri,
// so it can be tested on its own. The backend checks everything again.

export interface Item {
  /** Full path, as the backend knows it. */
  path: string
  /** The name without its folder. */
  name: string
  /** The folder, ending in its separator. */
  parent: string
  /** The folder's own name, for {parent}. */
  parentName: string
  /** Last modified, in seconds since the epoch. */
  modified: number
}

export type RenameMode = 'template' | 'replace'
export type CaseMode = 'keep' | 'lower' | 'upper' | 'title'

export interface RenameOptions {
  mode: RenameMode
  template: string
  find: string
  replacement: string
  regex: boolean
  caseSensitive: boolean
  /** Replace only in the part before the extension. */
  stemOnly: boolean
  caseMode: CaseMode
  start: number
  step: number
}

export const DEFAULT_OPTIONS: RenameOptions = {
  mode: 'template',
  template: '{name}.{ext}',
  find: '',
  replacement: '',
  regex: false,
  caseSensitive: false,
  stemOnly: true,
  caseMode: 'keep',
  start: 1,
  step: 1,
}

export interface Platform {
  windows: boolean
  /** macOS and Windows treat names differing only in case as the same name. */
  caseInsensitive: boolean
}

export interface PlannedRow {
  item: Item
  newName: string
  /** The name stays as it is. Unchanged rows are skipped when applying. */
  unchanged: boolean
  error: string | null
}

export interface Plan {
  /** A problem with the options as a whole. When set, there are no rows. */
  error: string | null
  rows: PlannedRow[]
}

/** Split a path into its folder (with the trailing separator) and its name. */
export function splitPath(path: string): { parent: string; name: string } {
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return { parent: path.slice(0, i + 1), name: path.slice(i + 1) }
}

/** The name of a folder, for {parent}. Empty at a drive or filesystem root. */
export function folderName(parent: string): string {
  return parent.replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? ''
}

/** The stem and extension. A leading dot is part of the name, not an extension. */
export function splitName(name: string): { stem: string; ext: string; suffix: string } {
  const dot = name.lastIndexOf('.')
  if (dot <= 0) return { stem: name, ext: '', suffix: '' }
  return { stem: name.slice(0, dot), ext: name.slice(dot + 1), suffix: name.slice(dot) }
}

// ── Template ──────────────────────────────────────────────────────────────

const TOKEN = /\{([^{}]*)\}/g
const COUNTER = /^n(?::(\d{1,3}))?$/

type Token = { kind: 'name' | 'ext' | 'parent' | 'date' } | { kind: 'n'; width: number }

function parseToken(body: string): Token | null {
  if (body === 'name' || body === 'ext' || body === 'parent' || body === 'date') return { kind: body }
  const m = COUNTER.exec(body)
  if (m) return { kind: 'n', width: m[1] ? Number(m[1]) : 0 }
  return null
}

/** Why the template cannot be used, or null when it can. */
export function templateError(template: string): string | null {
  if (!template.trim()) return 'The template is empty'
  for (const m of template.matchAll(TOKEN)) {
    if (!parseToken(m[1])) return `Unknown token {${m[1]}}`
  }
  if (/[{}]/.test(template.replace(TOKEN, ''))) return 'Braces must form a token such as {name}'
  return null
}

function ymd(secs: number): string {
  const d = new Date(secs * 1000)
  const two = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())}`
}

/**
 * Fill in a template for one item. `{ext}` with no extension also drops the
 * dot in front of it, so "{name}.{ext}" leaves a file without one unchanged.
 */
export function expandTemplate(template: string, item: Item, index: number, opts: RenameOptions): string {
  const { stem, ext } = splitName(item.name)
  let out = ''
  let last = 0
  for (const m of template.matchAll(TOKEN)) {
    out += template.slice(last, m.index ?? 0)
    last = (m.index ?? 0) + m[0].length
    const token = parseToken(m[1])
    if (!token) continue
    switch (token.kind) {
      case 'name':
        out += stem
        break
      case 'ext':
        if (!ext && out.endsWith('.')) out = out.slice(0, -1)
        out += ext
        break
      case 'parent':
        out += item.parentName
        break
      case 'date':
        out += ymd(item.modified)
        break
      case 'n':
        out += String(opts.start + index * opts.step).padStart(token.width, '0')
        break
    }
  }
  return out + template.slice(last)
}

// ── Find and replace ──────────────────────────────────────────────────────

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&')
}

/** Throws a SyntaxError when the pattern is not a valid regular expression. */
function compile(opts: RenameOptions): RegExp {
  return new RegExp(opts.regex ? opts.find : escapeRegExp(opts.find), opts.caseSensitive ? 'g' : 'gi')
}

/** Replace in `text`. Plain text is literal, and regex replacements can use $1 and so on. */
export function replaceText(text: string, opts: RenameOptions): string {
  if (!opts.find) return text
  const replacement = opts.regex ? opts.replacement : opts.replacement.replace(/\$/g, '$$$$')
  return text.replace(compile(opts), replacement)
}

// ── Case ──────────────────────────────────────────────────────────────────

export function applyCase(text: string, mode: CaseMode): string {
  switch (mode) {
    case 'lower':
      return text.toLowerCase()
    case 'upper':
      return text.toUpperCase()
    case 'title':
      return text
        .toLowerCase()
        .replace(/(^|[^\p{L}\p{N}'’])(\p{L})/gu, (_, sep: string, ch: string) => sep + ch.toUpperCase())
    default:
      return text
  }
}

/** Case for a whole new name. Title Case covers the stem; the extension is lowercased, not capitalized. */
function caseName(name: string, mode: CaseMode): string {
  if (mode !== 'title') return applyCase(name, mode)
  const { stem, suffix } = splitName(name)
  return applyCase(stem, 'title') + suffix.toLowerCase()
}

// ── Planning ──────────────────────────────────────────────────────────────

/** Why the options cannot be used at all, or null. */
export function optionsError(opts: RenameOptions): string | null {
  if (opts.mode === 'template') {
    const err = templateError(opts.template)
    if (err) return err
    if (!Number.isInteger(opts.start) || !Number.isInteger(opts.step)) return 'Start and step must be whole numbers'
    return null
  }
  if (opts.find) {
    try {
      compile(opts)
    } catch (e) {
      return `Not a valid pattern: ${e instanceof Error ? e.message : String(e)}`
    }
  }
  return null
}

/** The new name for the item at `index`, before any validation. */
export function newNameFor(item: Item, index: number, opts: RenameOptions): string {
  if (opts.mode === 'template') return caseName(expandTemplate(opts.template, item, index, opts), opts.caseMode)
  const { stem, suffix } = splitName(item.name)
  const out = opts.stemOnly ? replaceText(stem, opts) + suffix : replaceText(item.name, opts)
  return caseName(out, opts.caseMode)
}

/** Why a name cannot be used on this platform, or null. */
export function nameError(name: string, platform: Platform): string | null {
  if (!name.trim()) return 'Name is empty'
  if (name === '.' || name === '..') return `"${name}" is not a valid name`
  if (/[/\\]/.test(name)) return 'Names cannot contain / or \\'
  if (platform.windows && /[:*?"<>|]/.test(name)) return 'Contains a character Windows does not allow: : * ? " < > |'
  if (platform.windows && [...name].some((c) => c.charCodeAt(0) < 32)) return 'Contains a control character'
  if (platform.windows && /[. ]$/.test(name)) return 'Windows does not allow a name to end with a dot or a space'
  if (platform.windows && isWindowsReserved(name)) return `"${name}" is a reserved device name on Windows`
  return null
}

/**
 * True for the names Windows reserves for devices: CON, PRN, AUX, NUL,
 * COM1-COM9 and LPT1-LPT9, case-insensitively. The part before the first dot
 * counts, so `nul.txt` is reserved too. Mirrors `is_windows_reserved` in bulk_rename.rs.
 */
export function isWindowsReserved(name: string): boolean {
  const stem = (name.split('.')[0] ?? '').replace(/ +$/, '').toUpperCase()
  return /^(CON|PRN|AUX|NUL|(COM|LPT)[1-9])$/.test(stem)
}

/**
 * Compute the new name of every item and flag the rows that cannot be applied.
 * `listed` maps each folder to the names already in it, so a new name that
 * would overwrite a file outside the selection is caught. An item being
 * renamed away does not block its old name, which keeps swaps and cycles valid.
 */
export function planRenames(
  items: Item[],
  opts: RenameOptions,
  listed: Map<string, Set<string>>,
  platform: Platform,
): Plan {
  const error = optionsError(opts)
  if (error) return { error, rows: [] }

  const fold = (s: string) => (platform.caseInsensitive ? s.toLowerCase() : s)
  const keyOf = (parent: string, name: string) => `${parent}\n${fold(name)}`

  const sorted = [...items].sort((a, b) => a.path.localeCompare(b.path, undefined, { numeric: true }))
  const rows: PlannedRow[] = sorted.map((item, i) => {
    const newName = newNameFor(item, i, opts)
    return { item, newName, unchanged: newName === item.name, error: null }
  })

  const claims = new Map<string, number>()
  for (const r of rows) {
    const k = keyOf(r.item.parent, r.newName)
    claims.set(k, (claims.get(k) ?? 0) + 1)
  }
  const existing = new Set<string>()
  for (const [parent, names] of listed) for (const n of names) existing.add(keyOf(parent, n))
  const vacated = new Set(rows.filter((r) => !r.unchanged).map((r) => keyOf(r.item.parent, r.item.name)))

  for (const r of rows) {
    const k = keyOf(r.item.parent, r.newName)
    r.error = nameError(r.newName, platform)
    if (!r.error && (claims.get(k) ?? 0) > 1) r.error = 'Another item gets this name'
    if (!r.error && !r.unchanged && existing.has(k) && !vacated.has(k)) {
      r.error = 'A file with this name already exists'
    }
  }
  return { error: null, rows }
}
