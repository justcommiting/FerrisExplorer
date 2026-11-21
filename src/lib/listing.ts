// Bookkeeping for streamed folder listings. A tab's listing arrives as
// `listing-chunk` events; anything tagged with a request other than the tab's
// current one is stale and dropped.

import { newRequestId, type ArchiveLocation, type VNode } from '@/lib/api'

/** A request id, unique across windows (see `newRequestId`). */
export function nextListingRequest(): number {
  return newRequestId()
}

export interface ActiveListing {
  request: number
  dir: string
  /** Batches received so far. The first one replaces the tab's rows. */
  batches: number
  /**
   * Resolves the `refresh` promise waiting for this listing. `completed` is
   * false when the listing was superseded or abandoned, so callers skip any
   * follow-up that assumes this folder is the one now shown.
   */
  finish: (completed: boolean) => void
  /** The archive check started with the listing, so it runs in parallel. */
  archive: Promise<ArchiveLocation | null>
}

/** The listing each tab is waiting on, keyed by tab id. */
export const activeListings = new Map<number, ActiveListing>()

/**
 * A short name for the disk or share `path` lives on, for "Waiting for …".
 * Drive letters and UNC shares are named; macOS volumes and mounted network
 * shares on Linux use their folder name. Anything else says "this location".
 */
export function volumeLabel(path: string): string {
  const drive = /^[A-Za-z]:/.exec(path)
  if (drive) return drive[0]
  if (path.startsWith('\\\\') || path.startsWith('//')) {
    const parts = path.split(/[\\/]+/).filter(Boolean)
    if (parts.length >= 2) return `\\\\${parts[0]}\\${parts[1]}`
  }
  const volume = /^\/(?:Volumes|mnt|media)\/([^/]+)/.exec(path)
  if (volume) return volume[1]
  return 'this location'
}

/** The sizes worth asking for after a listing: the folder itself, and every subfolder when sorting by size. */
export function sizeRequests(
  dir: string,
  items: VNode[],
  sortsBySize: boolean,
  known: Record<string, unknown>,
  indexed: boolean,
): { visible: string[]; background: string[] } {
  // Known sizes and sizes already being measured are not asked for again; a failed one is retried.
  const unknown = (p: string) => typeof known[p] !== 'number' && known[p] !== 'pending'
  const visible = (indexed ? [dir] : []).filter(unknown)
  const background = sortsBySize ? items.filter((n) => n.is_dir && unknown(n.path)).map((n) => n.path) : []
  return { visible, background }
}
