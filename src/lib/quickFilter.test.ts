import { describe, expect, it } from 'vitest'
import { matchesQuickFilter } from './quickFilter'

describe('matchesQuickFilter', () => {
  it('matches everything for an empty or blank query', () => {
    expect(matchesQuickFilter('anything.txt', '')).toBe(true)
    expect(matchesQuickFilter('anything.txt', '   ')).toBe(true)
  })

  it('ignores case and surrounding spaces', () => {
    expect(matchesQuickFilter('FerrisExplorer', '  FERRIS  ')).toBe(true)
  })

  it('matches the query characters in order, not contiguously', () => {
    expect(matchesQuickFilter('FerrisExplorer', 'ferexp')).toBe(true)
    expect(matchesQuickFilter('ferris', 'sf')).toBe(false)
  })

  it('rejects names missing a query character', () => {
    expect(matchesQuickFilter('notes.md', 'xyz')).toBe(false)
  })

  it('compares accented letters as written', () => {
    expect(matchesQuickFilter('Résumé.pdf', 'rsm')).toBe(true)
    expect(matchesQuickFilter('Résumé.pdf', 'é')).toBe(true)
  })

  it('handles long names', () => {
    const name = 'a'.repeat(10_000)
    expect(matchesQuickFilter(name, 'a'.repeat(10_000))).toBe(true)
    expect(matchesQuickFilter(name, 'a'.repeat(10_001))).toBe(false)
  })
})
