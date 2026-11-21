/** Vim-style list keys (opt-in). Pure state machine plus the binding table the help overlay shows. */

export type VimAction =
  | 'down'
  | 'up'
  | 'top'
  | 'bottom'
  | 'parent'
  | 'open'
  | 'filter'
  | 'nextMatch'
  | 'prevMatch'
  | 'copyPath'
  | 'trash'
  | 'paste'
  | 'repeat'

const SINGLE: Record<string, VimAction> = {
  j: 'down',
  k: 'up',
  h: 'parent',
  l: 'open',
  G: 'bottom',
  '/': 'filter',
  n: 'nextMatch',
  N: 'prevMatch',
  p: 'paste',
  '.': 'repeat',
}

/** Two-key chords. The first key is held in `pending` until the second arrives. */
const CHORD: Record<string, VimAction> = { gg: 'top', yy: 'copyPath', dd: 'trash' }

export interface VimStep {
  action: VimAction | null
  pending: string
}

/** Feed one key (already known to be unmodified) into the chord state. */
export function vimStep(pending: string, key: string): VimStep {
  if (pending) {
    const action = CHORD[pending + key]
    if (action) return { action, pending: '' }
    // A key that does not finish the chord cancels it and is read on its own.
    return vimStep('', key)
  }
  if (Object.keys(CHORD).some((c) => c[0] === key)) return { action: null, pending: key }
  return { action: SINGLE[key] ?? null, pending: '' }
}

/** Keys the help overlay lists when Vim-style keys are on. Ctrl+d and Ctrl+u are handled apart from `vimStep`. */
export const VIM_BINDINGS: { keys: string; does: string }[] = [
  { keys: 'j / k', does: 'Move down / up' },
  { keys: 'h', does: 'Go to parent folder' },
  { keys: 'l', does: 'Open' },
  { keys: 'gg / G', does: 'Top / bottom' },
  { keys: 'Ctrl+d / Ctrl+u', does: 'Half page down / up' },
  { keys: '/', does: 'Quick filter' },
  { keys: 'n / N', does: 'Next / previous filter match' },
  { keys: 'yy', does: 'Copy path' },
  { keys: 'dd', does: 'Move to Trash' },
  { keys: 'p', does: 'Paste' },
  { keys: '.', does: 'Repeat the last trash or paste' },
]
