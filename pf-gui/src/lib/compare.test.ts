import { describe, expect, it } from 'vitest'
import type { ComparisonSide, TimelinePoint } from '../api/types'
import { alignmentXMode, formatDelta, formatDeltaValue, transformPoints } from './compare'

function side(fromMs: number, toMs: number): ComparisonSide {
  return {
    path: 'p',
    label: 'p',
    note: '',
    status: 'ok',
    recovered: false,
    wholeSession: false,
    fromMs,
    toMs,
    durationS: (toMs - fromMs) / 1000,
    intervalMs: 1000,
    coverage: 1,
    energy: {
      dischargeWh: null,
      chargeWh: null,
      coveredS: 0,
      unknownS: 0,
      unobservedS: 0,
      discontinuities: 0,
      crossesDiscontinuity: false,
      dischargePresent: false,
      chargePresent: false,
    },
    quality: {
      spanS: 0,
      coveredS: 0,
      unknownS: 0,
      unobservedS: 0,
      coverage: null,
      discontinuities: 0,
      timeouts: 0,
      recoveries: 0,
      errors: 0,
      events: 0,
      markers: 0,
      samples: 0,
      collectorCounts: [],
      staleCollectors: [],
    },
    context: {
      powerScheme: null,
      powerSource: null,
      refreshHz: null,
      displayCount: null,
      gpuAdapters: [],
      effectiveCadenceMs: null,
    },
  }
}

describe('compare formatting', () => {
  it('never fabricates a percent when the backend withheld one', () => {
    expect(formatDelta(-2.7, null, 'W')).toBe('-2.7 W')
    expect(formatDelta(-2.7, -0.237, 'W')).toBe('-2.7 W (-23.7%)')
    expect(formatDelta(null, null, 'W')).toBe('—')
  })

  it('renders percent units as percentage points', () => {
    expect(formatDeltaValue(-45, '%')).toBe('-45 pp')
  })
})

describe('comparison alignment', () => {
  const points: TimelinePoint[] = [
    { tMs: 10_000, value: 1, provenance: 'measured', quality: 'fresh' },
    { tMs: 20_000, value: 2, provenance: 'measured', quality: 'fresh' },
  ]

  it('maps alignment modes to explicit x clocks', () => {
    expect(alignmentXMode('absolute')).toBe('wall')
    expect(alignmentXMode('start')).toBe('elapsed')
    expect(alignmentXMode('normalized')).toBe('normalized')
  })

  it('start-aligns, end-aligns and normalizes without touching values', () => {
    const s = side(10_000, 20_000)
    expect(transformPoints(points, s, 'start').map((p) => p.tMs)).toEqual([0, 10_000])
    expect(transformPoints(points, s, 'end').map((p) => p.tMs)).toEqual([-10_000, 0])
    expect(transformPoints(points, s, 'normalized').map((p) => p.tMs)).toEqual([0, 100_000])
    expect(transformPoints(points, s, 'absolute').map((p) => p.tMs)).toEqual([10_000, 20_000])
    for (const mode of ['start', 'end', 'normalized', 'absolute'] as const) {
      expect(transformPoints(points, s, mode).map((p) => p.value)).toEqual([1, 2])
    }
  })
})
