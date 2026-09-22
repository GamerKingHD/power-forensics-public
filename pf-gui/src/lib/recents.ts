// Lightweight recent-items and last-session state. Deliberately tiny: a few
// bounded lists in localStorage, no history subsystem.

const MAX_RECENTS = 8

function key(kind: string) {
  return `pf.recent.${kind}`
}

export function loadRecents<T>(kind: string): T[] {
  try {
    const raw = localStorage.getItem(key(kind))
    if (!raw) return []
    const parsed = JSON.parse(raw)
    return Array.isArray(parsed) ? (parsed as T[]) : []
  } catch {
    return []
  }
}

export function pushRecent<T>(kind: string, item: T, same: (a: T, b: T) => boolean): T[] {
  const list = loadRecents<T>(kind).filter((x) => !same(x, item))
  list.unshift(item)
  const trimmed = list.slice(0, MAX_RECENTS)
  try {
    localStorage.setItem(key(kind), JSON.stringify(trimmed))
  } catch {
    // A full or unavailable store must never break navigation.
  }
  return trimmed
}

export interface RestorableState {
  page?: string
  selectedSession?: string | null
  experimentId?: string | null
}

const STATE_KEY = 'pf.state'

export function loadState(): RestorableState {
  try {
    const raw = localStorage.getItem(STATE_KEY)
    if (!raw) return {}
    const parsed = JSON.parse(raw)
    return typeof parsed === 'object' && parsed !== null ? (parsed as RestorableState) : {}
  } catch {
    return {}
  }
}

export function saveState(state: RestorableState) {
  try {
    localStorage.setItem(STATE_KEY, JSON.stringify(state))
  } catch {
    // ignore
  }
}
