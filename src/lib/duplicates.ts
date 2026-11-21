// Selection rules for the duplicate finder. A selection is the set of paths
// to move to the Trash. Every rule keeps exactly one copy per set, and
// `allowedSelection` is the check the store runs before accepting a change.
import type { DupGroup, DupMember, DupScan } from '@/lib/api'
import { parentOf } from '@/lib/format'

export type KeepRule =
  | { kind: 'newest' }
  | { kind: 'oldest' }
  | { kind: 'shortest' }
  | { kind: 'folder'; folder: string }

/** The copy a rule keeps, or null if the rule names none in this set. Ties go to the path that sorts first. */
export function keeperOf(group: DupGroup, rule: KeepRule): DupMember | null {
  const byPath = (a: DupMember, b: DupMember) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0)
  const members = group.members
  if (rule.kind === 'folder') {
    return members.filter((m) => parentOf(m.path) === rule.folder).sort(byPath)[0] ?? null
  }
  const score: Record<Exclude<KeepRule['kind'], 'folder'>, (m: DupMember) => number> = {
    newest: (m) => -m.modified_at,
    oldest: (m) => m.modified_at,
    shortest: (m) => m.path.length,
  }
  const key = score[rule.kind]
  return [...members].sort((a, b) => key(a) - key(b) || byPath(a, b))[0] ?? null
}

/** The paths a rule would trash in one set: every copy but the keeper. Empty when the rule keeps nothing. */
export function trashFor(group: DupGroup, rule: KeepRule): string[] {
  const keeper = keeperOf(group, rule)
  if (!keeper) return []
  return group.members.filter((m) => m.path !== keeper.path).map((m) => m.path)
}

/** Replace the selection for every set with what `rule` picks. */
export function selectionForScan(scan: DupScan, rule: KeepRule): Set<string> {
  return new Set(scan.groups.flatMap((g) => trashFor(g, rule)))
}

/** Replace the selection inside one set, leaving the other sets alone. */
export function withGroupSelection(selected: Set<string>, group: DupGroup, next: string[]): Set<string> {
  const out = new Set(selected)
  for (const m of group.members) out.delete(m.path)
  for (const p of next) out.add(p)
  return out
}

/** Whether `selected` leaves at least one copy in every set. */
export function allowedSelection(scan: DupScan, selected: Set<string>): boolean {
  return scan.groups.every((g) => g.members.some((m) => !selected.has(m.path)))
}

/** The sets that `selected` would leave with no copy, for the error message. */
export function fullySelectedSets(scan: DupScan, selected: Set<string>): DupGroup[] {
  return scan.groups.filter((g) => g.members.every((m) => selected.has(m.path)))
}

/** Count and total size of the selected copies that are in the scan. */
export function selectionTotals(scan: DupScan | null, selected: Set<string>): { count: number; bytes: number } {
  let count = 0
  let bytes = 0
  for (const g of scan?.groups ?? []) {
    for (const m of g.members) {
      if (selected.has(m.path)) {
        count++
        bytes += m.size
      }
    }
  }
  return { count, bytes }
}

/** Folders that hold copies, most common first, for the "keep the one in" picker. */
export function folderChoices(groups: DupGroup[]): string[] {
  const counts = new Map<string, number>()
  for (const g of groups) {
    for (const m of g.members) {
      const folder = parentOf(m.path) ?? ''
      counts.set(folder, (counts.get(folder) ?? 0) + 1)
    }
  }
  return [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([f]) => f)
}
