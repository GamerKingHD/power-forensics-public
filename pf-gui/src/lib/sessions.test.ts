import { describe, expect, it } from 'vitest'
import type { SessionListEntry } from '../api/types'
import { filterAndSortSessions } from './sessions'

function entry(overrides: Partial<SessionListEntry>): SessionListEntry {
  return {
    file: 'pf-session-1.jsonl',
    path: 'sessions/pf-session-1.jsonl',
    label: 'pf-session-1.jsonl',
    note: '',
    startWallMs: 1000,
    endWallMs: 2000,
    durationS: 1,
    status: 'ok',
    recovered: false,
    mode: 'recorded',
    samples: 1,
    intervalMs: 1000,
    dischargeMedianW: null,
    dischargeWh: null,
    chargeWh: null,
    cpuMedianPct: null,
    coveragePct: null,
    discontinuities: null,
    markers: 0,
    bytes: 100,
    mtimeS: null,
    ...overrides,
  }
}

const entries: SessionListEntry[] = [
  entry({ file: 'a.jsonl', note: 'work test', startWallMs: 3000, dischargeWh: 5.5, coveragePct: 98.7 }),
  entry({ file: 'b.jsonl', note: 'torn', startWallMs: 1000, status: 'incomplete', mode: 'incomplete' }),
  entry({
    file: 'c.jsonl',
    note: 'recovered run',
    startWallMs: 2000,
    recovered: true,
    mode: 'recovered',
    dischargeWh: 1.0,
  }),
]

describe('filterAndSortSessions', () => {
  it('filters by status and quality mode', () => {
    const torn = filterAndSortSessions(entries, {
      query: '',
      mode: 'incomplete',
      sortKey: 'startWallMs',
      asc: false,
    })
    expect(torn.map((e) => e.file)).toEqual(['b.jsonl'])
    const recovered = filterAndSortSessions(entries, {
      query: '',
      mode: 'recovered',
      sortKey: 'file',
      asc: true,
    })
    expect(recovered.map((e) => e.file)).toEqual(['c.jsonl'])
  })

  it('searches file, label and path', () => {
    const hits = filterAndSortSessions(entries, {
      query: 'WORK',
      mode: 'all',
      sortKey: 'file',
      asc: true,
    })
    expect(hits.map((e) => e.file)).toEqual(['a.jsonl'])
  })

  it('sorts numerically and keeps absent values last', () => {
    const sorted = filterAndSortSessions(entries, {
      query: '',
      mode: 'all',
      sortKey: 'dischargeWh',
      asc: false,
    })
    // 5.5, 1.0, then the absent b.jsonl.
    expect(sorted.map((e) => e.file)).toEqual(['a.jsonl', 'c.jsonl', 'b.jsonl'])
  })

  it('sorts by start time descending by default', () => {
    const sorted = filterAndSortSessions(entries, {
      query: '',
      mode: 'all',
      sortKey: 'startWallMs',
      asc: false,
    })
    expect(sorted.map((e) => e.startWallMs)).toEqual([3000, 2000, 1000])
  })

  it('filters by low coverage and discontinuities', () => {
    const withQuality = [
      entry({ file: 'good.jsonl', coveragePct: 99.5, discontinuities: 0 }),
      entry({ file: 'low.jsonl', coveragePct: 62.0, discontinuities: 0 }),
      entry({ file: 'disc.jsonl', coveragePct: 95.0, discontinuities: 3 }),
    ]
    const low = filterAndSortSessions(withQuality, {
      query: '',
      mode: 'all',
      sortKey: 'file',
      asc: true,
      lowCoverage: true,
    })
    expect(low.map((e) => e.file)).toEqual(['low.jsonl'])
    const disc = filterAndSortSessions(withQuality, {
      query: '',
      mode: 'all',
      sortKey: 'file',
      asc: true,
      hasDiscontinuities: true,
    })
    expect(disc.map((e) => e.file)).toEqual(['disc.jsonl'])
  })
})
