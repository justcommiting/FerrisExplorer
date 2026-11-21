import { describe, expect, it } from 'vitest'
import { TYPEAHEAD_MS, typeaheadStep, type TypeaheadState } from './typeahead'

const names = ['Apple', 'Banana', 'Blueberry', 'avocado', 'Cherry']
const fresh: TypeaheadState = { text: '', at: 0 }

describe('typeaheadStep', () => {
  it('jumps to the first name starting with the typed letter', () => {
    const r = typeaheadStep(fresh, 'b', 1000, names, -1)
    expect(r.index).toBe(1)
    expect(r.state).toEqual({ text: 'b', at: 1000 })
  })

  it('extends the search while the keys arrive within the pause limit', () => {
    const r = typeaheadStep({ text: 'b', at: 1000 }, 'l', 1500, names, 1)
    expect(r.index).toBe(2)
    expect(r.state.text).toBe('bl')
  })

  it('starts over after a longer pause', () => {
    const r = typeaheadStep({ text: 'b', at: 1000 }, 'c', 1000 + TYPEAHEAD_MS + 1, names, 1)
    expect(r.index).toBe(4)
    expect(r.state.text).toBe('c')
  })

  it('keeps the search at exactly the pause limit', () => {
    const r = typeaheadStep({ text: 'b', at: 1000 }, 'c', 1000 + TYPEAHEAD_MS, names, 1)
    expect(r.index).toBeNull()
    expect(r.state.text).toBe('bc')
  })

  it('cycles through names for a repeated letter, wrapping at the end', () => {
    const second = typeaheadStep({ text: 'a', at: 0 }, 'a', 100, names, 0)
    expect(second.index).toBe(3)
    const wrapped = typeaheadStep(second.state, 'a', 200, names, 3)
    expect(wrapped.index).toBe(0)
  })

  it('matches case-insensitively and returns null when nothing matches', () => {
    expect(typeaheadStep(fresh, 'A', 0, names, -1).index).toBe(0)
    const miss = typeaheadStep(fresh, 'z', 0, names, 2)
    expect(miss.index).toBeNull()
    expect(miss.state.text).toBe('z')
  })

  it('returns null for an empty list', () => {
    expect(typeaheadStep(fresh, 'a', 0, [], -1).index).toBeNull()
  })
})
