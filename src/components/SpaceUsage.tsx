import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ArrowUp, ChevronRight, ExternalLink, Loader2, PieChart, RotateCw } from 'lucide-react'

import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog'
import {
  api,
  errorText,
  spaceHistory,
  type SpaceChange,
  type SpaceChanges,
  type SpaceNode,
  type SpacePeriod,
} from '@/lib/api'
import { basename, formatSize } from '@/lib/format'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

// ── Squarified treemap layout (Bruls, Huizing & van Wijk) ─────────────────

interface Rect {
  x: number
  y: number
  w: number
  h: number
}
interface Placed<T> extends Rect {
  item: T
}

function worst(row: number[], side: number): number {
  const sum = row.reduce((a, b) => a + b, 0)
  const max = Math.max(...row)
  const min = Math.min(...row)
  const s2 = side * side
  const sum2 = sum * sum
  return Math.max((s2 * max) / sum2, sum2 / (s2 * min))
}

function squarify<T>(items: T[], sizeOf: (t: T) => number, box: Rect): Placed<T>[] {
  const total = items.reduce((a, t) => a + sizeOf(t), 0)
  if (!total || box.w <= 0 || box.h <= 0) return []
  const scale = (box.w * box.h) / total
  const areas = items.map((t) => sizeOf(t) * scale)
  const out: Placed<T>[] = []
  let { x, y, w, h } = box
  let i = 0
  while (i < items.length) {
    const side = Math.min(w, h)
    const row = [areas[i]]
    let j = i + 1
    while (j < items.length && worst([...row, areas[j]], side) <= worst(row, side)) {
      row.push(areas[j])
      j++
    }
    const rowSum = row.reduce((a, b) => a + b, 0)
    if (w >= h) {
      // Lay the row out as a column on the left.
      const cw = rowSum / h
      let cy = y
      row.forEach((a, k) => {
        const ch = a / cw
        out.push({ x, y: cy, w: cw, h: ch, item: items[i + k] })
        cy += ch
      })
      x += cw
      w -= cw
    } else {
      const rh = rowSum / w
      let cx = x
      row.forEach((a, k) => {
        const rw = a / rh
        out.push({ x: cx, y, w: rw, h: rh, item: items[i + k] })
        cx += rw
      })
      y += rh
      h -= rh
    }
    i = j
  }
  return out
}

// ── View ──────────────────────────────────────────────────────────────────

type Tile =
  | { kind: 'dir'; node: SpaceNode }
  | { kind: 'files'; size: number }
  | { kind: 'others'; count: number; size: number }

const PALETTE = ['#E58A4E', '#5CC0C8', '#7CC47F', '#C9A0F0', '#E0B450', '#6FA8E8', '#E5786D', '#C9A06B']

function tilesOf(n: SpaceNode): Tile[] {
  const t: Tile[] = n.children.map((c) => ({ kind: 'dir', node: c }) as Tile)
  if (n.direct > 0) t.push({ kind: 'files', size: n.direct })
  if (n.other_count > 0) t.push({ kind: 'others', count: n.other_count, size: n.other_size })
  return t.sort((a, b) => sizeOfTile(b) - sizeOfTile(a))
}
const sizeOfTile = (t: Tile) => (t.kind === 'dir' ? t.node.size : t.size)
const labelOf = (t: Tile) =>
  t.kind === 'dir' ? t.node.name : t.kind === 'files' ? 'Files here' : `${t.count} other folder${t.count === 1 ? '' : 's'}`
const keyOf = (t: Tile) => (t.kind === 'dir' ? t.node.path : t.kind)

function useSize(ref: React.RefObject<HTMLElement | null>) {
  const [size, setSize] = useState({ w: 0, h: 0 })
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const ro = new ResizeObserver(([e]) => setSize({ w: e.contentRect.width, h: e.contentRect.height }))
    ro.observe(el)
    return () => ro.disconnect()
  }, [ref])
  return size
}

function Treemap({
  root,
  selected,
  onSelect,
  onDrill,
}: {
  root: SpaceNode
  selected: string | null
  onSelect: (t: Tile) => void
  onDrill: (n: SpaceNode) => void
}) {
  const ref = useRef<HTMLDivElement>(null)
  const { w, h } = useSize(ref)
  const placed = useMemo(() => squarify(tilesOf(root), sizeOfTile, { x: 0, y: 0, w, h }), [root, w, h])

  return (
    <div ref={ref} role="group" aria-label={`Treemap of ${root.name} by size`} className="relative min-h-0 flex-1 overflow-hidden rounded-lg">
      {placed.map((p, i) => {
        const t = p.item
        const color = t.kind === 'dir' ? PALETTE[i % PALETTE.length] : '#5c5a55'
        const isSel = keyOf(t) === selected
        const big = p.w > 110 && p.h > 54
        const inner = t.kind === 'dir' && p.w > 160 && p.h > 110 ? tilesOf(t.node).filter((x) => x.kind === 'dir') : []
        const sub = inner.length
          ? squarify(inner, sizeOfTile, { x: 0, y: 0, w: p.w - 12, h: p.h - 42 })
          : []
        return (
          <button
            key={keyOf(t)}
            aria-pressed={isSel}
            aria-label={`${labelOf(t)}, ${formatSize(sizeOfTile(t))}`}
            onClick={() => onSelect(t)}
            onDoubleClick={() => t.kind === 'dir' && onDrill(t.node)}
            style={{
              left: p.x + 1.5,
              top: p.y + 1.5,
              width: Math.max(0, p.w - 3),
              height: Math.max(0, p.h - 3),
              background: `color-mix(in oklab, ${color} ${isSel ? 34 : 20}%, #111110)`,
              boxShadow: isSel ? `inset 0 0 0 2px ${color}` : `inset 0 0 0 1px color-mix(in oklab, ${color} 35%, transparent)`,
            }}
            className="absolute overflow-hidden rounded-md p-2 text-left transition-[background] hover:brightness-125"
          >
            {big && (
              <span className="relative z-10 flex items-baseline justify-between gap-2">
                <span className="truncate text-[13px] font-medium">{labelOf(t)}</span>
                <span className="shrink-0 font-mono text-xs text-muted-foreground">{formatSize(sizeOfTile(t))}</span>
              </span>
            )}
            {sub.map((q) => (
              <span
                key={keyOf(q.item)}
                style={{
                  left: q.x + 6,
                  top: q.y + 34,
                  width: Math.max(0, q.w - 2),
                  height: Math.max(0, q.h - 2),
                  background: `color-mix(in oklab, ${color} 14%, #111110)`,
                }}
                className="absolute overflow-hidden rounded px-1.5 py-1 text-[11px] text-muted-foreground"
              >
                {q.w > 70 && q.h > 22 && <span className="block truncate">{labelOf(q.item)}</span>}
              </span>
            ))}
          </button>
        )
      })}
    </div>
  )
}

// ── What changed ──────────────────────────────────────────────────────────

const PERIODS: { value: SpacePeriod; label: string }[] = [
  { value: 'day', label: 'Day' },
  { value: 'week', label: 'Week' },
  { value: 'month', label: 'Month' },
]

const signed = (bytes: number) => `${bytes >= 0 ? '+' : '\u2212'}${formatSize(Math.abs(bytes))}`

function ChangeSection({ title, rows, onOpen }: { title: string; rows: SpaceChange[]; onOpen: (path: string) => void }) {
  const max = Math.max(1, ...rows.map((r) => Math.abs(r.delta)))
  return (
    <section className="flex flex-col gap-1">
      <h2 className="m-0 mb-1 text-[11px] font-normal tracking-[.08em] text-muted-foreground uppercase">
        {title} · {rows.length}
      </h2>
      <ol className="m-0 flex list-none flex-col gap-0.5 p-0">
        {rows.map((r) => {
          const up = r.delta >= 0
          const gone = r.new_size === 0
          return (
            <li key={r.path}>
              <button
                onClick={() => onOpen(r.path)}
                disabled={gone}
                title={`${r.path}\n${formatSize(r.old_size)} \u2192 ${formatSize(r.new_size)}`}
                className="flex w-full flex-col gap-1 rounded-md px-2 py-1.5 text-left hover:bg-accent disabled:cursor-default disabled:hover:bg-transparent"
              >
                <span className="flex justify-between gap-3 text-xs">
                  <span className={cn('truncate', gone && 'text-muted-foreground line-through')}>{basename(r.path) || r.path}</span>
                  <span className={cn('shrink-0 font-mono', up ? 'text-primary' : 'text-muted-foreground')}>{signed(r.delta)}</span>
                </span>
                <span className="h-1 overflow-hidden rounded-sm bg-border">
                  <span
                    className={cn('block h-full', up ? 'bg-primary' : 'bg-muted-foreground')}
                    style={{ width: `${Math.max(2, (Math.abs(r.delta) / max) * 100)}%` }}
                  />
                </span>
              </button>
            </li>
          )
        })}
      </ol>
    </section>
  )
}

function ChangeLists({ diff, onOpen }: { diff: SpaceChanges['diff']; onOpen: (path: string) => void }) {
  const sections: [string, SpaceChange[]][] = [
    ['Grew', diff.grown],
    ['Shrank', diff.shrunk],
    ['New folders', diff.new],
    ['Deleted', diff.deleted],
  ]
  const shown = sections.filter(([, rows]) => rows.length > 0)
  if (shown.length === 0) {
    return <p className="m-auto text-sm text-muted-foreground">No folder changed noticeably.</p>
  }
  return (
    <>
      {shown.map(([title, rows]) => (
        <ChangeSection key={title} title={title} rows={rows} onOpen={onOpen} />
      ))}
    </>
  )
}

function ChangesView({ onOpen }: { onOpen: (path: string) => void }) {
  const [period, setPeriod] = useState<SpacePeriod>('week')
  // Tagged with the period it answers, so a stale result never shows under a new period.
  const [result, setResult] = useState<{ period: SpacePeriod; data?: SpaceChanges; error?: string } | null>(null)

  useEffect(() => {
    let current = true
    spaceHistory.changes(period).then(
      (data) => {
        if (current) setResult({ period, data })
      },
      (e) => {
        if (current) setResult({ period, error: errorText(e) })
      },
    )
    return () => {
      current = false
    }
  }, [period])
  const shown = result?.period === period ? result : null
  const data = shown?.data ?? null
  const error = shown?.error ?? null

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto">
      <div className="flex flex-wrap items-center gap-3">
        <div role="group" aria-label="Compare with" className="flex rounded-md border border-input p-0.5">
          {PERIODS.map((p) => (
            <button
              key={p.value}
              aria-pressed={period === p.value}
              onClick={() => setPeriod(p.value)}
              className={cn(
                'h-7 rounded px-3 text-[13px]',
                period === p.value ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent',
              )}
            >
              {p.label}
            </button>
          ))}
        </div>
        {data?.baseline_at != null && (
          <span className="text-xs text-muted-foreground">
            Compared with {new Date(data.baseline_at * 1000).toLocaleString()}
          </span>
        )}
      </div>
      {error && <p className="m-auto text-sm text-muted-foreground">{error}</p>}
      {!error && !data && (
        <div className="m-auto flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" /> Comparing…
        </div>
      )}
      {!error && data && data.baseline_at === null && (
        <p className="m-auto max-w-md text-center text-sm text-muted-foreground">
          {data.first_snapshot_at === null
            ? 'Not enough history yet. The first snapshot is taken after the next index build, or within a day.'
            : `Not enough history yet. First snapshot taken ${new Date(data.first_snapshot_at * 1000).toLocaleString()}.`}
        </p>
      )}
      {!error && data && data.baseline_at !== null && <ChangeLists diff={data.diff} onOpen={onOpen} />}
    </div>
  )
}

export function SpaceUsage() {
  const open = useExplorer((s) => s.spaceFinderOpen)
  const set = useExplorer((s) => s.set)
  const home = useExplorer((s) => s.home)
  const view = useExplorer((s) => s.spaceFinderView)
  const [root, setRoot] = useState<string | null>(null)
  const [data, setData] = useState<{ tree: SpaceNode; from_index: boolean } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [selected, setSelected] = useState<Tile | null>(null)
  const [trail, setTrail] = useState<string[]>([])

  const load = useCallback(async (path: string | null, refresh = false) => {
    setLoading(true)
    setError(null)
    try {
      const d = await api.spaceTree(path, refresh)
      setData(d)
      setRoot(d.tree.path)
      setSelected(null)
    } catch (e) {
      if (errorText(e) !== 'cancelled') setError(errorText(e))
    }
    setLoading(false)
  }, [])

  const [wasOpen, setWasOpen] = useState(open)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) setTrail([])
    else setData(null)
  }

  useEffect(() => {
    if (open) {
      // Starting the scan is the external sync; its loading flag must flip with it.
      // oxlint-disable-next-line react/set-state-in-effect
      void load(useExplorer.getState().activeTab().dir ?? home)
    } else {
      void api.cancelSpaceScan()
    }
  }, [open, load, home])

  const drill = (n: SpaceNode) => {
    if (root) setTrail((t) => [...t, root])
    void load(n.path)
  }
  const up = () => {
    const prev = trail[trail.length - 1]
    if (prev) {
      setTrail((t) => t.slice(0, -1))
      void load(prev)
    } else if (data) {
      const parent = data.tree.path.replace(/[\\/][^\\/]+[\\/]?$/, '')
      if (parent && parent !== data.tree.path) void load(/^[A-Za-z]:$/.test(parent) ? parent + '\\' : parent)
    }
  }
  const openChange = (path: string) => {
    setTrail([])
    set({ spaceFinderView: 'map' })
    void load(path)
  }
  const openInExplorer = (path: string) => {
    set({ spaceFinderOpen: false })
    void useExplorer.getState().navigate(path)
  }

  const tree = data?.tree
  const ranking = tree ? tilesOf(tree) : []
  const detail = selected?.kind === 'dir' ? selected.node : tree

  return (
    <Dialog open={open} onOpenChange={(o) => set({ spaceFinderOpen: o })}>
      <DialogContent
        className="flex h-[86vh] max-w-none flex-col gap-0 p-0 sm:max-w-[min(1200px,94vw)]"
        onKeyDown={(e) => {
          if (view === 'changes') return
          if (e.key === 'Backspace') {
            e.preventDefault()
            up()
          } else if (e.key === 'Enter' && selected?.kind === 'dir') {
            drill(selected.node)
          }
        }}
      >
        <header className="flex flex-wrap items-center gap-x-6 gap-y-2 border-b px-5 py-3.5 pr-12">
          <div className="flex min-w-0 flex-col gap-1">
            <DialogTitle className="flex items-center gap-2 text-lg font-semibold">
              <PieChart className="size-4 text-primary" /> Space usage
            </DialogTitle>
            <DialogDescription className="flex min-w-0 items-center gap-1 font-mono text-xs">
              <button onClick={up} aria-label="Up one folder" className="rounded p-0.5 hover:bg-accent hover:text-foreground">
                <ArrowUp className="size-3.5" />
              </button>
              <span className="truncate text-foreground">{tree?.path ?? '…'}</span>
              {tree && (
                <>
                  <ChevronRight className="size-3 shrink-0" />
                  <span className="shrink-0 text-primary">{formatSize(tree.size)}</span>
                </>
              )}
            </DialogDescription>
          </div>
          <div className="ml-auto flex items-center gap-3 text-xs text-muted-foreground">
            {view === 'map' && data && (data.from_index ? 'From the live index, no rescan needed' : 'Scanned from disk (folder not in the index)')}
            <div role="group" aria-label="View" className="flex rounded-md border border-input p-0.5">
              {(['map', 'changes'] as const).map((v) => (
                <button
                  key={v}
                  aria-pressed={view === v}
                  onClick={() => set({ spaceFinderView: v })}
                  className={cn(
                    'h-7 rounded px-2.5 text-[13px]',
                    view === v ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent',
                  )}
                >
                  {v === 'map' ? 'Map' : 'What changed'}
                </button>
              ))}
            </div>
            <button
              onClick={() => void load(root, true)}
              disabled={loading}
              className="flex h-8 items-center gap-1.5 rounded-md border border-input bg-accent px-2.5 text-[13px] text-foreground hover:bg-[#2c2c29] disabled:opacity-50"
            >
              <RotateCw className={cn('size-3.5', loading && 'animate-spin')} /> Rescan
            </button>
          </div>
        </header>

        <div className="flex min-h-0 flex-1">
          <main className="flex min-w-0 flex-1 flex-col gap-2 p-4">
            {view === 'changes' && <ChangesView onOpen={openChange} />}
            {view === 'map' && error && <p className="m-auto text-sm text-muted-foreground">{error}</p>}
            {view === 'map' && !error && !tree && (
              <div className="m-auto flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="size-4 animate-spin" /> Measuring folders…
              </div>
            )}
            {view === 'map' && !error && tree && (
              <>
                {tree.size === 0 ? (
                  <p className="m-auto text-sm text-muted-foreground">This folder is empty.</p>
                ) : (
                  <Treemap root={tree} selected={selected ? keyOf(selected) : null} onSelect={setSelected} onDrill={drill} />
                )}
                <p className="m-0 text-xs text-dim">
                  Tile area is proportional to size. Click to inspect, double-click or{' '}
                  <kbd className="rounded border border-input px-1 font-mono">Enter</kbd> to drill in,{' '}
                  <kbd className="rounded border border-input px-1 font-mono">Backspace</kbd> to go up.
                </p>
              </>
            )}
          </main>

          {view === 'map' && (
          <aside className="flex w-80 shrink-0 flex-col gap-4 overflow-y-auto border-l bg-card p-4">
            {detail && (
              <div className="flex flex-col gap-1">
                <div className="text-[15px] font-semibold [overflow-wrap:anywhere]">{detail.name}</div>
                <div className="font-mono text-xs text-muted-foreground">
                  {formatSize(detail.size)}
                  {tree && detail !== tree && tree.size > 0 && ` · ${((detail.size / tree.size) * 100).toFixed(1)}% of ${tree.name}`}
                </div>
                <div className="mt-2 flex gap-2">
                  {detail !== tree && (
                    <button onClick={() => drill(detail)} className="h-8 rounded-md bg-primary px-3 text-[13px] font-semibold text-primary-foreground hover:bg-primary/90">
                      Drill in
                    </button>
                  )}
                  <button
                    onClick={() => openInExplorer(detail.path)}
                    className="flex h-8 items-center gap-1.5 rounded-md border border-input bg-accent px-3 text-[13px] hover:bg-[#2c2c29]"
                  >
                    <ExternalLink className="size-3.5" /> Open
                  </button>
                </div>
              </div>
            )}
            {selected && selected.kind !== 'dir' && (
              <p className="m-0 text-xs text-muted-foreground">
                {selected.kind === 'files'
                  ? `${formatSize(selected.size)} in files directly inside ${tree?.name}.`
                  : `${selected.count} smaller folders, ${formatSize(selected.size)} together.`}
              </p>
            )}
            <div>
              <h2 className="m-0 mb-2 text-[11px] font-normal tracking-[.08em] text-muted-foreground uppercase">Largest here</h2>
              <ol className="m-0 flex list-none flex-col gap-0.5 p-0">
                {ranking.slice(0, 14).map((t) => {
                  const pct = tree && tree.size ? (sizeOfTile(t) / tree.size) * 100 : 0
                  return (
                    <li key={keyOf(t)}>
                      <button
                        onClick={() => setSelected(t)}
                        onDoubleClick={() => t.kind === 'dir' && drill(t.node)}
                        className={cn(
                          'flex w-full flex-col gap-1 rounded-md px-2 py-1.5 text-left hover:bg-accent',
                          selected && keyOf(selected) === keyOf(t) && 'bg-accent',
                        )}
                      >
                        <span className="flex justify-between gap-2 text-xs">
                          <span className={cn('truncate', t.kind !== 'dir' && 'text-muted-foreground italic')}>{labelOf(t)}</span>
                          <span className="shrink-0 font-mono text-muted-foreground">{formatSize(sizeOfTile(t))}</span>
                        </span>
                        <span className="h-1 overflow-hidden rounded-sm bg-border">
                          <span className="block h-full bg-primary" style={{ width: `${Math.max(1, pct)}%` }} />
                        </span>
                      </button>
                    </li>
                  )
                })}
              </ol>
            </div>
          </aside>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
