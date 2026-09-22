import type { SessionListEntry } from '../api/types'

export type SessionSortKey =
  | 'startWallMs'
  | 'durationS'
  | 'note'
  | 'mode'
  | 'samples'
  | 'dischargeWh'
  | 'coveragePct'
  | 'discontinuities'
  | 'markers'
  | 'bytes'
  | 'file'

export type SessionModeFilter = 'all' | 'ok' | 'incomplete' | 'recovered'

interface Options {
  query: string
  mode: SessionModeFilter
  sortKey: SessionSortKey
  asc: boolean
  /** Show only sessions with discharge coverage below 90%. */
  lowCoverage?: boolean
  /** Show only sessions with at least one clock discontinuity. */
  hasDiscontinuities?: boolean
}

/**
 * Presentation-only filtering and sorting. No forensic value is recomputed.
 * Unavailable numeric fields sort below present ones.
 */
export function filterAndSortSessions(
  entries: SessionListEntry[],
  { query, mode, sortKey, asc, lowCoverage, hasDiscontinuities }: Options,
): SessionListEntry[] {
  const filtered = entries.filter((s) => {
    if (mode === 'ok' && s.status !== 'ok') return false
    if (mode === 'incomplete' && s.mode !== 'incomplete') return false
    if (mode === 'recovered' && !s.recovered) return false
    if (lowCoverage && !(s.coveragePct !== null && s.coveragePct < 90)) return false
    if (hasDiscontinuities && !(s.discontinuities !== null && s.discontinuities > 0)) return false
    if (query) {
      const q = query.toLowerCase()
      return (
        s.file.toLowerCase().includes(q) ||
        s.note.toLowerCase().includes(q) ||
        s.path.toLowerCase().includes(q)
      )
    }
    return true
  })

  const dir = asc ? 1 : -1
  return [...filtered].sort((a, b) => {
    const pick = (s: SessionListEntry): number | null | string => {
      switch (sortKey) {
        case 'startWallMs':
          return s.startWallMs
        case 'durationS':
          return s.durationS
        case 'samples':
          return s.samples
        case 'dischargeWh':
          return s.dischargeWh
        case 'coveragePct':
          return s.coveragePct
        case 'discontinuities':
          return s.discontinuities
        case 'markers':
          return s.markers
        case 'bytes':
          return s.bytes
        case 'note':
          return s.note || s.file
        case 'mode':
          return s.mode
        case 'file':
          return s.file
      }
    }
    const av = pick(a)
    const bv = pick(b)
    if (typeof av === 'string' || typeof bv === 'string') {
      return String(av).localeCompare(String(bv)) * dir
    }
    // Absent numeric fields always sort last, regardless of direction.
    if (av === null && bv === null) return 0
    if (av === null) return 1
    if (bv === null) return -1
    return (av - bv) * dir
  })
}
