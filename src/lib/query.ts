// Client-side view of the search syntax, for chips and facets. The Rust
// parser (src-tauri/src/search.rs) remains the authority on what a query means.

export const KNOWN_FILTERS = ['type', 'ext', 'size', 'modified', 'name', 'content', 'git', 'duplicate', 'here'] as const

export interface Token {
  raw: string
  /** Filter key (`ext`, `size`, …), or null for free text. */
  key: string | null
  value: string
  known: boolean
}

/** Split on spaces, keeping `"quoted strings"` (and `key:"quoted"`) whole. */
export function tokenize(query: string): Token[] {
  const out: Token[] = []
  const re = /(\S*?"[^"]*"?\S*|\S+)/g
  for (const m of query.matchAll(re)) {
    const raw = m[0]
    const colon = raw.indexOf(':')
    const looksLikeUrl = raw.slice(colon + 1).startsWith('//')
    if (colon > 0 && !raw.startsWith('"') && !looksLikeUrl && /^[a-z]+$/i.test(raw.slice(0, colon))) {
      const key = raw.slice(0, colon).toLowerCase()
      out.push({ raw, key, value: raw.slice(colon + 1), known: (KNOWN_FILTERS as readonly string[]).includes(key) })
    } else if (raw.startsWith('*.') && raw.length > 2) {
      out.push({ raw, key: 'ext', value: raw.slice(2), known: true })
    } else {
      out.push({ raw, key: null, value: raw, known: true })
    }
  }
  return out
}

export const join = (tokens: Token[]) => tokens.map((t) => t.raw).join(' ')

export function withoutToken(query: string, index: number): string {
  return join(tokenize(query).filter((_, i) => i !== index))
}

/** Replace every token of `keys` with `value` (or drop them when null). */
export function setFilter(query: string, keys: string | string[], value: string | null): string {
  const ks = Array.isArray(keys) ? keys : [keys]
  const kept = tokenize(query).filter((t) => !t.key || !ks.includes(t.key))
  const text = join(kept)
  return value ? (text ? `${text} ${value}` : value) : text
}

export function filterValue(query: string, key: string): string | null {
  const t = tokenize(query).find((t) => t.key === key)
  return t ? t.value : null
}

function distance(a: string, b: string): number {
  let prev = Array.from({ length: b.length + 1 }, (_, i) => i)
  for (let i = 0; i < a.length; i++) {
    const cur = [i + 1]
    for (let j = 0; j < b.length; j++) {
      cur.push(Math.min(prev[j] + (a[i] === b[j] ? 0 : 1), prev[j + 1] + 1, cur[j] + 1))
    }
    prev = cur
  }
  return prev[b.length]
}

/** Same rule as `suggest_filter` in search.rs. */
export function suggestFilter(key: string): string | null {
  let best: [number, string] | null = null
  for (const k of KNOWN_FILTERS) {
    const d = distance(key, k)
    if (d === 0) return null
    if ((d === 1 || (d === 2 && k.length >= 5)) && (!best || d < best[0])) best = [d, k]
  }
  return best?.[1] ?? null
}
