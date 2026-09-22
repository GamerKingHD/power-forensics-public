import { describe, expect, it } from 'vitest'
import {
  classificationClass,
  classificationLabel,
  metricByKey,
  planRuns,
  validityLabel,
} from './experiments'

describe('experiment planning', () => {
  it('interleaves baseline and treatment by default', () => {
    const plan = planRuns(3, false, 1, false)
    expect(plan.map((p) => p.group)).toEqual([
      'baseline',
      'treatment',
      'baseline',
      'treatment',
      'baseline',
      'treatment',
    ])
    expect(plan.every((p) => p.pairId === null)).toBe(true)
  })

  it('assigns pair ids for paired designs', () => {
    const plan = planRuns(2, false, 1, true)
    expect(plan.map((p) => p.pairId)).toEqual([1, 1, 2, 2])
  })

  it('randomizes deterministically from a persisted seed', () => {
    const a = planRuns(5, true, 12345, false)
    const b = planRuns(5, true, 12345, false)
    // Same seed -> same order, so it is never regenerated halfway through.
    expect(a.map((p) => p.group)).toEqual(b.map((p) => p.group))
    // Still the same multiset of groups.
    expect(a.filter((p) => p.group === 'baseline').length).toBe(5)
    expect(a.filter((p) => p.group === 'treatment').length).toBe(5)
  })

  it('does not require formal terminology to pick a metric', () => {
    expect(metricByKey('battery_discharge_w')?.defaultDirection).toBe('lower')
    expect(metricByKey('net_rx_bps')?.defaultDirection).toBe('neutral')
  })

  it('uses neutral result language', () => {
    expect(classificationLabel('consistent_difference')).toContain('difference')
    expect(classificationClass('confounded')).toBe('error')
    expect(validityLabel('insufficient')).toBe('Insufficient')
  })
})
