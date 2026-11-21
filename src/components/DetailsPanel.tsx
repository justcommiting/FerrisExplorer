import { useEffect, useMemo, useState } from 'react'
import { FolderOpen, GitBranch, Loader2 } from 'lucide-react'
import type { MarkdownIt } from 'markdown-it'
import type { LanguageFn } from 'highlight.js'
import { toast } from 'sonner'

import { FileIcon } from '@/components/FileIcon'
import {
  api,
  errorText,
  newRequestId,
  type ContentMatches,
  type GitBlame,
  type Preview,
  type PluginPreviewResult,
  type VNode,
} from '@/lib/api'
import { basename, formatFullDate, formatSize, kindLabel } from '@/lib/format'
import { filterValue } from '@/lib/query'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

const GIT_LABEL: Record<string, [string, string]> = {
  M: ['M · modified', 'text-git-modified'],
  '+': ['A · added', 'text-git-added'],
  U: ['U · untracked', 'text-git-untracked'],
  D: ['D · deleted', 'text-git-deleted'],
  I: ['ignored', 'text-dim'],
}

function Meta({ rows }: { rows: [string, React.ReactNode, string?][] }) {
  return (
    <dl className="m-0 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3.5 gap-y-1.5 text-xs">
      {rows.map(([k, v, cls]) => (
        <div key={k} className="contents">
          <dt className="text-muted-foreground">{k}</dt>
          <dd className={cn('selectable m-0 min-w-0 break-words', cls)}>{v}</dd>
        </div>
      ))}
    </dl>
  )
}

function ContentMatchesView({ path, needle }: { path: string; needle: string }) {
  const id = `${path}|${needle}`
  const [state, setState] = useState<{ id: string; data: ContentMatches | 'error' | null }>({ id: '', data: null })

  useEffect(() => {
    let live = true
    api
      .contentMatches(path, needle)
      .then((data) => live && setState({ id, data }))
      .catch(() => live && setState({ id, data: 'error' }))
    return () => {
      live = false
    }
  }, [id, path, needle])

  const cur = state.id === id ? state.data : null
  if (cur === null) {
    return <div className="text-xs text-dim">Searching inside the file…</div>
  }
  if (cur === 'error') {
    return <div className="text-xs text-dim">This file could not be searched.</div>
  }

  const shown = cur.snippets.length
  const summary =
    cur.matching_lines === 0
      ? 'No matches in this file'
      : `${cur.matching_lines} ${cur.matching_lines === 1 ? 'line' : 'lines'}${shown < cur.matching_lines ? `, first ${shown} shown` : ''}`

  return (
    <section aria-label="Matches" className="flex flex-col gap-2">
      <div className="text-[13px] font-semibold">
        Matches <span className="text-xs font-normal text-muted-foreground">· {summary}</span>
      </div>
      {shown > 0 && (
        <ul className="selectable m-0 flex max-h-72 list-none flex-col gap-1.5 overflow-auto rounded-md border bg-field p-2 font-mono text-[11.5px] leading-relaxed">
          {cur.snippets.map((snip) => (
            <li key={snip.line} className="flex gap-2">
              <span className="w-10 shrink-0 text-right text-dim">{snip.line}</span>
              <span className="min-w-0 break-words text-[#D8D5CE]">
                {snip.before}
                <mark className="rounded-sm bg-primary/30 px-0.5 text-foreground">{snip.matched}</mark>
                {snip.after}
              </span>
            </li>
          ))}
        </ul>
      )}
      {cur.excerpt !== null && (
        <>
          <div className="text-xs font-semibold text-muted-foreground">Extracted text</div>
          <pre className="selectable m-0 max-h-40 overflow-auto rounded-md border bg-field p-2 font-mono text-xs leading-[1.6] whitespace-pre-wrap text-[#D8D5CE]">
            {cur.excerpt || 'No text could be extracted.'}
          </pre>
        </>
      )}
    </section>
  )
}

/**
 * Each language is its own chunk, fetched the first time a file needs it. The
 * highlighter and the Markdown renderer are lazy for the same reason.
 */
const LANGUAGES: Record<string, () => Promise<{ default: LanguageFn }>> = {
  rust: () => import('highlight.js/lib/languages/rust'),
  python: () => import('highlight.js/lib/languages/python'),
  javascript: () => import('highlight.js/lib/languages/javascript'),
  typescript: () => import('highlight.js/lib/languages/typescript'),
  json: () => import('highlight.js/lib/languages/json'),
  css: () => import('highlight.js/lib/languages/css'),
  xml: () => import('highlight.js/lib/languages/xml'),
  yaml: () => import('highlight.js/lib/languages/yaml'),
  ini: () => import('highlight.js/lib/languages/ini'),
  bash: () => import('highlight.js/lib/languages/bash'),
  dos: () => import('highlight.js/lib/languages/dos'),
  powershell: () => import('highlight.js/lib/languages/powershell'),
  c: () => import('highlight.js/lib/languages/c'),
  cpp: () => import('highlight.js/lib/languages/cpp'),
  go: () => import('highlight.js/lib/languages/go'),
  java: () => import('highlight.js/lib/languages/java'),
}

/** HTML with highlight.js classes, or null when the language is not one we ship. Its output escapes the code. */
async function highlightCode(code: string, language: string): Promise<string | null> {
  const load = LANGUAGES[language]
  if (!load) return null
  const [{ default: hljs }] = await Promise.all([
    import('highlight.js/lib/core'),
    import('highlight.js/styles/github-dark.css'),
  ])
  if (!hljs.getLanguage(language)) hljs.registerLanguage(language, (await load()).default)
  return hljs.highlight(code, { language, ignoreIllegals: true }).value
}

let markdownPromise: Promise<MarkdownIt> | null = null

/** A Markdown renderer with raw HTML off, so the output is text and markup we wrote, never a script. */
function markdownRenderer() {
  markdownPromise ??= import('markdown-it').then(({ default: MarkdownIt }) => {
    const md = new MarkdownIt({ html: false, linkify: false, typographer: false })
    // The webview cannot load remote or relative images, so an image shows its alt text.
    md.renderer.rules.image = (tokens, idx) => md.utils.escapeHtml(tokens[idx].content)
    // Only web and mail links are rendered as links. Anything else stays text.
    md.validateLink = (url) => /^(https?:|mailto:)/i.test(url.trim())
    return md
  })
  return markdownPromise
}

function CodeView({ box, language, lines, truncated }: { box: string; language: string; lines: string[]; truncated: boolean }) {
  const text = lines.join('\n')
  const [state, setState] = useState<{ text: string; language: string; html: string | null } | null>(null)

  useEffect(() => {
    let live = true
    highlightCode(text, language)
      .then((html) => live && setState({ text, language, html }))
      .catch(() => live && setState({ text, language, html: null }))
    return () => {
      live = false
    }
  }, [text, language])

  const html = state && state.text === text && state.language === language ? state.html : null
  const cls = cn(box, 'selectable m-0 p-3 font-mono text-xs leading-[1.6] whitespace-pre text-[#D8D5CE]')
  return (
    <pre className={cls}>
      {html === null ? <code>{text}</code> : <code dangerouslySetInnerHTML={{ __html: html }} />}
      {truncated && <span className="block text-dim">…</span>}
    </pre>
  )
}

/** A link click opens in the default app or browser, never inside the webview. */
function openMarkdownLink(e: React.MouseEvent) {
  const link = (e.target as HTMLElement).closest('a')
  if (!link) return
  e.preventDefault()
  const href = link.getAttribute('href') ?? ''
  if (!/^(https?:|mailto:)/i.test(href)) return
  api.openPath(href).catch((err) => toast.error('Could not open link', { description: errorText(err) }))
}

function MarkdownView({ box, text, truncated }: { box: string; text: string; truncated: boolean }) {
  const [state, setState] = useState<{ text: string; html: string } | null>(null)

  useEffect(() => {
    let live = true
    markdownRenderer()
      .then((md) => live && setState({ text, html: md.render(text) }))
      .catch(() => live && setState({ text, html: '<p>This file could not be rendered.</p>' }))
    return () => {
      live = false
    }
  }, [text])

  const html = state && state.text === text ? state.html : null
  return (
    <div className={cn(box, 'p-3 text-[13px] leading-relaxed text-[#D8D5CE] [&_a]:text-primary [&_a]:underline [&_blockquote]:border-l-2 [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground [&_code]:rounded [&_code]:bg-accent [&_code]:px-1 [&_code]:font-mono [&_code]:text-[11.5px] [&_h1]:mb-2 [&_h1]:text-base [&_h1]:font-semibold [&_h2]:mb-2 [&_h2]:text-[14px] [&_h2]:font-semibold [&_h3]:mb-1 [&_h3]:font-semibold [&_li]:ml-4 [&_li]:list-disc [&_ol_li]:list-decimal [&_p]:my-2 [&_pre]:overflow-auto [&_pre]:rounded [&_pre]:bg-accent [&_pre]:p-2 [&_pre]:font-mono [&_pre]:text-[11.5px]')}>
      {html === null ? (
        <Loader2 className="size-4 animate-spin text-muted-foreground" />
      ) : (
        // Rendered with html:false, so the markup is ours. Links are caught by openMarkdownLink.
        // eslint-disable-next-line react/no-danger
        <div onClick={openMarkdownLink} dangerouslySetInnerHTML={{ __html: html }} />
      )}
      {truncated && <div className="text-dim">…</div>}
    </div>
  )
}

/** Hex and ASCII side by side, 16 bytes a row. `bytes` is already capped by the backend. */
function HexView({ box, bytes, size }: { box: string; bytes: number[]; size: number }) {
  const rows: number[][] = []
  for (let i = 0; i < bytes.length; i += 16) rows.push(bytes.slice(i, i + 16))
  return (
    <div className={cn(box, 'selectable p-3 font-mono text-[11px] leading-[1.6] text-[#D8D5CE]')}>
      {rows.map((row, i) => {
        const cells = Array.from({ length: 16 }, (_, k) => (k < row.length ? row[k].toString(16).padStart(2, '0') : '  '))
        const ascii = row.map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.')).join('')
        return (
          <div key={i} className="flex gap-3 whitespace-pre">
            <span className="text-dim">{(i * 16).toString(16).padStart(8, '0')}</span>
            <span>{cells.slice(0, 8).join(' ')}  {cells.slice(8).join(' ')}</span>
            <span className="text-muted-foreground">{ascii}</span>
          </div>
        )
      })}
      <div className="mt-2 font-sans text-[11px] text-dim">
        First {bytes.length.toLocaleString()} of {size.toLocaleString()} bytes
      </div>
    </div>
  )
}

function ActionButton({ primary, onClick, children }: { primary?: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      onClick={onClick}
      className={cn(
        'h-8 rounded-md px-3.5 text-[13px]',
        primary ? 'bg-primary font-semibold text-primary-foreground hover:bg-primary/90' : 'border border-input bg-accent hover:bg-[#2c2c29]',
      )}
    >
      {children}
    </button>
  )
}

/** A plugin previewer's output, as plain text. React escapes it, so nothing in it runs. */
function PluginPreviewBox({ box, result }: { box: string; result: PluginPreviewResult }) {
  return (
    <div className={cn(box, 'flex flex-col')}>
      <div className="px-3 pt-2 text-[11px] text-dim">{result.plugin}</div>
      {result.error && <div className="px-3 pt-1 text-xs text-destructive">{result.error}</div>}
      <pre className="selectable m-0 p-3 font-mono text-xs leading-[1.6] whitespace-pre-wrap text-[#D8D5CE]">
        {result.text}
        {result.truncated && <span className="block text-dim">…</span>}
      </pre>
    </div>
  )
}

export function PreviewView({ path }: { path: string }) {
  const [state, setState] = useState<{ path: string; preview: Preview | null }>({ path, preview: null })
  const [plugin, setPlugin] = useState<{ path: string; result: PluginPreviewResult } | null>(null)

  useEffect(() => {
    let live = true
    // An enabled plugin previewer for this file wins; the built-in preview is the fallback.
    // Unique across windows: the backend's preview registry is shared by all of them.
    const request = newRequestId()
    api
      .pluginPreview(path, request)
      .then((r) => {
        if (!live) return
        if (r) {
          setPlugin({ path, result: r })
          return
        }
        return api.preview(path).then((p) => live && setState({ path, preview: p }))
      })
      .catch(() => live && setState({ path, preview: { kind: 'none' } }))
    return () => {
      live = false
      // Stops a previewer that is still running once the selection has moved on.
      void api.cancelPluginPreview(request).catch(() => undefined)
    }
  }, [path])

  const p = state.path === path ? state.preview : null
  const pluginResult = plugin?.path === path ? plugin.result : null
  const box = 'min-h-40 flex-1 overflow-auto rounded-md border bg-field'
  if (pluginResult) {
    return <PluginPreviewBox box={box} result={pluginResult} />
  }
  if (!p) {
    return (
      <div className={cn(box, 'grid place-items-center')}>
        <Loader2 className="size-4 animate-spin text-muted-foreground" />
      </div>
    )
  }
  if (p.kind === 'image') {
    return (
      <div className={cn(box, 'grid place-items-center p-3')}>
        <img src={p.data_url} alt="" className="max-h-72 max-w-full object-contain" />
      </div>
    )
  }
  if (p.kind === 'text') {
    return (
      <pre className={cn(box, 'selectable m-0 p-3 font-mono text-xs leading-[1.6] whitespace-pre-wrap text-[#D8D5CE]')}>
        {p.lines.join('\n')}
        {p.truncated && <span className="block text-dim">…</span>}
      </pre>
    )
  }
  if (p.kind === 'code') {
    return <CodeView box={box} language={p.language} lines={p.lines} truncated={p.truncated} />
  }
  if (p.kind === 'markdown') {
    return <MarkdownView box={box} text={p.text} truncated={p.truncated} />
  }
  if (p.kind === 'video') {
    return (
      <div className={cn(box, 'grid place-items-center p-2')}>
        <video controls preload="metadata" src={p.data_url} className="max-h-72 w-full rounded" />
      </div>
    )
  }
  if (p.kind === 'audio') {
    return (
      <div className={cn(box, 'grid place-items-center p-3')}>
        <audio controls preload="metadata" src={p.data_url} className="w-full" />
      </div>
    )
  }
  if (p.kind === 'hex') {
    return <HexView box={box} bytes={p.bytes} size={p.size} />
  }
  return (
    <div className={cn(box, 'grid place-items-center text-xs text-dim')}>No preview for this file type</div>
  )
}

/** Blame runs only when asked: it is one git call for the file, not a per-line one. */
function BlameView({ path }: { path: string }) {
  const [state, setState] = useState<{ path: string; data: GitBlame | string | null }>({ path, data: null })

  useEffect(() => {
    let live = true
    api
      .gitBlame(path)
      .then((data) => live && setState({ path, data }))
      .catch((e) => live && setState({ path, data: errorText(e) }))
    return () => {
      live = false
    }
  }, [path])

  const box = 'min-h-40 flex-1 overflow-auto rounded-md border bg-field'
  const cur = state.path === path ? state.data : null
  if (cur === null) {
    return (
      <div className={cn(box, 'grid place-items-center')}>
        <Loader2 className="size-4 animate-spin text-muted-foreground" />
      </div>
    )
  }
  if (typeof cur === 'string') {
    return <div className={cn(box, 'grid place-items-center p-3 text-center text-xs text-dim')}>{cur}</div>
  }
  return (
    <div className={cn(box, 'py-2 font-mono text-[11px] leading-[1.6]')}>
      {cur.lines.map((l) => (
        <div key={l.line} className="flex gap-2 px-2 whitespace-pre">
          <span className="w-8 shrink-0 text-right text-dim">{l.line}</span>
          <span className="w-16 shrink-0 text-git-modified" title="Commit">{l.sha.slice(0, 8)}</span>
          <span className="w-24 shrink-0 truncate text-muted-foreground" title={l.author}>{l.author}</span>
          <span className="w-20 shrink-0 text-muted-foreground" title="Author date">
            {l.time > 0 ? new Date(l.time * 1000).toISOString().slice(0, 10) : ''}
          </span>
          <span className="selectable text-[#D8D5CE]">{l.text}</span>
        </div>
      ))}
      {cur.truncated && <div className="px-2 text-dim">… first 2000 lines only</div>}
    </div>
  )
}

export function DetailsPanel() {
  const tab = useExplorer((s) => s.activeTab())
  const dirSizes = useExplorer((s) => s.dirSizes)
  const calculateDirSizes = useExplorer((s) => s.calculateDirSizes)
  const [blame, setBlame] = useState(false)

  const nodes = useMemo(() => {
    const pool = tab.activeQuery !== null ? tab.results : tab.items
    const sel = new Set(tab.selected)
    return pool.filter((n) => sel.has(n.path))
  }, [tab.activeQuery, tab.results, tab.items, tab.selected])

  const sizeOf = (n: VNode) => (n.is_dir ? dirSizes[n.path] : n.size)
  // The `content:` term of the active search, if any. Its matches are shown in the preview.
  const contentNeedle = useMemo(() => {
    const raw = tab.activeQuery ? filterValue(tab.activeQuery, 'content') : null
    return raw ? raw.replace(/^"(.*)"$/, '$1') : ''
  }, [tab.activeQuery])
  const st = useExplorer.getState

  return (
    <aside aria-label="Preview" className="flex w-80 shrink-0 flex-col gap-3.5 overflow-y-auto border-l bg-card p-4">
      {nodes.length === 0 && (
        <>
          <div className="flex items-start gap-3">
            <div className="grid size-10 shrink-0 place-items-center rounded-lg bg-accent">
              <FolderOpen className="size-5 text-folder" strokeWidth={1.5} />
            </div>
            <div className="min-w-0">
              <div className="text-[15px] font-semibold break-words">{basename(tab.dir) || tab.dir}</div>
              <div className="text-xs text-muted-foreground">
                Folder · {tab.items.length} items
                {typeof dirSizes[tab.dir] === 'number' && ` · ${formatSize(dirSizes[tab.dir] as number)}`}
                {dirSizes[tab.dir] === 'pending' && ' · measuring…'}
              </div>
            </div>
          </div>
          <Meta
            rows={[
              ['Path', <span key="v" className="font-mono text-[11.5px]">{tab.dir}</span>],
              ...(tab.git
                ? ([
                    ['Branch', <span key="v" className="inline-flex items-center gap-1 font-mono"><GitBranch className="size-3" />{tab.git.branch}</span>, 'text-git-modified'],
                    ['Changes', String(Object.values(tab.git.marks).filter((m) => m !== 'I').length)],
                  ] as [string, React.ReactNode, string?][])
                : []),
            ]}
          />
          <p className="text-xs leading-relaxed text-dim">
            Select a file to preview it. <kbd className="rounded border border-input px-1 font-mono text-[11px]">Space</kbd> hides this panel.
          </p>
        </>
      )}

      {nodes.length === 1 && (() => {
        const n = nodes[0]
        const size = sizeOf(n)
        const mark = tab.git?.marks[n.path]
        const sizeText = n.is_dir
          ? typeof size === 'number' ? formatSize(size) : size === 'pending' ? 'calculating…' : null
          : formatSize(n.size)
        return (
          <>
            <div className="flex items-start gap-3">
              <div className="grid size-10 shrink-0 place-items-center rounded-lg bg-accent">
                <FileIcon node={n} className="size-5" />
              </div>
              <div className="min-w-0">
                <div className="selectable text-[15px] font-semibold [overflow-wrap:anywhere]">{n.name}</div>
                <div className="text-xs text-muted-foreground">
                  {kindLabel(n)}
                  {sizeText && ` · ${sizeText}`}
                </div>
              </div>
            </div>
            <Meta
              rows={[
                ['Modified', formatFullDate(n.modified_at)],
                ['Created', formatFullDate(n.created_at)],
                ...(mark ? ([['Git', GIT_LABEL[mark]?.[0] ?? mark, GIT_LABEL[mark]?.[1]]] as [string, React.ReactNode, string?][]) : []),
                ['Access', <span key="v" className="font-mono">{n.readonly ? 'read-only' : 'read/write'}</span>],
                ...(n.is_dir && typeof size !== 'number' && size !== 'pending'
                  ? ([['Size', <button key="v" className="text-primary hover:underline" onClick={() => calculateDirSizes([n.path])}>Calculate</button>]] as [string, React.ReactNode, string?][])
                  : []),
                ['Path', <span key="v" className="font-mono text-[11.5px]">{n.path}</span>],
              ]}
            />
            {!n.is_dir && (blame && tab.git ? <BlameView key={n.path} path={n.path} /> : <PreviewView key={n.path} path={n.path} />)}
            {!n.is_dir && contentNeedle && <ContentMatchesView path={n.path} needle={contentNeedle} />}
            <div className="flex flex-wrap gap-2">
              <ActionButton primary onClick={() => void st().openNode(n)}>Open</ActionButton>
              <ActionButton onClick={() => void st().duplicateSelection()}>Duplicate</ActionButton>
              <ActionButton onClick={() => void st().compressSelection()}>Compress</ActionButton>
              {tab.git && !n.is_dir && (
                <ActionButton onClick={() => setBlame(!blame)}>{blame ? 'Hide blame' : 'Blame'}</ActionButton>
              )}
            </div>
          </>
        )
      })()}

      {nodes.length > 1 && (
        <>
          <div className="text-[15px] font-semibold">{nodes.length} items selected</div>
          <Meta
            rows={[
              ['Files', String(nodes.filter((n) => !n.is_dir).length)],
              ['Folders', String(nodes.filter((n) => n.is_dir).length)],
              [
                'Size',
                formatSize(nodes.reduce((acc, n) => acc + (typeof sizeOf(n) === 'number' ? (sizeOf(n) as number) : 0), 0)) +
                  (nodes.some((n) => n.is_dir && typeof sizeOf(n) !== 'number') ? ' + folders' : ''),
              ],
            ]}
          />
          <div className="flex flex-wrap gap-2">
            <ActionButton onClick={() => void st().duplicateSelection()}>Duplicate</ActionButton>
            <ActionButton onClick={() => st().set({ prompt: { kind: 'moveTo', initial: tab.dir } })}>Move to…</ActionButton>
          </div>
        </>
      )}
    </aside>
  )
}
