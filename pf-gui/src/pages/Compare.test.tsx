import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ComparisonAnalysis, SessionListEntry, SessionWindow } from '../api/types'

const h = vi.hoisted(() => ({
  panes: {} as Record<string, any>,
  compareSessions: vi.fn(),
  sessionWindow: vi.fn(),
  listSessions: vi.fn(),
}))

vi.mock('../components/analyze/TimelinePane', () => ({
  TimelinePane: (props: any) => {
    h.panes[props.label] = props
    return <div data-testid={`pane-${props.label}`} />
  },
}))

vi.mock('../api/bridge', () => {
  class BridgeFailure extends Error {
    code = 'unknown'
    detail?: string
  }
  return {
    BridgeFailure,
    isTauri: () => true,
    bridge: {
      compareSessions: h.compareSessions,
      sessionWindow: h.sessionWindow,
      listSessions: h.listSessions,
    },
  }
})

import { ComparePage } from './Compare'

const NOW = 1_000_000

function entry(file: string, note: string): SessionListEntry {
  return {
    file,
    path: `sessions/${file}`,
    label: file,
    note,
    startWallMs: NOW,
    endWallMs: NOW + 600_000,
    durationS: 600,
    status: 'ok',
    recovered: false,
    mode: 'recorded',
    samples: 600,
    intervalMs: 1000,
    dischargeMedianW: 8.0,
    dischargeWh: 1.2,
    chargeWh: null,
    cpuMedianPct: 10,
    coveragePct: 98,
    discontinuities: 0,
    markers: 0,
    collectors: ['battery', 'cpu'],
    bytes: 1000,
    mtimeS: 1,
  }
}

function analysis(overrides: Partial<ComparisonAnalysis> = {}): ComparisonAnalysis {
  const side = (label: string) => ({
    path: `sessions/${label}`,
    label,
    note: label,
    status: 'ok' as const,
    recovered: false,
    wholeSession: true,
    fromMs: NOW,
    toMs: NOW + 600_000,
    durationS: 600,
    intervalMs: 1000,
    coverage: 0.99,
    energy: {
      dischargeWh: 1.2,
      chargeWh: null,
      coveredS: 594,
      unknownS: 6,
      unobservedS: 0,
      discontinuities: 0,
      crossesDiscontinuity: false,
      dischargePresent: true,
      chargePresent: false,
    },
    quality: {
      spanS: 600,
      coveredS: 594,
      unknownS: 6,
      unobservedS: 0,
      coverage: 0.99,
      discontinuities: 0,
      timeouts: 0,
      recoveries: 0,
      errors: 0,
      events: 0,
      markers: 0,
      samples: 600,
      collectorCounts: [{ name: 'battery', samples: 600 }],
      staleCollectors: [],
    },
    context: {
      powerScheme: 'Balanced',
      powerSource: 'battery',
      refreshHz: '60 Hz',
      displayCount: 1,
      gpuAdapters: [],
      effectiveCadenceMs: 1000,
    },
  })
  return {
    a: side('a.jsonl'),
    b: side('b.jsonl'),
    comparability: {
      overall: 'weak',
      findings: [
        { metric: null, level: 'weak', reason: 'low discharge coverage (A 99%, B 42%)' },
        { metric: 'gpu_util_pct', level: 'not_comparable', reason: 'missing evidence in B for GPU utilization' },
      ],
    },
    quality: {
      coverageA: 0.99,
      coverageB: 0.42,
      discontinuitiesA: 0,
      discontinuitiesB: 1,
      timeoutsA: 0,
      timeoutsB: 2,
      staleCollectorsA: [],
      staleCollectorsB: ['gpu'],
      weakestSide: 'B',
      note: 'Coverage A 99% / B 42%',
    },
    energy: {
      totalWhA: 1.2,
      totalWhB: 1.1,
      avgPowerWA: 8.0,
      avgPowerWB: 7.0,
      normalizedWhA: 7.27,
      normalizedWhB: 6.6,
      durationRatio: 1,
      durationsSimilar: true,
      preferredBasis: 'total_energy',
      note: 'Durations are similar.',
    },
    headlineMetrics: [
      {
        metric: 'battery_discharge_w',
        label: 'Battery discharge',
        domain: 'power',
        unit: 'W',
        a: 11.4,
        b: 8.7,
        absoluteDelta: -2.7,
        relativeDelta: -0.237,
        direction: 'decreased',
        sampleCountA: 600,
        sampleCountB: 250,
        coverageA: 0.99,
        coverageB: 0.42,
        provenanceA: 'measured',
        provenanceB: 'estimated',
        comparability: 'compatible_with_caveats',
        comparabilityReason: 'partial sample coverage in range (A 99%, B 42%)',
        evidence: 'measured on A, estimated on B',
        reliability: { level: 'weak_evidence', note: 'effective n 8', noiseFloor: 0.5, effectSize: 2.1 },
      },
      {
        metric: 'gpu_util_pct',
        label: 'GPU utilization',
        domain: 'gpu',
        unit: '%',
        a: 30,
        b: null,
        absoluteDelta: null,
        relativeDelta: null,
        direction: 'missing_b',
        sampleCountA: 600,
        sampleCountB: 0,
        coverageA: 0.99,
        coverageB: null,
        provenanceA: 'measured',
        provenanceB: 'unavailable',
        comparability: 'not_comparable',
        comparabilityReason: 'missing evidence in B for GPU utilization',
        evidence: 'measured on A, unavailable on B',
        reliability: { level: 'insufficient', note: 'no defensible comparison', noiseFloor: null, effectSize: null },
      },
    ],
    rankedChanges: [
      {
        metric: 'battery_discharge_w',
        label: 'Battery discharge',
        domain: 'power',
        unit: 'W',
        delta: -2.7,
        relativeDelta: -0.237,
        direction: 'decreased',
        relevance: 5.4,
        comparability: 'compatible_with_caveats',
        reliability: 'weak_evidence',
        basis: 'measured on A, estimated on B',
      },
    ],
    domains: [
      {
        domain: 'gpu',
        availableA: true,
        availableB: false,
        unavailableReason: 'not comparable — missing gpu evidence in B',
        metrics: [],
      },
    ],
    processes: {
      aTicks: 300,
      bTicks: 100,
      aIncomplete: false,
      bIncomplete: true,
      aTotalThreads: 2200,
      bTotalThreads: 2100,
      incomplete: true,
      rows: [
        {
          identity: 'game',
          displayName: 'game.exe',
          aCpuMedianPct: 70,
          bCpuMedianPct: null,
          delta: null,
          presence: 'a_only',
          aPresence: 300,
          bPresence: 0,
          aTicks: 300,
          bTicks: 100,
          aAmbiguous: false,
          bAmbiguous: false,
          note: 'not observed in B (evidence incomplete)',
        },
      ],
    },
    categoricalDifferences: [
      { key: 'power_source', label: 'Power source', a: 'battery', b: 'AC', state: 'changed', note: 'battery → AC' },
    ],
    distributions: [
      {
        metric: 'battery_discharge_w',
        label: 'Battery discharge',
        unit: 'W',
        a: { n: 600, known: 594, min: 9, p10: 10, median: 11.4, p90: 12.5, max: 13, spread: 2.5 },
        b: { n: 250, known: 150, min: 5, p10: 6, median: 8.7, p90: 11, max: 12, spread: 5 },
      },
    ],
    correlations: [
      {
        x: 'battery_discharge_w',
        y: 'cpu_utility_pct',
        label: 'Power vs CPU utility',
        rA: 0.82,
        rB: 0.41,
        nA: 600,
        nB: 250,
        effectiveNA: 40,
        effectiveNB: 8,
        comparable: false,
        note: 'B has insufficient evidence for a coefficient',
      },
    ],
    caveats: [
      { scope: 'coverage', severity: 'critical', message: 'Low discharge coverage (A 99%, B 42%)' },
      { scope: 'processes', severity: 'warning', message: 'Process evidence incomplete; not observed ≠ did not run.' },
    ],
    ...overrides,
  }
}

function window_(): SessionWindow {
  return {
    path: 'sessions/a.jsonl',
    intervalMs: 1000,
    fromMs: 0,
    toMs: 0,
    maxPoints: 1200,
    downsampled: false,
    sourcePoints: 2,
    series: [
      {
        key: 'battery_discharge_w',
        label: 'Battery discharge',
        unit: 'W',
        points: [
          { tMs: NOW, value: 11.4, provenance: 'measured', quality: 'fresh' },
          { tMs: NOW + 1000, value: 8.7, provenance: 'measured', quality: 'fresh' },
        ],
      },
    ],
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

async function selectB() {
  const combos = screen.getAllByRole('combobox')
  // A select is first; B is the second.
  await act(async () => {
    fireEvent.change(combos[1], { target: { value: 'sessions/b.jsonl' } })
  })
}

describe('ComparePage', () => {
  beforeEach(() => {
    vi.resetAllMocks()
    h.panes = {}
    h.listSessions.mockResolvedValue([entry('a.jsonl', 'Alpha'), entry('b.jsonl', 'Beta')])
    h.sessionWindow.mockResolvedValue(window_())
    h.compareSessions.mockResolvedValue(analysis())
  })

  it('requires both sides before running the comparison', async () => {
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    expect(h.compareSessions).not.toHaveBeenCalled()
    expect(screen.getByText(/Choose session A and session B/)).toBeTruthy()
  })

  it('renders headline differences and prominent quality warnings', async () => {
    h.compareSessions.mockResolvedValue(analysis())
    const view = render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await selectB()
    await waitFor(() => expect(h.compareSessions).toHaveBeenCalledWith({ path: 'sessions/a.jsonl' }, { path: 'sessions/b.jsonl' }))
    expect(await screen.findByText('Headline differences')).toBeTruthy()
    expect(screen.getAllByText(/Weak comparison/).length).toBeGreaterThan(0)
    expect(screen.getAllByText(/Low discharge coverage/).length).toBeGreaterThan(0)
    expect(view.container.querySelector('.quality-warning')).toBeTruthy()
  })

  it('shows a not-comparable metric and withholds a percent on a missing delta', async () => {
    h.compareSessions.mockResolvedValue(analysis())
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await selectB()
    expect((await screen.findAllByText('GPU utilization')).length).toBeGreaterThan(0)
    expect(screen.getAllByText(/Not comparable/).length).toBeGreaterThan(0)
    // Battery delta: absolute + relative supplied by Rust.
    expect(screen.getAllByText(/-2.7 W \(-23.7%\)/).length).toBeGreaterThan(0)
  })

  it('swaps A and B', async () => {
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    let combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await act(async () => fireEvent.change(combos[1], { target: { value: 'sessions/b.jsonl' } }))
    await act(async () => {
      screen.getByText('⇄ Swap').click()
    })
    combos = screen.getAllByRole('combobox')
    expect((combos[0] as HTMLSelectElement).value).toBe('sessions/b.jsonl')
    expect((combos[1] as HTMLSelectElement).value).toBe('sessions/a.jsonl')
  })

  it('renders largest differences and the process not-observed warning', async () => {
    h.compareSessions.mockResolvedValue(analysis())
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await selectB()
    expect(await screen.findByText('Largest observed differences')).toBeTruthy()
    expect(screen.getByText('Process comparison')).toBeTruthy()
    // Absence is qualified as "not observed", never a bare "did not run" claim.
    expect(screen.getAllByText(/A only · not observed in B/).length).toBeGreaterThan(0)
  })

  it('switches timeline alignment and passes the x-mode through', async () => {
    h.compareSessions.mockResolvedValue(analysis())
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await selectB()
    await waitFor(() => expect(h.panes['A — a.jsonl']).toBeTruthy())
    expect(h.panes['A — a.jsonl'].xMode).toBe('elapsed')
    await act(async () => {
      screen.getByText('Normalized 0–100%').click()
    })
    await waitFor(() => expect(h.panes['A — a.jsonl'].xMode).toBe('normalized'))
  })

  it('pre-populates side A from an Analyze range seed without deep comparison', async () => {
    render(
      <ComparePage
        seed={{ path: 'sessions/a.jsonl', fromMs: NOW + 10_000, toMs: NOW + 40_000 }}
        onAnalyze={() => {}}
        onNotice={() => {}}
      />,
    )
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combo = screen.getAllByRole('combobox')[0] as HTMLSelectElement
    expect(combo.value).toBe('sessions/a.jsonl')
    // Only one side is configured, so no deep comparison runs yet.
    expect(h.compareSessions).not.toHaveBeenCalled()
  })

  it('navigates from Compare into Analyze with the resolved range', async () => {
    h.compareSessions.mockResolvedValue(analysis())
    const onAnalyze = vi.fn()
    render(<ComparePage seed={null} onAnalyze={onAnalyze} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await selectB()
    const btn = await screen.findByText('Analyze A (whole)')
    act(() => btn.click())
    expect(onAnalyze).toHaveBeenCalledWith('sessions/a.jsonl')
  })

  it('ignores an obsolete comparison response when B changes', async () => {
    let resolveFirst: (a: ComparisonAnalysis) => void = () => {}
    let resolveSecond: (a: ComparisonAnalysis) => void = () => {}
    h.compareSessions
      .mockImplementationOnce(() => new Promise((r) => (resolveFirst = r)))
      .mockImplementationOnce(() => new Promise((r) => (resolveSecond = r)))
    render(<ComparePage seed={null} onAnalyze={() => {}} onNotice={() => {}} />)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    const combos = screen.getAllByRole('combobox')
    await act(async () => fireEvent.change(combos[0], { target: { value: 'sessions/a.jsonl' } }))
    await act(async () => fireEvent.change(combos[1], { target: { value: 'sessions/b.jsonl' } }))
    await sleep(180)
    // Change B again before the first response resolves.
    await act(async () =>
      fireEvent.change(screen.getAllByRole('combobox')[1], { target: { value: 'sessions/b.jsonl' } }),
    )
    await sleep(180)
    const first = analysis({
      caveats: [{ scope: 'comparison', severity: 'info', message: 'STALE FIRST RESPONSE' }],
    })
    const second = analysis({
      caveats: [{ scope: 'comparison', severity: 'info', message: 'CURRENT SECOND RESPONSE' }],
    })
    await act(async () => resolveSecond(second))
    await act(async () => resolveFirst(first))
    await waitFor(() => expect(screen.getByText('CURRENT SECOND RESPONSE')).toBeTruthy())
    expect(screen.queryByText('STALE FIRST RESPONSE')).toBeNull()
  })
})
