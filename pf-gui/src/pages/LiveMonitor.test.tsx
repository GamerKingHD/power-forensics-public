import { render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import type { LiveEvidence } from '../api/types'
import type { AgentControls } from '../hooks/useAgentControls'

// The uPlot canvas is unavailable under jsdom; stub the chart and keep the
// test focused on the monitor's own controls, series list and event rail.
vi.mock('../components/chart/TimeSeriesChart', () => ({
  TimeSeriesChart: ({ label }: { label: string }) => <div data-testid="chart">{label}</div>,
}))

import { LiveMonitorPage } from './LiveMonitor'

function controls(): AgentControls {
  return {
    busy: false,
    pending: null,
    error: null,
    message: null,
    start: async () => {},
    pause: async () => {},
    resume: async () => {},
    stop: async () => {},
    marker: async () => {},
    clear: () => {},
  }
}

function snapshot(overrides: Partial<LiveEvidence>): LiveEvidence {
  return {
    agent: {
      available: true,
      running: true,
      paused: false,
      label: 'run',
      uptimeMs: 1000,
      markersAccepted: 0,
      markersDropped: 0,
    },
    generatedAt: 10_000,
    sessionState: 'recording',
    sessionOutsideDir: false,
    headline: [],
    gpus: [],
    system: { processCount: 0 },
    collectors: [
      {
        name: 'cpu',
        state: 'available',
        samples: 4,
        failures: 0,
        requiresAdmin: false,
        elevationWouldHelp: false,
      },
    ],
    panes: [
      {
        key: 'battery_discharge_w',
        label: 'Battery discharge',
        unit: 'W',
        points: [{ tMs: 9_000, value: 7.2, provenance: 'measured', quality: 'fresh' }],
      },
      {
        key: 'cpu_utility_pct',
        label: 'CPU utility',
        unit: '%',
        points: [{ tMs: 9_500, value: 12, provenance: 'measured', quality: 'fresh' }],
      },
    ],
    events: [
      { wallMs: 9_000, monoMs: 0, kind: 'marker', detail: 'lunch', severity: 'marker' },
      { wallMs: 9_200, monoMs: 0, kind: 'collector_timeout', detail: 'gpu', severity: 'warning' },
    ],
    markers: 1,
    ...overrides,
  }
}

describe('LiveMonitorPage', () => {
  it('lists series, renders an event rail and collector health', () => {
    render(<LiveMonitorPage live={snapshot({})} controls={controls()} />)
    expect(screen.getAllByText('Battery discharge').length).toBeGreaterThan(0)
    expect(screen.getAllByText('CPU utility').length).toBeGreaterThan(0)
    // Event rail chips carry both kind and detail (tooltip source text).
    expect(screen.getAllByText(/marker: lunch/).length).toBeGreaterThan(0)
    expect(screen.getAllByText(/collector_timeout: gpu/).length).toBeGreaterThan(0)
    // Collector health is present.
    expect(screen.getByText('cpu')).toBeTruthy()
    expect(screen.getByText('Available')).toBeTruthy()
  })

  it('shows a compact no-recording state instead of empty plots', () => {
    render(
      <LiveMonitorPage
        live={snapshot({ sessionState: 'none', session: null, panes: [] })}
        controls={controls()}
      />,
    )
    expect(screen.getByText('No active recording')).toBeTruthy()
    expect(screen.getByText('Start monitoring')).toBeTruthy()
    // The idle state must not mount any chart panes.
    expect(screen.queryByTestId('chart')).toBeNull()
  })

  it('reports an explicit state when no session is active', () => {
    render(
      <LiveMonitorPage
        live={snapshot({
          sessionState: 'outside-dir',
          sessionOutsideDir: true,
          sessionNote: 'Agent running — active session is outside configured GUI session directory',
          session: null,
        })}
        controls={controls()}
      />,
    )
    expect(screen.getByText(/outside configured GUI session directory/)).toBeTruthy()
  })

  it('treats an empty event list as none, not zero', () => {
    render(<LiveMonitorPage live={snapshot({ events: [] })} controls={controls()} />)
    expect(screen.getAllByText('No events recorded yet.').length).toBeGreaterThan(0)
  })
})
