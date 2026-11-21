import type { VNode } from './api'

const SECS_PER_DAY = 86_400

/** Binary units, matching how the search filters parse `size:>1mb`. */
export function formatSize(bytes: number): string {
  if (bytes === 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let size = bytes
  let i = 0
  while (size >= 1024 && i < units.length - 1) {
    size /= 1024
    i++
  }
  return i === 0 ? `${size} B` : `${size.toFixed(1)} ${units[i]}`
}

export function formatTimestamp(secs: number, nowMs = Date.now()): string {
  if (!secs) return '--'
  const now = Math.floor(nowMs / 1000)
  const daysAgo = Math.floor(Math.max(0, now - secs) / SECS_PER_DAY)
  if (daysAgo === 0) return 'Today'
  if (daysAgo === 1) return 'Yesterday'
  if (daysAgo < 7) return `${daysAgo} days ago`
  return new Date(secs * 1000).toLocaleDateString()
}

export function formatFullDate(secs: number): string {
  if (!secs) return 'Unknown'
  return new Date(secs * 1000).toLocaleString()
}

const KINDS: Record<string, string> = {
  rs: 'Rust Source',
  py: 'Python',
  js: 'JavaScript',
  ts: 'TypeScript',
  tsx: 'TypeScript',
  jsx: 'JavaScript',
  toml: 'TOML',
  json: 'JSON',
  md: 'Markdown',
  pdf: 'PDF Document',
  html: 'Web',
  css: 'Web',
  sh: 'Script',
  bat: 'Script',
  ps1: 'Script',
  exe: 'Executable',
  dll: 'Executable',
  msi: 'Executable',
  yaml: 'YAML',
  yml: 'YAML',
  xml: 'XML',
  csv: 'CSV',
  txt: 'Text',
  log: 'Log',
}
const GROUPS: [string[], string][] = [
  [['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg', 'ico'], 'Image'],
  [['mp4', 'avi', 'mkv', 'mov', 'webm'], 'Video'],
  [['mp3', 'wav', 'flac', 'ogg', 'm4a'], 'Audio'],
  [['zip', 'tar', 'gz', 'rar', '7z'], 'Archive'],
]

export function extension(name: string): string {
  const dot = name.lastIndexOf('.')
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : ''
}

export function kindLabel(node: VNode): string {
  if (node.is_dir) return 'Folder'
  const ext = extension(node.name)
  if (!ext) return 'File'
  if (KINDS[ext]) return KINDS[ext]
  for (const [exts, label] of GROUPS) if (exts.includes(ext)) return label
  return `${ext.toUpperCase()} File`
}

// ── Paths ─────────────────────────────────────────────────────────────────
// Paths come from Rust as native strings; on Windows that means `\`.

export function sepOf(path: string): string {
  return path.includes('\\') ? '\\' : '/'
}

export function basename(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, '')
  const i = Math.max(trimmed.lastIndexOf('\\'), trimmed.lastIndexOf('/'))
  return i >= 0 ? trimmed.slice(i + 1) || path : trimmed || path
}

export function parentOf(path: string): string | null {
  const sep = sepOf(path)
  const trimmed = path.length > 1 && !/^[A-Za-z]:\\$/.test(path) ? path.replace(/[\\/]+$/, '') : path
  if (/^[A-Za-z]:\\?$/.test(trimmed) || trimmed === '/') return null
  const i = trimmed.lastIndexOf(sep)
  if (i < 0) return null
  const parent = trimmed.slice(0, i)
  if (/^[A-Za-z]:$/.test(parent)) return parent + '\\'
  return parent || sep
}

/** Breadcrumb segments: [label, fullPath][] */
export function crumbs(path: string): [string, string][] {
  const out: [string, string][] = []
  let cur: string | null = path
  while (cur) {
    out.unshift([basename(cur) || cur, cur])
    cur = parentOf(cur)
  }
  return out
}

export function relativeTo(path: string, root: string): string {
  const r = root.replace(/[\\/]+$/, '')
  if (path.startsWith(r) && path.length > r.length) return path.slice(r.length + 1)
  return path
}
