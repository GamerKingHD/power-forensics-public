import { describe, expect, it, beforeEach } from 'vitest'
import { loadRecents, pushRecent, loadState, saveState } from './recents'

// jsdom in this environment does not expose window.localStorage, so provide a
// minimal in-memory implementation for the storage round-trip tests.
function memoryStorage(): Storage {
  const map = new Map<string, string>()
  return {
    get length() {
      return map.size
    },
    clear: () => map.clear(),
    getItem: (k) => (map.has(k) ? (map.get(k) as string) : null),
    key: (i) => Array.from(map.keys())[i] ?? null,
    removeItem: (k) => void map.delete(k),
    setItem: (k, v) => void map.set(k, v),
  }
}

if (!(globalThis as { localStorage?: Storage }).localStorage) {
  ;(globalThis as unknown as { localStorage: Storage }).localStorage = memoryStorage()
}

describe('recents', () => {
  beforeEach(() => {
    localStorage.clear()
  })

  it('deduplicates and bounds the recent list', () => {
    for (let i = 0; i < 12; i++) pushRecent<string>('sessions', `s${i}.jsonl`, (a, b) => a === b)
    const list = loadRecents<string>('sessions')
    expect(list.length).toBe(8)
    expect(list[0]).toBe('s11.jsonl')
    // Re-pushing an existing item moves it to the front without duplicating.
    pushRecent<string>('sessions', 's5.jsonl', (a, b) => a === b)
    const again = loadRecents<string>('sessions')
    expect(again[0]).toBe('s5.jsonl')
    expect(again.filter((x) => x === 's5.jsonl').length).toBe(1)
  })

  it('restores and persists last state', () => {
    saveState({ page: 'calibration', selectedSession: 'a.jsonl' })
    expect(loadState()).toEqual({ page: 'calibration', selectedSession: 'a.jsonl' })
  })

  it('survives corrupt storage', () => {
    localStorage.setItem('pf.recent.sessions', '{not json')
    localStorage.setItem('pf.state', 'broken')
    expect(loadRecents<string>('sessions')).toEqual([])
    expect(loadState()).toEqual({})
  })
})
