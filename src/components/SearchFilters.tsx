import { useMemo } from 'react'
import { AlertTriangle, Search, Sparkles, X } from 'lucide-react'

import { extension } from '@/lib/format'
import { filterValue, setFilter, suggestFilter, tokenize, withoutToken } from '@/lib/query'
import { cn } from '@/lib/utils'
import { useExplorer, type Tab } from '@/store/explorer'

const DAY = 86_400

function rerun(query: string) {
  void useExplorer.getState().runSearch(query)
}

/** Chips for the active query, the "did you mean" fix, and the scope toggle. */
export function SearchHeader({ tab }: { tab: Tab }) {
  const query = tab.activeQuery ?? ''
  const tokens = useMemo(() => tokenize(query), [query])
  const unknown = tokens
    .map((t, i) => ({ t, i, near: t.key && !t.known ? suggestFilter(t.key) : null }))
    .filter((x) => x.t.key && !x.t.known)
  const hereOnly = tokens.some((t) => t.key === 'here')
  const hasText = tokens.some((t) => !t.key)
  const hasFilter = tokens.some((t) => t.key && t.known && t.key !== 'here')
  const wholeDisk = !hereOnly && !hasText && hasFilter
  // Errors the chips already explain are not repeated below them.
  const otherErrors = [...tab.queryErrors, ...(tab.searchError ? [tab.searchError] : [])].filter(
    (e) => !unknown.some((u) => e.includes(`'${u.t.key}:'`)),
  )

  return (
    <div className="flex shrink-0 flex-col gap-2 border-b bg-card px-3 py-2.5">
      <div className="flex flex-wrap items-center gap-1.5">
        <Search className="mr-0.5 size-3.5 text-muted-foreground" />
        {tokens.map((t, i) => (
          <span
            key={`${i}-${t.raw}`}
            className={cn(
              'group inline-flex h-6 items-center gap-1 rounded-md border pr-1 pl-2 font-mono text-xs',
              !t.key && 'border-input bg-accent text-foreground',
              t.key && t.known && 'border-primary/40 bg-primary/12 text-foreground',
              t.key && !t.known && 'border-destructive/60 bg-destructive/10 text-destructive line-through decoration-destructive/60',
            )}
          >
            {t.key && t.known ? (
              <>
                <span className="text-primary">{t.key}:</span>
                {t.value}
              </>
            ) : (
              t.raw
            )}
            <button
              aria-label={`Remove ${t.raw}`}
              onClick={() => rerun(withoutToken(query, i))}
              className="grid size-4 place-items-center rounded text-muted-foreground hover:bg-background hover:text-foreground"
            >
              <X className="size-3" />
            </button>
          </span>
        ))}

        <div role="group" aria-label="Scope" className="ml-auto flex overflow-hidden rounded-md border text-xs">
          <button
            aria-pressed={!hereOnly}
            onClick={() => hereOnly && rerun(setFilter(query, 'here', null))}
            title={wholeDisk ? 'Filter-only queries search the whole index' : 'Free text searches this folder and below'}
            className={cn('h-6 px-2.5', !hereOnly ? 'bg-accent text-foreground' : 'text-muted-foreground hover:bg-accent')}
          >
            {wholeDisk ? 'Whole disk' : 'This folder and below'}
          </button>
          <button
            aria-pressed={hereOnly}
            onClick={() => !hereOnly && rerun(setFilter(query, 'here', 'here:'))}
            className={cn('h-6 border-l px-2.5 font-mono', hereOnly ? 'bg-accent text-foreground' : 'text-muted-foreground hover:bg-accent')}
          >
            here:
          </button>
        </div>
      </div>

      {unknown.map(({ t, i, near }) => (
        <div key={`u-${i}`} role="alert" className="flex flex-wrap items-center gap-2 text-xs text-amber-300">
          <AlertTriangle className="size-3.5 shrink-0" />
          <span>
            Unknown filter <code className="rounded bg-background px-1 font-mono">{t.key}:</code> was not applied.
            {near && (
              <>
                {' '}Did you mean <code className="rounded bg-background px-1 font-mono">{near}:</code>?
              </>
            )}
          </span>
          {near && (
            <button
              onClick={() => {
                const fixed = tokenize(query).map((x, j) => (j === i ? `${near}:${x.value}` : x.raw)).join(' ')
                rerun(fixed)
              }}
              className="h-6 rounded-md border border-amber-300/40 px-2 font-mono text-amber-200 hover:bg-amber-300/10"
            >
              Fix to {near}:
            </button>
          )}
        </div>
      ))}

      {tab.interpretedAs && (
        <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Sparkles className="size-3 text-primary" /> Read as
          <code className="rounded bg-background px-1.5 py-0.5 font-mono text-[11px] text-foreground">{tab.interpretedAs}</code>
        </div>
      )}
      {otherErrors.map((err) => (
        <div key={err} className="flex items-center gap-1.5 text-xs text-amber-300">
          <AlertTriangle className="size-3 shrink-0" /> {err}
        </div>
      ))}
    </div>
  )
}

function Facet({ legend, children }: { legend: string; children: React.ReactNode }) {
  return (
    <fieldset className="m-0 flex flex-col gap-0.5 border-0 p-0">
      <legend className="mb-1 px-1 text-[11px] tracking-[.08em] text-muted-foreground uppercase">{legend}</legend>
      {children}
    </fieldset>
  )
}

function Option({
  type,
  checked,
  label,
  count,
  mono,
  onChange,
}: {
  type: 'checkbox' | 'radio'
  checked: boolean
  label: React.ReactNode
  count?: number
  mono?: boolean
  onChange: () => void
}) {
  return (
    <label className="flex h-7 cursor-default items-center gap-2 rounded-md px-1 hover:bg-accent">
      <input type={type} checked={checked} onChange={onChange} className="size-3.5 accent-[var(--primary)]" />
      <span className={cn('truncate', mono && 'font-mono text-xs')}>{label}</span>
      {count !== undefined && <span className="ml-auto font-mono text-[11px] text-muted-foreground">{count}</span>}
    </label>
  )
}

/** Facets that add or remove filters; counts come from the current results. */
export function RefinePanel({ tab }: { tab: Tab }) {
  const query = tab.activeQuery ?? ''
  const results = tab.results

  const exts = useMemo(() => {
    const m = new Map<string, number>()
    for (const n of results) {
      if (n.is_dir) continue
      const e = extension(n.name)
      if (e) m.set(e, (m.get(e) ?? 0) + 1)
    }
    return [...m.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6)
  }, [results])

  const now = Math.floor(Date.now() / 1000)
  const ages = useMemo(() => {
    const within = (days: number) => results.filter((n) => now - n.modified_at <= days * DAY).length
    return { today: within(1), week: within(7), month: within(30) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [results])

  const currentExt = (filterValue(query, 'ext') ?? filterValue(query, 'type'))?.replace(/^\./, '').toLowerCase() ?? null
  const currentMod = filterValue(query, 'modified')
  const currentGit = filterValue(query, 'git')
  const dupes = filterValue(query, 'duplicate') === 'true'
  const gitCount = (badges: string[]) => results.filter((n) => badges.includes(tab.git?.marks[n.path] ?? '')).length

  const extOptions = currentExt && !exts.some(([e]) => e === currentExt) ? [[currentExt, undefined] as const, ...exts] : exts

  return (
    <aside aria-label="Refine" className="flex w-52 shrink-0 flex-col gap-5 overflow-y-auto border-r bg-sidebar p-3">
      <Facet legend="Type">
        {extOptions.length === 0 && <span className="px-1 text-xs text-dim">No files yet</span>}
        {extOptions.map(([e, count]) => (
          <Option
            key={e}
            type="checkbox"
            mono
            checked={currentExt === e}
            label={`.${e}`}
            count={count}
            onChange={() => rerun(setFilter(query, ['ext', 'type'], currentExt === e ? null : `ext:${e}`))}
          />
        ))}
      </Facet>

      <Facet legend="Modified">
        {(
          [
            [null, 'Any time', undefined],
            ['today', 'Today', ages.today],
            ['<7d', 'Last 7 days', ages.week],
            ['<30d', 'Last 30 days', ages.month],
          ] as const
        ).map(([v, label, count]) => (
          <Option
            key={label}
            type="radio"
            checked={currentMod === v || (v === null && !currentMod)}
            label={label}
            count={count}
            onChange={() => rerun(setFilter(query, 'modified', v ? `modified:${v}` : null))}
          />
        ))}
      </Facet>

      {tab.git && (
        <Facet legend="Git state">
          {(
            [
              ['modified', 'M', 'modified', ['M', '+', 'D'], 'text-git-modified'],
              ['untracked', 'U', 'untracked', ['U'], 'text-git-untracked'],
            ] as const
          ).map(([v, badge, label, badges, color]) => (
            <Option
              key={v}
              type="checkbox"
              checked={currentGit === v}
              label={
                <>
                  <span className={cn('mr-1.5 font-mono text-xs', color)}>{badge}</span>
                  {label}
                </>
              }
              count={gitCount([...badges])}
              onChange={() => rerun(setFilter(query, 'git', currentGit === v ? null : `git:${v}`))}
            />
          ))}
        </Facet>
      )}

      <Facet legend="Other">
        <Option
          type="checkbox"
          checked={dupes}
          label="Duplicates only"
          onChange={() => rerun(setFilter(query, 'duplicate', dupes ? null : 'duplicate:true'))}
        />
      </Facet>

      <button
        onClick={() => useExplorer.getState().set({ prompt: { kind: 'saveSearch', initial: query } })}
        className="mt-auto h-8 rounded-md border border-input bg-accent text-[13px] hover:bg-[#2c2c29]"
      >
        Save this search
      </button>
      <button
        onClick={() => useExplorer.getState().set({ prompt: { kind: 'saveSmart', initial: query, query } })}
        disabled={!query.trim()}
        className="h-8 rounded-md border border-input bg-accent text-[13px] hover:bg-[#2c2c29] disabled:opacity-50"
      >
        Save as smart folder
      </button>
    </aside>
  )
}
