import { act, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type {
  AnalyzeOverview,
  RangeAnalysis,
  Series,
  SessionWindow,
} from '../api/types'

// uPlot needs a canvas jsdom does not provide. Stub the pane and record its
// props so tests can drive interactions (select / viewport) and inspect data.
const h = vi.hoisted(() => ({
  panes: {} as Record<string, any>,
  analyzeOverview: vi.fn(),
  analyzeRange: vi.fn(),
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
      analyzeOverview: h.analyzeOverview,
      analyzeRange: h.analyzeRange,
      sessionWindow: h.sessionWindow,
      listSessions: h.listSessions,
    },
  }
})

import { AnalyzePage } from './Analyze'

const START = 1_000_000

function series(key: string, label: string, unit: string, points: Series['points']): Series {
  return { key, label, unit, points }
}

function overview(overrides: Partial<AnalyzeOverview> = {}): AnalyzeOverview {
  return {
    path: 'sessions/s.jsonl',
    label: 's.jsonl',
    note: 'demo',
    status: 'ok',
    recovered: false,
    startWallMs: START,
    endWallMs: START + 60_000,
    durationS: 60,
    intervalMs: 1000,
    stats: {
      totalSamples: 120,
      ticks: 60,
      collectorSamples: [
        { name: 'battery', samples: 60 },
        { name: 'cpu', samples: 60 },
      ],
      firstSampleWallMs: START,
      lastSampleWallMs: START + 59_000,
      observedHz: 1,
      events: 3,
      markers: 1,
      timeouts: 1,
      recoveries: 0,
      errors: 0,
      discontinuities: 0,
      durationS: 59,
    },
    collectors: [],
    events: [
      { wallMs: START + 20_000, monoMs: 20_000, kind: 'marker', detail: 'lunch', severity: 'marker' },
    ],
    regions: [],
    domains: [
      { domain: 'power', available: true },
      { domain: 'gpu', available: false, reason: 'no gpu evidence was recorded' },
    ],
    coveragePct: 98.7,
    discontinuities: 0,
    markers: 1,
    series: [
      series('battery_discharge_w', 'Battery discharge', 'W', [
        { tMs: START, value: 6, provenance: 'measured', quality: 'fresh' },
        { tMs: START + 30_000, value: null, provenance: 'unavailable', quality: 'unknown' },
        { tMs: START + 59_000, value: 8, provenance: 'measured', quality: 'fresh' },
      ]),
      series('cpu_utility_pct', 'CPU utility', '%', [
        { tMs: START, value: 5, provenance: 'measured', quality: 'fresh' },
      ]),
    ],
    ...overrides,
  }
}

function range(overrides: Partial<RangeAnalysis> = {}): RangeAnalysis {
  return {
    path: 'sessions/s.jsonl',
    fromMs: START,
    toMs: START + 30_000,
    durationS: 30,
    quality: {
      spanS: 30,
      coveredS: 12,
      unknownS: 18,
      unobservedS: 5,
      coverage: 0.4,
      discontinuities: 1,
      timeouts: 1,
      recoveries: 0,
      errors: 0,
      events: 2,
      markers: 1,
      samples: 30,
      collectorCounts: [{ name: 'battery', samples: 30 }],
      staleCollectors: ['gpu'],
    },
    energy: {
      dischargeWh: 0.05,
      chargeWh: null,
      coveredS: 12,
      unknownS: 18,
      unobservedS: 5,
      discontinuities: 1,
      crossesDiscontinuity: true,
      dischargePresent: true,
      chargePresent: false,
    },
    domains: [
      {
        domain: 'power',
        available: true,
        metrics: [
          {
            key: 'battery_discharge_w',
            label: 'Battery discharge',
            unit: 'W',
            n: 30,
            known: 12,
            median: 7,
            min: 5,
            max: 9,
            p10: 5.5,
            p90: 8.5,
            provenance: 'measured',
          },
        ],
      },
    ],
    processes: {
      ticks: 30,
      entries: [
        {
          pid: 42,
          name: 'game.exe',
          ppid: 1,
          cpuMedianPct: 70,
          cpuMaxPct: 90,
          presence: 30,
          ticks: 30,
          firstSeenMs: START,
          lastSeenMs: START + 30_000,
          startUnixMs: null,
        },
      ],
      totalThreads: 2200,
      inaccessible: 12,
      truncated: true,
      incomplete: true,
      note: 'Process evidence incomplete for this interval.',
    },
    changes: [
      {
        domain: 'cpu',
        key: 'cpu_utility_pct',
        label: 'CPU utility',
        unit: '%',
        reference: 5,
        during: 40,
        after: 38,
        delta: 35,
        direction: 'increased',
        confidence: 'high',
        basis: 'CPU activity changed during this interval',
        nBefore: 30,
        nDuring: 30,
        nAfter: 20,
      },
    ],
    categorical: [],
    correlations: [],
    ...overrides,
  }
}

function window_(marker: number): SessionWindow {
  return {
    path: 'sessions/s.jsonl',
    intervalMs: 1000,
    fromMs: 0,
    toMs: 0,
    maxPoints: 1600,
    downsampled: false,
    sourcePoints: 1,
    series: [
      series('battery_discharge_w', 'Battery discharge', 'W', [
        { tMs: START + marker, value: marker, provenance: 'measured', quality: 'fresh' },
      ]),
    ],
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
const POWER = 'Power (battery discharge)'

async function renderAnalyze(path: string | null = 'sessions/s.jsonl') {
  const view = render(
    <AnalyzePage sessionPath={path} onSelectSession={() => {}} onOpenDetails={() => {}} />,
  )
  return view
}

describe('AnalyzePage', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    h.panes = {}
    h.listSessions.mockResolvedValue([])
  })

  it('shows the session picker instead of silently selecting one', async () => {
    renderAnalyze(null)
    expect(
      screen.getByText(/Select a recording explicitly/),
    ).toBeTruthy()
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
    expect(h.analyzeOverview).not.toHaveBeenCalled()
  })

  it('shows whole-session stats and preserves unavailable gaps as gaps', async () => {
    h.analyzeOverview.mockResolvedValue(overview())
    renderAnalyze()
    await waitFor(() => expect(h.analyzeOverview).toHaveBeenCalled())
    expect(await screen.findByText('Samples')).toBeTruthy()
    await waitFor(() => expect(h.panes[POWER]).toBeTruthy())
    // The null reading must remain null, never zero.
    const points: any[] = h.panes[POWER].points
    expect(points.some((p) => p.value === null)).toBe(true)
    expect(points.every((p) => p.value !== 0)).toBe(true)
  })

  it('renders range quality, changes and the process incompleteness warning', async () => {
    h.analyzeOverview.mockResolvedValue(overview())
    h.analyzeRange.mockResolvedValue(range())
    renderAnalyze()
    await waitFor(() => expect(h.panes[POWER]).toBeTruthy())

    act(() => h.panes[POWER].onSelect([START, START + 30_000]))
    await waitFor(() => expect(h.analyzeRange).toHaveBeenCalledWith('sessions/s.jsonl', START, START + 30_000))

    expect((await screen.findAllByText(/Coverage/)).length).toBeGreaterThan(0)
    expect(screen.getAllByText('40.0%').length).toBeGreaterThan(0)
    expect(screen.getByText(/Evidence for this interval is weak/)).toBeTruthy()
    expect(screen.getByText(/CPU activity changed during this interval/)).toBeTruthy()
    expect(screen.getByText(/Process evidence incomplete/)).toBeTruthy()
    expect(screen.getByText(/12 process\(es\) inaccessible/)).toBeTruthy()
  })

  it('centers the timeline and opens details when an event is clicked', async () => {
    h.analyzeOverview.mockResolvedValue(overview())
    renderAnalyze()
    const chip = await screen.findByText(/lunch/)
    act(() => chip.click())
    expect(await screen.findByText('Close')).toBeTruthy()
    expect(screen.getAllByText(/lunch/).length).toBeGreaterThan(0)
  })

  it('toggles metric tracks', async () => {
    h.analyzeOverview.mockResolvedValue(overview())
    renderAnalyze()
    await waitFor(() => expect(h.panes[POWER]).toBeTruthy())
    const toggle = screen.getByRole('button', { name: 'CPU utility' })
    expect(toggle.getAttribute('aria-pressed')).toBe('true')
    act(() => toggle.click())
    expect(toggle.getAttribute('aria-pressed')).toBe('false')
  })

  it('surfaces a recovered session state', async () => {
    h.analyzeOverview.mockResolvedValue(overview({ recovered: true, status: 'recovered' }))
    renderAnalyze()
    expect(await screen.findByText('recovered')).toBeTruthy()
  })

  it('requests bounded windows and ignores an obsolete response', async () => {
    h.analyzeOverview.mockResolvedValue(overview())
    let resolveFirst: (w: SessionWindow) => void = () => {}
    let resolveSecond: (w: SessionWindow) => void = () => {}
    h.sessionWindow
      .mockImplementationOnce(() => new Promise((r) => (resolveFirst = r)))
      .mockImplementationOnce(() => new Promise((r) => (resolveSecond = r)))
    renderAnalyze()
    await waitFor(() => expect(h.panes[POWER]).toBeTruthy())

    await act(async () => {
      h.panes[POWER].onViewport([START, START + 10_000])
    })
    await sleep(180)
    await act(async () => {
      h.panes[POWER].onViewport([START, START + 20_000])
    })
    await sleep(180)
    expect(h.sessionWindow).toHaveBeenCalledTimes(2)
    // Bounded request: padded beyond the viewport, never unbounded.
    const first = h.sessionWindow.mock.calls[0][1]
    expect(first.fromMs).toBeGreaterThanOrEqual(0)
    expect(first.toMs).toBeGreaterThan(START + 10_000)

    await act(async () => {
      resolveSecond(window_(2000))
    })
    await act(async () => {
      resolveFirst(window_(1000))
    })
    // The stale first response must not overwrite the newer one.
    const points: any[] = h.panes[POWER].points
    expect(points[0].value).toBe(2000)
  })
})
