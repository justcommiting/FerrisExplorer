import { describe, expect, it } from 'vitest'
import type { VNode } from './api'
import { extension, formatFullDate, formatSize, formatTimestamp, kindLabel } from './format'

const NOW_SECS = 1_700_000_000
const NOW_MS = NOW_SECS * 1000
const DAY = 86_400

function node(name: string, isDir = false): VNode {
  return {
    name,
    path: `/x/${name}`,
    size: 0,
    is_dir: isDir,
    is_symlink: false,
    modified_at: 0,
    created_at: 0,
    readonly: false,
    hidden: false,
    mime_type: '',
  }
}

describe('formatSize', () => {
  it('prints bytes without a decimal below 1 KB', () => {
    expect(formatSize(0)).toBe('0 B')
    expect(formatSize(1)).toBe('1 B')
    expect(formatSize(1023)).toBe('1023 B')
  })

  it('uses binary units with one decimal place', () => {
    expect(formatSize(1024)).toBe('1.0 KB')
    expect(formatSize(1536)).toBe('1.5 KB')
    expect(formatSize(1024 * 1024)).toBe('1.0 MB')
    expect(formatSize(2 * 1024 ** 3)).toBe('2.0 GB')
  })

  it('stops at TB instead of inventing a larger unit', () => {
    expect(formatSize(3 * 1024 ** 5)).toBe('3072.0 TB')
  })
})

describe('formatTimestamp', () => {
  it('shows a dash for a missing timestamp', () => {
    expect(formatTimestamp(0, NOW_MS)).toBe('--')
  })

  it('uses relative labels for the last week', () => {
    expect(formatTimestamp(NOW_SECS, NOW_MS)).toBe('Today')
    expect(formatTimestamp(NOW_SECS - DAY, NOW_MS)).toBe('Yesterday')
    expect(formatTimestamp(NOW_SECS - 2 * DAY, NOW_MS)).toBe('2 days ago')
    expect(formatTimestamp(NOW_SECS - 6 * DAY, NOW_MS)).toBe('6 days ago')
  })

  it('treats a future timestamp as today', () => {
    expect(formatTimestamp(NOW_SECS + 500, NOW_MS)).toBe('Today')
  })

  it('falls back to a locale date from a week ago', () => {
    const old = NOW_SECS - 30 * DAY
    expect(formatTimestamp(old, NOW_MS)).toBe(new Date(old * 1000).toLocaleDateString())
  })
})

describe('formatFullDate', () => {
  it('says Unknown for a missing timestamp', () => {
    expect(formatFullDate(0)).toBe('Unknown')
  })
})

describe('extension and kindLabel', () => {
  it('lowercases the extension and ignores dotfiles and trailing dots', () => {
    expect(extension('Photo.JPG')).toBe('jpg')
    expect(extension('archive.tar.gz')).toBe('gz')
    expect(extension('.gitignore')).toBe('')
    expect(extension('Makefile')).toBe('')
    expect(extension('trailing.')).toBe('')
  })

  it('labels folders, known kinds, grouped kinds and unknown extensions', () => {
    expect(kindLabel(node('src', true))).toBe('Folder')
    expect(kindLabel(node('README'))).toBe('File')
    expect(kindLabel(node('lib.rs'))).toBe('Rust Source')
    expect(kindLabel(node('shot.PNG'))).toBe('Image')
    expect(kindLabel(node('data.parquet'))).toBe('PARQUET File')
  })
})
