import { describe, expect, it } from 'vitest'
import { displayUnit, formatAge, formatEvidence, formatNumber } from './format'
import type { EvidenceValue } from '../api/types'

const measured: EvidenceValue = {
  value: 8.4,
  provenance: 'measured',
  quality: 'fresh',
  source: 'battery',
  unit: 'W',
  wallMs: 1,
  monoMs: 1,
}

describe('formatNumber', () => {
  it('formats watts and percentages without noise', () => {
    expect(formatNumber(8.4, 'W')).toBe('8.4')
    expect(formatNumber(13.8123, 'W')).toBe('13.81')
    expect(formatNumber(71.4, '%')).toBe('71')
    expect(formatNumber(4.25, '%')).toBe('4.3')
  })

  it('renders health ratios as percent and bytes rates as human units', () => {
    expect(formatNumber(0.93, 'ratio')).toBe('93')
    expect(formatNumber(2048, 'B/s')).toBe('2 KiB/s')
  })
})

describe('formatEvidence', () => {
  it('renders a dash for unavailable rather than zero', () => {
    const absent: EvidenceValue = {
      value: null,
      provenance: 'unavailable',
      quality: 'unknown',
      absenceKind: 'unsupported',
      reason: 'no sensor',
      source: '',
      unit: 'W',
      wallMs: 0,
      monoMs: 0,
    }
    expect(formatEvidence(absent)).toBe('—')
    expect(formatEvidence(absent)).not.toBe('0 W')
    expect(formatEvidence(null)).toBe('—')
  })

  it('appends the display unit for present values', () => {
    expect(formatEvidence(measured)).toBe('8.4 W')
    expect(displayUnit('ratio')).toBe('%')
    expect(displayUnit('B/s')).toBe('')
  })
})

describe('formatAge', () => {
  it('renders distinct ages for independently sampled readings', () => {
    expect(formatAge(400)).toBe('400 ms ago')
    expect(formatAge(1800)).toBe('1.8 s ago')
    expect(formatAge(65_000)).toBe('1m 5s ago')
  })

  it('treats unknown age as empty, never zero', () => {
    expect(formatAge(null)).toBe('')
    expect(formatAge(undefined)).toBe('')
    expect(formatAge(-1)).toBe('')
  })
})
