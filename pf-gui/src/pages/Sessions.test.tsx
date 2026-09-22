import { render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'

const h = vi.hoisted(() => {
  class HoistedBridgeFailure extends Error {
    code = 'unknown'
    detail?: string
  }
  return {
    BridgeFailure: HoistedBridgeFailure,
    listSessions: vi.fn(),
  }
})

vi.mock('../api/bridge', () => ({
  BridgeFailure: h.BridgeFailure,
  isTauri: () => true,
  bridge: {
    listSessions: h.listSessions,
    recoverSession: vi.fn(),
    exportSession: vi.fn(),
  },
}))

// Render all rows without virtualization so the test needs no layout engine.
vi.mock('react-virtuoso', () => ({
  TableVirtuoso: ({
    data,
    itemContent,
    fixedHeaderContent,
  }: {
    data: unknown[]
    itemContent: (i: number, item: unknown) => ReactNode
    fixedHeaderContent?: () => ReactNode
  }) => (
    <table>
      <thead>{fixedHeaderContent ? fixedHeaderContent() : null}</thead>
      <tbody>
        {data.map((item, i) => (
          <tr key={i}>{itemContent(i, item)}</tr>
        ))}
      </tbody>
    </table>
  ),
}))

import { SessionsPage } from './Sessions'
import type { SessionListEntry } from '../api/types'

function entry(overrides: Partial<SessionListEntry>): SessionListEntry {
  return {
    file: 'pf-session-1.jsonl',
    path: 'sessions/pf-session-1.jsonl',
    label: 'x',
    note: '',
    startWallMs: 1000,
    endWallMs: 61000,
    durationS: 60,
    status: 'ok',
    recovered: false,
    mode: 'recorded',
    samples: 10,
    intervalMs: 1000,
    dischargeMedianW: 8.4,
    dischargeWh: 1.2,
    chargeWh: null,
    cpuMedianPct: 5,
    coveragePct: 98.7,
    discontinuities: 0,
    markers: 3,
    bytes: 2048,
    mtimeS: 100,
    ...overrides,
  }
}

describe('SessionsPage', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('renders discovered sessions with energy, coverage and markers', async () => {
    h.listSessions.mockResolvedValue([
      entry({ file: 'a.jsonl', note: 'work test' }),
      entry({ file: 'torn.jsonl', note: 'torn', status: 'incomplete', mode: 'incomplete', markers: 0 }),
    ])
    render(<SessionsPage onOpen={() => {}} onAnalyze={() => {}} onNotice={() => {}} />)
    expect(await screen.findByText('a.jsonl')).toBeTruthy()
    expect(screen.getByText('torn.jsonl')).toBeTruthy()
    expect(screen.getByText('work test')).toBeTruthy()
    expect(screen.getAllByText('1.200').length).toBe(2)
    expect(screen.getAllByText('98.7%').length).toBe(2)
    await waitFor(() => expect(h.listSessions).toHaveBeenCalled())
  })

  it('marks an incomplete session as recoverable', async () => {
    h.listSessions.mockResolvedValue([
      entry({ file: 'torn.jsonl', status: 'incomplete', mode: 'incomplete' }),
    ])
    render(<SessionsPage onOpen={() => {}} onAnalyze={() => {}} onNotice={() => {}} />)
    expect(await screen.findByText('Recover')).toBeTruthy()
    expect(screen.getAllByText('incomplete').length).toBeGreaterThan(0)
  })

  it('shows a recovered badge and collector set for a recovered session', async () => {
    h.listSessions.mockResolvedValue([
      entry({
        file: 'rec.jsonl',
        note: 'recovered run',
        recovered: true,
        mode: 'recovered',
        collectors: ['battery', 'cpu'],
      }),
    ])
    render(<SessionsPage onOpen={() => {}} onAnalyze={() => {}} onNotice={() => {}} />)
    expect(await screen.findByText('recovered')).toBeTruthy()
    expect(screen.getByText('battery, cpu')).toBeTruthy()
  })
})
