/**
 * Quick filter: case-insensitive, and the query's characters must appear in
 * order in the name (so "ferexp" finds "FerrisExplorer"). Substring matches
 * are a special case of this.
 */
export function matchesQuickFilter(name: string, query: string): boolean {
  const q = query.trim().toLowerCase()
  if (!q) return true
  const n = name.toLowerCase()
  if (n.includes(q)) return true
  let at = 0
  for (const ch of q) {
    at = n.indexOf(ch, at)
    if (at === -1) return false
    at += 1
  }
  return true
}
