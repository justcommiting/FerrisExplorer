import { describe, expect, it } from 'vitest'
import { filterValue, setFilter, suggestFilter, tokenize, withoutToken } from './query'

describe('tokenize', () => {
  it('returns no tokens for an empty query', () => {
    expect(tokenize('')).toEqual([])
    expect(tokenize('   ')).toEqual([])
  })

  it('splits known filters from free text', () => {
    expect(tokenize('ext:rs main')).toEqual([
      { raw: 'ext:rs', key: 'ext', value: 'rs', known: true },
      { raw: 'main', key: null, value: 'main', known: true },
    ])
  })

  it('keeps quoted values with spaces whole', () => {
    expect(tokenize('name:"my file.txt" x')).toEqual([
      { raw: 'name:"my file.txt"', key: 'name', value: '"my file.txt"', known: true },
      { raw: 'x', key: null, value: 'x', known: true },
    ])
  })

  it('lowercases keys and marks unknown ones', () => {
    expect(tokenize('EXT:rs')[0]).toMatchObject({ key: 'ext', value: 'rs', known: true })
    expect(tokenize('colour:red')[0]).toMatchObject({ key: 'colour', known: false })
  })

  it('reads *.ext as an extension filter', () => {
    expect(tokenize('*.md')[0]).toMatchObject({ key: 'ext', value: 'md' })
  })

  it('treats a URL after the colon as free text', () => {
    expect(tokenize('https://example.com')[0]).toMatchObject({ key: null, value: 'https://example.com' })
  })
})

describe('query edits', () => {
  it('replaces every token of a key and appends the new one', () => {
    expect(setFilter('ext:rs foo ext:ts', 'ext', 'ext:md')).toBe('foo ext:md')
  })

  it('drops the key when the value is null', () => {
    expect(setFilter('ext:rs', 'ext', null)).toBe('')
    expect(setFilter('a size:>1mb b', ['size', 'modified'], null)).toBe('a b')
  })

  it('removes one token by index', () => {
    expect(withoutToken('size:>1mb foo bar', 1)).toBe('size:>1mb bar')
  })

  it('reads the value of the first matching filter', () => {
    expect(filterValue('ext:rs size:>1mb', 'size')).toBe('>1mb')
    expect(filterValue('foo', 'ext')).toBeNull()
  })
})

describe('suggestFilter', () => {
  it('suggests the nearest known filter for a typo', () => {
    expect(suggestFilter('modifed')).toBe('modified')
    expect(suggestFilter('tyoe')).toBe('type')
  })

  it('suggests nothing for a known or distant key', () => {
    expect(suggestFilter('ext')).toBeNull()
    expect(suggestFilter('colour')).toBeNull()
  })
})
