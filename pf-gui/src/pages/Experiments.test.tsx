import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type {
  ExperimentAnalysis,
  ExperimentRecord,
  ExperimentSummary,
  RunOutcome,
  SessionListEntry,
} from '../api/types'

const h = vi.hoisted(() => ({
  listExperiments: vi.fn(),
  getExperiment: vi.fn(),
  createExperiment: vi.fn(),
  saveExperiment: vi.fn(),
  deleteExperiment: vi.fn(),
  analyzeExperiment: vi.fn(),
  validateExperimentRun: vi.fn(),
  listSessions: vi.fn(),
  start: vi.fn(),
  stop: vi.fn(),
  agentStatus: vi.fn(),
  addMarker: vi.fn(),
}))

vi.mock('../api/bridge', () => {
  class BridgeFailure extends Error {
    code = 'unknown'
    detail?: string
  }
  return {
    BridgeFailure,
    isTauri: () => true,
    bridge: h,
  }
})

import { ExperimentsPage } from './Experiments'

const NOW = 1_700_000_000_000

function session(file: string): SessionListEntry {
  return {
    file,
    path: `sessions/${file}`,
    label: file,
    note: file,
    startWallMs: NOW,
    endWallMs: NOW + 600_000,
    durationS: 600,
    status: 'ok',
    recovered: false,
    mode: 'recorded',
    samples: 600,
    intervalMs: 1000,
    dischargeMedianW: 9,
    dischargeWh: 1.2,
    chargeWh: null,
    cpuMedianPct: 10,
    coveragePct: 98,
    discontinuities: 0,
    markers: 0,
    collectors: ['battery'],
    bytes: 1000,
    mtimeS: 1,
  }
}

function runOutcome(id: string, group: 'baseline' | 'treatment', value: number): RunOutcome {
  return {
    id,
    label: id,
    group,
    order: group === 'baseline' ? 0 : 1,
    pairId: 1,
    included: true,
    available: true,
    unavailableReason: null,
    fromMs: NOW,
    toMs: NOW + 120_000,
    durationS: 120,
    primaryValue: value,
    primaryKnown: 120,
    coverage: 0.98,
    discontinuities: 0,
    timeouts: 0,
    cadenceMs: 1000,
    context: {
      powerScheme: 'Balanced',
      powerSource: 'battery',
      refreshHz: group === 'baseline' ? '120 Hz' : '60 Hz',
      displayCount: 1,
      gpuAdapters: [],
      effectiveCadenceMs: 1000,
    },
    collectors: ['battery'],
    processIncomplete: false,
    processNames: ['explorer.exe'],
    secondary: [],
    validation: { status: 'accepted', primaryPresent: true, findings: [] },
  }
}

function record(overrides: Partial<ExperimentRecord> = {}): ExperimentRecord {
  return {
    schema: 1,
    id: 'exp-1',
    name: '60 Hz vs 120 Hz',
    question: 'Does reducing refresh rate lower discharge power?',
    primaryMetric: 'battery_discharge_w',
    primaryLabel: 'Battery discharge power',
    primaryUnit: 'W',
    direction: 'lower',
    pairing: 'paired',
    baselineLabel: '120 Hz',
    treatmentLabel: '60 Hz',
    settleS: 30,
    measureS: 120,
    repetitions: 1,
    randomized: false,
    orderSeed: null,
    collectors: null,
    preset: null,
    notes: '',
    status: 'complete',
    createdMs: NOW,
    updatedMs: NOW,
    runs: [
      {
        id: 'A1',
        label: 'A1',
        group: 'baseline',
        order: 0,
        pairId: 1,
        sessionPath: 'sessions/a1.jsonl',
        fromMs: null,
        toMs: null,
        included: true,
        source: 'existing',
        notes: '',
        capturedMs: null,
      },
      {
        id: 'B1',
        label: 'B1',
        group: 'treatment',
        order: 1,
        pairId: 1,
        sessionPath: 'sessions/b1.jsonl',
        fromMs: null,
        toMs: null,
        included: true,
        source: 'existing',
        notes: '',
        capturedMs: null,
      },
    ],
    guided: null,
    lastResult: null,
    ...overrides,
  }
}

function analysis(overrides: Partial<ExperimentAnalysis> = {}): ExperimentAnalysis {
  return {
    analysisVersion: 1,
    primaryMetric: 'battery_discharge_w',
    primaryLabel: 'Battery discharge power',
    primaryUnit: 'W',
    direction: 'lower',
    pairing: 'paired',
    runs: [runOutcome('A1', 'baseline', 11.4), runOutcome('B1', 'treatment', 9.8)],
    baseline: {
      group: 'baseline',
      runsTotal: 1,
      runsIncluded: 1,
      runsExcluded: 0,
      runsWithPrimary: 1,
      runsValid: 1,
      runsInvalid: 0,
      median: 11.4,
      mean: 11.4,
      spread: 0,
      min: 11.4,
      max: 11.4,
      coverageMedian: 0.98,
    },
    treatment: {
      group: 'treatment',
      runsTotal: 1,
      runsIncluded: 1,
      runsExcluded: 0,
      runsWithPrimary: 1,
      runsValid: 1,
      runsInvalid: 0,
      median: 9.8,
      mean: 9.8,
      spread: 0,
      min: 9.8,
      max: 9.8,
      coverageMedian: 0.98,
    },
    effect: {
      baselineValue: 11.4,
      treatmentValue: 9.8,
      absoluteDelta: -1.6,
      relativeDelta: -0.14,
      direction: 'lower',
      effectSize: 4,
      confidenceInterval: null,
      paired: false,
    },
    paired: {
      pairs: [{ pairId: 1, baselineRun: 'A1', treatmentRun: 'B1', baseline: 11.4, treatment: 9.8, delta: -1.6 }],
      usablePairs: 1,
      medianDelta: -1.6,
      spread: 0,
      consistent: false,
    },
    noise: { source: 'run_to_run', estimate: 0.4, note: 'Run-to-run spread is about 0.400.' },
    classification: 'possible_difference',
    validity: 'weak',
    validityReasons: ['exploratory only: 1 baseline / 1 treatment'],
    summaryLines: [
      'Baseline: Battery discharge power median 11.400 W across 1 run(s).',
      'Treatment runs showed 1.600 W lower Battery discharge power than baseline (-14.0%).',
    ],
    confounders: [
      { key: 'power_source', label: 'Power source', state: 'changed', evidence: 'varies across runs (AC / battery)', runs: ['B1'] },
    ],
    secondary: [
      {
        metric: 'cpu_utility_pct',
        label: 'CPU utility',
        unit: '%',
        baselineMedian: 12,
        treatmentMedian: 9,
        delta: -3,
        comparable: true,
        note: 'Observed alongside the primary result; correlation is not causation.',
      },
    ],
    caveats: ['exploratory only: 1 baseline / 1 treatment run(s) with the primary metric'],
    unavailableRuns: [],
    ...overrides,
  }
}

async function openDetail() {
  render(<ExperimentsPage onAnalyze={() => {}} onCompare={() => {}} onNotice={() => {}} />)
  const name = await screen.findByText('60 Hz vs 120 Hz')
  await act(async () => {
    name.click()
  })
  await waitFor(() => expect(h.getExperiment).toHaveBeenCalled())
}

describe('ExperimentsPage', () => {
  beforeEach(() => {
    vi.resetAllMocks()
    h.listExperiments.mockResolvedValue([
      {
        id: 'exp-1',
        name: '60 Hz vs 120 Hz',
        question: 'Does reducing refresh rate lower discharge power?',
        primaryMetric: 'battery_discharge_w',
        primaryLabel: 'Battery discharge power',
        primaryUnit: 'W',
        baselineLabel: '120 Hz',
        treatmentLabel: '60 Hz',
        status: 'draft',
        baselineRuns: 1,
        treatmentRuns: 1,
        includedRuns: 2,
        missingRuns: 0,
        updatedMs: NOW,
        lastResult: null,
      } satisfies ExperimentSummary,
    ])
    h.getExperiment.mockResolvedValue(record())
    h.analyzeExperiment.mockResolvedValue(analysis())
    h.listSessions.mockResolvedValue([session('a1.jsonl'), session('b1.jsonl')])
    h.saveExperiment.mockImplementation((r: ExperimentRecord) => Promise.resolve(r))
  })

  it('renders a purposeful empty state with no experiments', async () => {
    h.listExperiments.mockResolvedValue([])
    render(<ExperimentsPage onAnalyze={() => {}} onCompare={() => {}} onNotice={() => {}} />)
    expect(await screen.findByText('No experiments yet')).toBeTruthy()
    expect(screen.getByText('Baseline runs')).toBeTruthy()
    expect(screen.getByText('Treatment runs')).toBeTruthy()
    expect(screen.getByText('Repeated trials')).toBeTruthy()
    expect(screen.getByText('Confounder checks')).toBeTruthy()
    expect(screen.getByText('New experiment')).toBeTruthy()
  })

  it('lists experiments and opens the detail view', async () => {
    await openDetail()
    expect(h.analyzeExperiment).toHaveBeenCalledWith('exp-1')
    expect(await screen.findByText('Primary result')).toBeTruthy()
    expect(screen.getAllByText(/Battery discharge power/).length).toBeGreaterThan(0)
  })

  it('creates a new experiment from the compact form', async () => {
    h.createExperiment.mockResolvedValue(record({ id: 'exp-2', name: 'My test' }))
    h.getExperiment.mockResolvedValue(record({ id: 'exp-2', name: 'My test' }))
    render(<ExperimentsPage onAnalyze={() => {}} onCompare={() => {}} onNotice={() => {}} />)
    await screen.findByText('60 Hz vs 120 Hz')
    await act(async () => {
      screen.getByText('New experiment').click()
    })
    const nameInput = screen.getByPlaceholderText('60 Hz vs 120 Hz') as HTMLInputElement
    fireEvent.change(nameInput, { target: { value: 'My test' } })
    await act(async () => {
      screen.getByText('Create').click()
    })
    await waitFor(() => expect(h.createExperiment).toHaveBeenCalled())
    const req = h.createExperiment.mock.calls[0][0]
    expect(req.name).toBe('My test')
    expect(req.primaryMetric).toBe('battery_discharge_w')
  })

  it('assigns an existing session to a group', async () => {
    await openDetail()
    const select = screen.getByRole('combobox') as HTMLSelectElement
    await act(async () => {
      fireEvent.change(select, { target: { value: 'sessions/a1.jsonl' } })
    })
    await act(async () => {
      screen.getByText('Add as treatment').click()
    })
    await waitFor(() => expect(h.saveExperiment).toHaveBeenCalled())
    const saved = h.saveExperiment.mock.calls.at(-1)![0] as ExperimentRecord
    expect(saved.runs.some((r) => r.group === 'treatment' && r.sessionPath === 'sessions/a1.jsonl')).toBe(true)
  })

  it('excludes and reincludes a run through metadata only', async () => {
    await openDetail()
    const box = (await screen.findByLabelText('Include run A1')) as HTMLInputElement
    expect(box.checked).toBe(true)
    await act(async () => {
      fireEvent.click(box)
    })
    await waitFor(() => expect(h.saveExperiment).toHaveBeenCalled())
    const saved = h.saveExperiment.mock.calls.at(-1)![0] as ExperimentRecord
    expect(saved.runs.find((r) => r.id === 'A1')!.included).toBe(false)
  })

  it('renders the result language, confounders and the paired plot', async () => {
    await openDetail()
    expect(await screen.findByText(/Treatment runs showed 1.600 W lower/)).toBeTruthy()
    expect(screen.getByText('Quality & confounders')).toBeTruthy()
    expect(screen.getByText('Power source')).toBeTruthy()
    expect(screen.getByLabelText('Run outcome distribution')).toBeTruthy()
    expect(screen.getByText('Secondary differences')).toBeTruthy()
  })

  it('shows an insufficient/weak state for N=1 without overclaiming', async () => {
    h.analyzeExperiment.mockResolvedValue(
      analysis({
        classification: 'insufficient',
        validity: 'insufficient',
        summaryLines: ['Only one usable run per group: exploratory only, no reproducibility evidence.'],
      }),
    )
    await openDetail()
    expect((await screen.findAllByText(/exploratory only/)).length).toBeGreaterThan(0)
    expect(screen.getAllByText('Insufficient evidence').length).toBeGreaterThan(0)
  })

  it('surfaces an unavailable run rather than dropping it', async () => {
    h.analyzeExperiment.mockResolvedValue(
      analysis({
        unavailableRuns: [
          { id: 'B2', label: 'B2', group: 'treatment', order: 2, sessionPath: 'sessions/gone.jsonl', reason: 'not-found: session not found' },
        ],
      }),
    )
    await openDetail()
    expect(await screen.findByText(/unavailable —/)).toBeTruthy()
  })

  it('navigates a run into Analyze', async () => {
    const onAnalyze = vi.fn()
    render(<ExperimentsPage onAnalyze={onAnalyze} onCompare={() => {}} onNotice={() => {}} />)
    const name = await screen.findByText('60 Hz vs 120 Hz')
    await act(async () => name.click())
    await waitFor(() => expect(h.getExperiment).toHaveBeenCalled())
    const buttons = await screen.findAllByText('Analyze')
    // The first "Analyze" button in the run table belongs to A1.
    await act(async () => buttons[0].click())
    expect(onAnalyze).toHaveBeenCalledWith('sessions/a1.jsonl', undefined, undefined)
  })

  it('compares paired runs', async () => {
    const onCompare = vi.fn()
    render(<ExperimentsPage onAnalyze={() => {}} onCompare={onCompare} onNotice={() => {}} />)
    const name = await screen.findByText('60 Hz vs 120 Hz')
    await act(async () => name.click())
    await waitFor(() => expect(h.getExperiment).toHaveBeenCalled())
    const compare = await screen.findAllByText('Compare pair')
    // The second paired row is B1; its partner is A1.
    await act(async () => compare[compare.length - 1].click())
    expect(onCompare).toHaveBeenCalledWith(
      { path: 'sessions/b1.jsonl', fromMs: undefined, toMs: undefined },
      { path: 'sessions/a1.jsonl', fromMs: undefined, toMs: undefined },
    )
  })

  it('resumes a running guided experiment instead of pretending it is complete', async () => {
    h.getExperiment.mockResolvedValue(
      record({
        status: 'running',
        repetitions: 2,
        guided: { phase: 'prepare', runIndex: 0, runStartedMs: null, treatmentConfirmed: false, confirmations: [] },
        runs: [
          { id: 'A1', label: 'A1', group: 'baseline', order: 0, pairId: 1, sessionPath: '', fromMs: null, toMs: null, included: true, source: 'guided', notes: '', capturedMs: null },
          { id: 'B1', label: 'B1', group: 'treatment', order: 1, pairId: 1, sessionPath: '', fromMs: null, toMs: null, included: true, source: 'guided', notes: '', capturedMs: null },
        ],
      }),
    )
    h.analyzeExperiment.mockResolvedValue(
      analysis({
        unavailableRuns: [
          { id: 'A1', label: 'A1', group: 'baseline', order: 0, sessionPath: '', reason: 'not recorded yet' },
          { id: 'B1', label: 'B1', group: 'treatment', order: 1, sessionPath: '', reason: 'not recorded yet' },
        ],
      }),
    )
    await openDetail()
    expect(await screen.findByText('Guided run')).toBeTruthy()
    expect(screen.getByText(/Run 1 of 2/)).toBeTruthy()
    expect(screen.getByText('Start run')).toBeTruthy()
  })
})
