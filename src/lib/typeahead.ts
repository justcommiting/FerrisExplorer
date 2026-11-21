/** Type-ahead: typed letters jump to the next name that starts with them. Pure, so it can be tested. */

/** A pause this long starts a new search. */
export const TYPEAHEAD_MS = 800

export interface TypeaheadState {
  text: string
  at: number
}

/**
 * Advance type-ahead by one typed character. `names` are the visible names in
 * list order and `cur` is the cursor row (-1 for none). Returns the row to
 * select, or null when nothing matches and the selection should stay put.
 *
 * Typing a prefix finds the first match at or after the cursor. Repeating one
 * letter (a, a, a) cycles through the names that start with it.
 */
export function typeaheadStep(
  state: TypeaheadState,
  key: string,
  now: number,
  names: string[],
  cur: number,
): { state: TypeaheadState; index: number | null } {
  const k = key.toLowerCase()
  const text = now - state.at > TYPEAHEAD_MS ? k : state.text + k
  const next = { text, at: now }
  const repeated = [...text].every((c) => c === text[0])
  const prefix = repeated ? text[0] : text
  const start = repeated ? cur + 1 : Math.max(cur, 0)
  if (names.length === 0) return { state: next, index: null }
  for (let step = 0; step < names.length; step++) {
    const i = (start + step) % names.length
    if (names[i].toLowerCase().startsWith(prefix)) return { state: next, index: i }
  }
  return { state: next, index: null }
}
