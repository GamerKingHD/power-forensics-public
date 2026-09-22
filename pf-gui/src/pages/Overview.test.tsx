import { render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import type { LiveEvidence } from '../api/types'
import type { AgentControls } from '../hooks/useAgentControls'

// Chart is a canvas component; capture the label it is asked to draw.
const chartLabels: string[] = []
vi.mock('../components/chart/TimeSeriesChart', () => ({
  TimeSeriesChart: ({ label }: { label: string }) => {
    chartLabels.push(label)
    return <div data-testid="chart">{label}</div>
  },
}))

import { OverviewPage } from './Overview'

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
    collectors: [],
    panes: [],
    events: [],
    markers: 0,
    ...overrides,
  }
}

describe('OverviewPage', () => {
  it('charts an available series instead of a legitimately empty battery series', () => {
    chartLabels.length = 0
    render(
      <OverviewPage
        live={snapshot({
          panes: [
            {
              key: 'battery_discharge_w',
              label: 'Battery discharge',
              unit: 'W',
              // On AC this series exists but every value is null.
              points: [
                { tMs: 9_000, value: null, provenance: 'unavailable', quality: 'unknown' },
                { tMs: 9_500, value: null, provenance: 'unavailable', quality: 'unknown' },
              ],
            },
            {
              key: 'cpu_pkg_power_w',
              label: 'CPU package (measured)',
              unit: 'W',
              points: [{ tMs: 9_500, value: 9.7, provenance: 'measured', quality: 'fresh' }],
            },
          ],
        })}
        error={null}
        controls={controls()}
        onStart={async () => {}}
        onOpenSession={() => {}}
      />,
    )
    expect(screen.getByTestId('chart').textContent).toContain('CPU package (measured)')
  })

  it('renders a deliberate no-recording state instead of empty telemetry boxes', () => {
    chartLabels.length = 0
    render(
      <OverviewPage
        live={snapshot({
          sessionState: 'none',
          agent: {
            available: false,
            running: false,
            paused: false,
            label: '',
            uptimeMs: 0,
            markersAccepted: 0,
            markersDropped: 0,
          },
          headline: [],
          panes: [],
        })}
        error={null}
        controls={controls()}
        onStart={async () => {}}
        onOpenSession={() => {}}
      />,
    )
    expect(screen.getByText('No active recording')).toBeTruthy()
    expect(screen.getByText('Start monitoring')).toBeTruthy()
    // No giant empty chart is drawn while idle.
    expect(chartLabels.length).toBe(0)
  })

  it('labels the bounded live-buffer count as recent, never as a session total', () => {
    render(
      <OverviewPage
        live={snapshot({
          session: {
            path: 'sessions/live.jsonl',
            label: 'run',
            intervalMs: 1000,
            startWallMs: 1_000,
            elapsedMs: 9_000,
            samples: 42,
            bytes: 2048,
            batterySamples: 7,
          },
          panes: [
            {
              key: 'battery_discharge_w',
              label: 'Battery discharge',
              unit: 'W',
              points: [{ tMs: 9_500, value: 6.2, provenance: 'measured', quality: 'fresh' }],
            },
          ],
        })}
        error={null}
        controls={controls()}
        onStart={async () => {}}
        onOpenSession={() => {}}
      />,
    )
    // The count is the recent-tail buffer, and the wording says so explicitly.
    expect(screen.getByText('Recent samples (live buffer)')).toBeTruthy()
    expect(screen.getByText(/recent samples in the live buffer/)).toBeTruthy()
  })
})
