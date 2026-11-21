// Undo and redo for the changes the user makes. Each change records the paths
// it touched, so the reverse can be asked of the backend, which refuses
// anything that would overwrite a file.
import { api, type CreatedItem, type RenameOutcome, type SetAside, type WrittenUndo } from '@/lib/api'
import { basename, parentOf } from '@/lib/format'

export type Change =
  | { kind: 'rename'; from: string; to: string }
  | { kind: 'createFolder'; path: string }
  /** Moved items as [from, to]. */
  | { kind: 'move'; pairs: [string, string][] }
  /** Copies (or duplicates) made; undo trashes them. Not redoable. */
  | { kind: 'copy'; created: string[] }
  /** Items moved to the Trash; undo restores them. */
  | { kind: 'trash'; paths: string[] }
  /** Renames done as one batch, as [from, to]. Swaps and cycles need the batch, not single moves. */
  | { kind: 'batchRename'; pairs: [string, string][] }
  /**
   * A folder sync: items written, old versions overwritten, and removed items moved
   * to the Trash. Undo trashes the written items that are still as the sync left
   * them, puts each overwritten old version back from its set-aside name, then
   * restores the removed items. Items that changed are left in place and stay on
   * the stack. Not redoable, like copies.
   */
  | { kind: 'sync'; created: string[]; written: CreatedItem[]; trashed: string[]; setAside: SetAside[] }

export interface UndoEntry {
  /** Past tense, without the verb's subject: "rename to “b.txt”". */
  label: string
  change: Change
}

/** Copies are not replayed: the copies may since have been trashed or changed. */
export function isRedoable(change: Change): boolean {
  return change.kind !== 'copy' && change.kind !== 'sync'
}

/** How many items a change touches. */
export function itemCount(change: Change): number {
  switch (change.kind) {
    case 'rename':
    case 'createFolder':
      return 1
    case 'move':
      return change.pairs.length
    case 'copy':
      return change.created.length
    case 'trash':
      return change.paths.length
    case 'batchRename':
      return change.pairs.length
    case 'sync':
      return change.created.length + change.trashed.length
  }
}

/**
 * A reversal or replay that got through some items and then stopped. Both
 * parts are in the change's original direction, so the caller can file each
 * one back on the stack it belongs to.
 */
export class PartialChangeError extends Error {
  /** The items that went through. */
  done: Change
  /** The items still to do. */
  rest: Change

  constructor(done: Change, rest: Change, reason: string, total: number) {
    super(`${total - itemCount(rest)} of ${total} done. ${reason}`)
    this.name = 'PartialChangeError'
    this.done = done
    this.rest = rest
  }
}

function failReason([path, reason]: [string, string]): string {
  return `${basename(path)}: ${reason}`
}

function swapPair([a, b]: [string, string]): [string, string] {
  return [b, a]
}

function failedAny(out: { failures: [string, string][] }) {
  if (out.failures.length) throw new Error(out.failures.map(failReason).join('\n'))
}

/** Stop partway. With nothing done there is nothing to file, so it is a plain error. */
function stop(total: number, done: Change | null, rest: Change, reason: string): never {
  if (!done) throw new Error(reason)
  throw new PartialChangeError(done, rest, reason, total)
}

function stopMove(change: Extract<Change, { kind: 'move' }>, done: [string, string][], failure: [string, string]): never {
  const rest = change.pairs.filter((p) => !done.some((d) => d[0] === p[0] && d[1] === p[1]))
  stop(
    change.pairs.length,
    done.length ? { kind: 'move', pairs: done } : null,
    { kind: 'move', pairs: rest },
    failReason(failure),
  )
}

/** A batch that failed. It was rolled back, except for any items listed as stranded. */
function batchStopped(out: RenameOutcome): never {
  const lines = out.failure ? [failReason(out.failure)] : []
  for (const [now, was] of out.stranded) lines.push(`${basename(was)} is now at ${basename(now)}`)
  throw new Error(lines.join('\n'))
}

function syncChange(
  created: string[],
  written: CreatedItem[],
  trashed: string[],
  setAside: SetAside[],
): Change {
  return { kind: 'sync', created, written, trashed, setAside }
}

function pathChange(kind: 'trash' | 'copy', paths: string[]): Change {
  return kind === 'trash' ? { kind: 'trash', paths } : { kind: 'copy', created: paths }
}

function stopPaths(kind: 'trash' | 'copy', total: number, done: string[], rest: string[], reason: string): never {
  stop(total, done.length ? pathChange(kind, done) : null, pathChange(kind, rest), reason)
}

/**
 * Put the change back the way it was. Throws with the reason when it cannot.
 * When some items went through first, throws a `PartialChangeError` whose
 * `rest` is what is still to undo.
 */
export async function reverseChange(change: Change): Promise<void> {
  switch (change.kind) {
    case 'rename':
      await api.renamePath(change.to, basename(change.from))
      return
    case 'createFolder': {
      // A folder that has gained contents is no longer ours to remove. The
      // backend checks the folder directly, so hidden files count too.
      if (!(await api.dirIsEmpty(change.path))) throw new Error(`${basename(change.path)} is not empty`)
      failedAny(await api.trashPaths([change.path]))
      return
    }
    case 'move': {
      // Undo runs each pair backwards, so the outcome lists pairs backwards too.
      const out = await api.moveExact(change.pairs.map(swapPair))
      if (out.failure) stopMove(change, out.done.map(swapPair), out.failure)
      return
    }
    case 'copy': {
      const out = await api.trashPaths(change.created)
      if (out.failures.length) {
        const failed = out.failures.map(([p]) => p)
        stopPaths('copy', change.created.length, out.done, failed, out.failures.map(failReason).join('\n'))
      }
      return
    }
    case 'trash': {
      const out = await api.restoreFromTrash(change.paths)
      if (out.failure) {
        const rest = change.paths.filter((p) => !out.done.includes(p))
        stopPaths('trash', change.paths.length, out.done, rest, failReason(out.failure))
      }
      return
    }
    case 'batchRename': {
      const out = await api.renameBatch(change.pairs.map(([from, to]): [string, string] => [to, basename(from)]))
      if (out.failure) batchStopped(out)
      return
    }
    case 'sync': {
      const total = itemCount(change)
      // The new versions go first, so their places are free for the old versions.
      // An old version comes back only once its new version is in the Trash. An
      // item that changed since the sync is left in place and stays on the stack.
      const out: WrittenUndo =
        change.written.length || change.setAside.length
          ? await api.trashUnchangedCreated(change.written, change.setAside)
          : { done: [], restored: [], failures: [], skipped: [] }
      const trashedNew = new Set(out.done)
      const putBack = new Set(out.restored)
      const stuck = [...out.failures, ...out.skipped]
      if (stuck.length) {
        const gotThrough = trashedNew.size > 0 || putBack.size > 0
        const done = gotThrough
          ? syncChange(
              change.created.filter((p) => trashedNew.has(p)),
              change.written.filter((w) => trashedNew.has(w.path)),
              [],
              change.setAside.filter((s) => putBack.has(s.path)),
            )
          : null
        const rest = syncChange(
          change.created.filter((p) => !trashedNew.has(p)),
          change.written.filter((w) => !trashedNew.has(w.path)),
          change.trashed,
          change.setAside.filter((s) => !putBack.has(s.path)),
        )
        stop(total, done, rest, stuck.map(failReason).join('\n'))
      }
      if (change.trashed.length) {
        const back = await api.restoreFromTrash(change.trashed)
        if (back.failure) {
          const left = change.trashed.filter((p) => !back.done.includes(p))
          const someDone = change.written.length > 0 || change.setAside.length > 0 || back.done.length > 0
          const done = someDone ? syncChange(change.created, change.written, back.done, change.setAside) : null
          stop(total, done, syncChange([], [], left, []), failReason(back.failure))
        }
      }
      return
    }
  }
}

/** Do the change again after it was undone. Only call when `isRedoable`. */
export async function replayChange(change: Change): Promise<void> {
  switch (change.kind) {
    case 'rename':
      await api.renamePath(change.from, basename(change.to))
      return
    case 'createFolder': {
      const parent = parentOf(change.path)
      if (!parent) throw new Error('no parent folder')
      await api.createFolder(parent, basename(change.path))
      return
    }
    case 'move': {
      const out = await api.moveExact(change.pairs)
      if (out.failure) stopMove(change, out.done, out.failure)
      return
    }
    case 'trash': {
      const out = await api.trashPaths(change.paths)
      if (out.failures.length) {
        const failed = out.failures.map(([p]) => p)
        stopPaths('trash', change.paths.length, out.done, failed, out.failures.map(failReason).join('\n'))
      }
      return
    }
    case 'batchRename': {
      const out = await api.renameBatch(change.pairs.map(([from, to]): [string, string] => [from, basename(to)]))
      if (out.failure) batchStopped(out)
      return
    }
    case 'copy':
    case 'sync':
      return
  }
}
