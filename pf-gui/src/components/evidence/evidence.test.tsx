import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { CollectorStateRow, EvidenceBadge, EvidenceValue } from './index'
import type { CollectorState, EvidenceValue as Ev } from '../../api/types'

function ev(overrides: Partial<Ev>): Ev {
  return {
    value: null,
    provenance: 'unavailable',
    quality: 'unknown',
    absenceKind: 'unsupported',
    reason: 'no sensor',
    source: 'battery',
    unit: 'W',
    wallMs: 0,
    monoMs: 0,
    ...overrides,
  }
}

describe('EvidenceValue', () => {
  it('renders a dash and the reason for an unavailable reading, never zero', () => {
    render(<EvidenceValue evidence={ev({})} />)
    expect(screen.getByText('—')).toBeTruthy()
    expect(screen.getByText(/no sensor/)).toBeTruthy()
    expect(screen.getByText('Unavailable')).toBeTruthy()
    expect(screen.queryByText('0')).toBeNull()
  })

  it('renders the measured value with its provenance badge', () => {
    render(<EvidenceValue evidence={ev({ value: 8.4, provenance: 'measured', quality: 'fresh', reason: null })} />)
    expect(screen.getByText('8.4')).toBeTruthy()
    expect(screen.getByText('Measured')).toBeTruthy()
    expect(screen.queryByText('Unavailable')).toBeNull()
  })

  it('shows the sample age for a live reading', () => {
    render(
      <EvidenceValue
        evidence={ev({ value: 7.2, provenance: 'measured', quality: 'fresh', reason: null, ageMs: 400 })}
      />,
    )
    expect(screen.getByText('400 ms ago')).toBeTruthy()
  })

  it('keeps a stale value but marks it stale rather than current', () => {
    render(
      <EvidenceValue
        evidence={ev({ value: 7.2, provenance: 'measured', quality: 'stale', reason: null, ageMs: 12_000 })}
      />,
    )
    expect(screen.getByText('7.2')).toBeTruthy()
    expect(screen.getByText('Stale')).toBeTruthy()
    expect(screen.getByText('12 s ago')).toBeTruthy()
  })

  it('treats a missing evidence object as unavailable', () => {
    render(<EvidenceValue evidence={null} />)
    expect(screen.getByText('—')).toBeTruthy()
  })
})

describe('EvidenceBadge', () => {
  it('labels derived and estimated provenance distinctly', () => {
    const { rerender } = render(<EvidenceBadge provenance="derived" />)
    expect(screen.getByText('Derived')).toBeTruthy()
    rerender(<EvidenceBadge provenance="estimated" />)
    expect(screen.getByText('Estimated')).toBeTruthy()
  })
})

describe('CollectorStateRow', () => {
  it('shows a degraded collector with its failure reason', () => {
    const state: CollectorState = {
      name: 'net',
      state: 'degraded',
      reason: '2 of 10 samples unavailable',
      samples: 10,
      failures: 2,
      requiresAdmin: false,
      elevationWouldHelp: false,
    }
    render(<CollectorStateRow state={state} />)
    expect(screen.getByText('net')).toBeTruthy()
    expect(screen.getByText('Degraded')).toBeTruthy()
    expect(screen.getByText(/2 of 10/)).toBeTruthy()
  })

  it('shows an unavailable collector with elevation hint', () => {
    const state: CollectorState = {
      name: 'storage',
      state: 'unavailable',
      reason: 'requires administrator privileges',
      samples: 0,
      failures: 0,
      requiresAdmin: true,
      elevationWouldHelp: true,
    }
    render(<CollectorStateRow state={state} />)
    expect(screen.getByText('Unavailable')).toBeTruthy()
    expect(screen.getByText('elevation helps')).toBeTruthy()
  })
})
