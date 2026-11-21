import { describe, expect, it } from 'vitest'
import {
  applyCase,
  DEFAULT_OPTIONS,
  expandTemplate,
  isWindowsReserved,
  nameError,
  newNameFor,
  optionsError,
  planRenames,
  replaceText,
  splitName,
  templateError,
  type Item,
  type RenameOptions,
} from './bulkRename'

const POSIX = { windows: false, caseInsensitive: false }
const WINDOWS = { windows: true, caseInsensitive: true }

function item(name: string, parentName = 'Photos', modified = 0): Item {
  const parent = `/home/user/${parentName}/`
  return { path: parent + name, name, parent, parentName, modified }
}

const opts = (over: Partial<RenameOptions>): RenameOptions => ({ ...DEFAULT_OPTIONS, ...over })

describe('templates', () => {
  it('rejects an empty template and unknown tokens', () => {
    expect(templateError('  ')).toBe('The template is empty')
    expect(templateError('{foo}')).toBe('Unknown token {foo}')
    expect(templateError('{n:1234}')).toBe('Unknown token {n:1234}')
    expect(templateError('a}b')).toBe('Braces must form a token such as {name}')
    expect(templateError('{name}-{n:3}')).toBeNull()
  })

  it('keeps the extension case and drops the dot when there is no extension', () => {
    expect(expandTemplate('{name}.{ext}', item('photo.JPG'), 0, DEFAULT_OPTIONS)).toBe('photo.JPG')
    expect(expandTemplate('{name}.{ext}', item('README'), 0, DEFAULT_OPTIONS)).toBe('README')
  })

  it('numbers items from start and step, padded to the counter width', () => {
    const o = opts({ start: 1, step: 1 })
    expect(expandTemplate('{parent}-{n:3}', item('a.txt', 'Trip'), 4, o)).toBe('Trip-005')
    expect(expandTemplate('{n}', item('a.txt'), 2, opts({ start: 10, step: 5 }))).toBe('20')
  })

  it('fills {date} from the modified time (UTC noon-safe value)', () => {
    // 2024-01-02 06:00 UTC, which is the same calendar day in most time zones.
    expect(expandTemplate('{date}', item('a.txt', 'P', 1704175200), 0, DEFAULT_OPTIONS)).toBe('2024-01-02')
  })

  it('keeps unicode names intact', () => {
    expect(expandTemplate('{name}', item('日本語.txt'), 0, DEFAULT_OPTIONS)).toBe('日本語')
    expect(splitName('café.tar.gz')).toEqual({ stem: 'café.tar', ext: 'gz', suffix: '.gz' })
  })

  it('treats a leading dot as part of the name', () => {
    expect(splitName('.bashrc')).toEqual({ stem: '.bashrc', ext: '', suffix: '' })
  })
})

describe('find and replace', () => {
  it('matches plain text literally, not as a regex', () => {
    expect(replaceText('a.b.c', opts({ find: '.', replacement: '-' }))).toBe('a-b-c')
  })

  it('is case-insensitive unless asked otherwise', () => {
    expect(replaceText('Foo foo', opts({ find: 'foo', replacement: 'bar' }))).toBe('bar bar')
    expect(replaceText('Foo foo', opts({ find: 'foo', replacement: 'bar', caseSensitive: true }))).toBe('Foo bar')
  })

  it('keeps a $ in a plain replacement as written', () => {
    expect(replaceText('x5', opts({ find: '5', replacement: '$1' }))).toBe('x$1')
  })

  it('supports capture groups in regex mode', () => {
    expect(replaceText('img_001', opts({ find: '(\\d+)', replacement: '[$1]', regex: true }))).toBe('img_[001]')
  })

  it('reports an invalid regex as an options error', () => {
    expect(optionsError(opts({ mode: 'replace', find: '(', regex: true }))).toMatch(/^Not a valid pattern:/)
  })
})

describe('case', () => {
  it('title-cases words and leaves apostrophes alone', () => {
    expect(applyCase('hello wORLD', 'title')).toBe('Hello World')
    expect(applyCase("don't stop", 'title')).toBe("Don't Stop")
  })

  it('uses JavaScript upper-casing for unicode', () => {
    expect(applyCase('straße', 'upper')).toBe('STRASSE')
  })

  it('lowercases only the extension in title mode', () => {
    expect(newNameFor(item('my photo.JPG'), 0, opts({ caseMode: 'title', mode: 'replace', find: 'zzz' }))).toBe(
      'My Photo.jpg',
    )
  })
})

describe('name validation', () => {
  it('rejects separators and empty names on every platform', () => {
    expect(nameError('a/b', POSIX)).toBe('Names cannot contain / or \\')
    expect(nameError('  ', POSIX)).toBe('Name is empty')
    expect(nameError('..', POSIX)).toBe('".." is not a valid name')
  })

  it('applies the stricter Windows rules only on Windows', () => {
    expect(nameError('a:b', POSIX)).toBeNull()
    expect(nameError('a:b', WINDOWS)).toMatch(/^Contains a character Windows does not allow/)
    expect(nameError('name.', WINDOWS)).toBe('Windows does not allow a name to end with a dot or a space')
  })

  it('recognises reserved device names, including with an extension', () => {
    expect(isWindowsReserved('nul.txt')).toBe(true)
    expect(isWindowsReserved('lpt1 ')).toBe(true)
    expect(isWindowsReserved('COM10')).toBe(false)
    expect(isWindowsReserved('console')).toBe(false)
    expect(nameError('CON.txt', WINDOWS)).toBe('"CON.txt" is a reserved device name on Windows')
  })
})

describe('planRenames', () => {
  it('flags two items that would get the same name', () => {
    const plan = planRenames(
      [item('a.txt'), item('b.txt')],
      opts({ template: 'same.{ext}' }),
      new Map(),
      POSIX,
    )
    expect(plan.error).toBeNull()
    expect(plan.rows.map((r) => r.error)).toEqual(['Another item gets this name', 'Another item gets this name'])
  })

  it('flags a new name that would overwrite a file outside the selection', () => {
    const listed = new Map([['/home/user/Photos/', new Set(['a.txt', 'taken.txt'])]])
    const plan = planRenames(
      [item('a.txt')],
      opts({ mode: 'replace', find: 'a', replacement: 'taken' }),
      listed,
      POSIX,
    )
    expect(plan.rows[0].newName).toBe('taken.txt')
    expect(plan.rows[0].error).toBe('A file with this name already exists')
  })

  it('marks unchanged names and leaves a valid rename without an error', () => {
    const plan = planRenames([item('a.txt'), item('b.txt')], opts({ template: '{name}.{ext}' }), new Map(), POSIX)
    expect(plan.rows.every((r) => r.unchanged && r.error === null)).toBe(true)

    const renamed = planRenames([item('a.txt')], opts({ template: 'new-{n}.{ext}' }), new Map(), POSIX)
    expect(renamed.rows[0]).toMatchObject({ newName: 'new-1.txt', unchanged: false, error: null })
  })

  it('returns no rows when the options are invalid as a whole', () => {
    const plan = planRenames([item('a.txt')], opts({ template: '' }), new Map(), POSIX)
    expect(plan).toEqual({ error: 'The template is empty', rows: [] })
  })
})
