import { useState } from 'react'
import { Activity, Stethoscope } from 'lucide-react'
import { BridgeFailure } from '../api/bridge'
import type { CapabilityReport, LiveEvidence } from '../api/types'
import { CollectorStateRow, EvidenceValue, MetricCell } from '../components/evidence'
import { TimeSeriesChart } from '../components/chart/TimeSeriesChart'
import { EmptyState } from '../components/layout/EmptyState'
import type { EmptyStateLine } from '../components/layout/EmptyState'
import type { AgentControls } from '../hooks/useAgentControls'
import { formatBytes, formatDateTime, formatDuration } from '../lib/format'

interface Props {
  live: LiveEvidence | null
  error: BridgeFailure | null
  controls: AgentControls
  onStart: (opts: {
    intervalMs?: number
    collectors?: string
    preset?: string
    note?: string
  }) => Promise<void>
  onOpenSession: (path: string) => void
  onRetry?: () => void
  capabilities?: CapabilityReport | null
  recentSession?: string | null
  onOpenDiagnostics?: () => void
}

function Row({ k, children }: { k: string; children: React.ReactNode }) {
  return (
    <>
      <div className="k">{k}</div>
      <div className="v">{children}</div>
    </>
  )
}

/** Explicit recording-lifecycle banner. The state word and shape carry the
 * meaning; color is only reinforcement. */
function SessionBanner({ live }: { live: LiveEvidence | null }) {
  const state = live?.sessionState ?? 'none'
  const label: Record<string, string> = {
    recording: 'RECORDING',
    paused: 'PAUSED',
    none: 'NO ACTIVE RECORDING',
    'outside-dir': 'RECORDING — OUTSIDE CONFIGURED DIRECTORY',
    ended: 'RECORDING ENDED',
    unreadable: 'RECORDING UNREADABLE',
  }
  const cls =
    state === 'recording' ? 'ok' : state === 'paused' || state === 'outside-dir' ? 'warn' : 'muted'
  return (
    <div className={`session-banner ${cls}`} role="status">
      <span className={`dot ${cls === 'ok' ? 'ok' : cls === 'warn' ? 'warn' : 'muted'}`} aria-hidden />
      <strong>{label[state] ?? state.toUpperCase()}</strong>
      {live?.sessionNote ? <span className="muted">{live.sessionNote}</span> : null}
    </div>
  )
}

const DOMAIN_LABELS: Record<string, string> = {
  battery: 'Battery',
  cpu: 'CPU',
  gpu: 'GPU',
  display: 'Display',
  net: 'Network',
  storage: 'Storage',
}

/**
 * Collector readiness for the idle state: what the machine can actually
 * report before a recording starts. Partial support is stated as a field
 * count rather than hidden behind a single word.
 */
function collectorReadiness(
  capabilities: CapabilityReport | null | undefined,
  live: LiveEvidence | null,
): EmptyStateLine[] {
  if (capabilities?.collectors.length) {
    return Object.entries(DOMAIN_LABELS).flatMap(([name, label]) => {
      const c = capabilities.collectors.find((x) => x.name === name)
      if (!c) return []
      const total = c.available + c.unavailable
      const value =
        c.state === 'available'
          ? total > 0 && c.unavailable > 0
            ? `Available · ${c.available} of ${total} fields`
            : 'Available'
          : c.state === 'degraded'
            ? `Degraded · ${c.available} of ${total} fields`
            : 'Unavailable'
      return [{ k: label, v: value }]
    })
  }
  if (live?.collectors.length) {
    const ready = live.collectors.filter((c) => c.state === 'available').length
    return [
      {
        k: 'Collectors ready',
        v: `${ready} / ${live.collectors.length}`,
      },
    ]
  }
  return [{ k: 'Collector readiness', v: 'agent not attached' }]
}

export function OverviewPage({
  live,
  error,
  controls,
  onStart,
  onOpenSession,
  onRetry,
  capabilities,
  recentSession,
  onOpenDiagnostics,
}: Props) {
  const [note, setNote] = useState('')
  const [intervalMs, setIntervalMs] = useState(1000)
  const [preset, setPreset] = useState('normal')
  const [markerText, setMarkerText] = useState('')

  const agent = live?.agent
  const session = live?.session
  const battery = live?.battery
  const cpu = live?.cpu
  const display = live?.display
  const system = live?.system
  const panes = live?.panes ?? []
  const hasValue = (p: { points: { value: number | null }[] }) =>
    p.points.some((pt) => pt.value !== null)
  // The headline chart must show something useful. Battery discharge is the
  // natural primary when discharging, but on AC that series is legitimately
  // empty; fall through to the first series that actually has values rather
  // than rendering a blank plot.
  const primary =
    panes.find((p) => p.key === 'battery_discharge_w' && hasValue(p)) ??
    panes.find(hasValue) ??
    panes.find((p) => p.key === 'battery_discharge_w') ??
    panes[0]

  const recording = live?.sessionState === 'recording' || live?.sessionState === 'paused'
  const paused = live?.sessionState === 'paused'

  const submitMarker = () => {
    const text = markerText.trim()
    if (!text) return
    void controls.marker(text)
    setMarkerText('')
  }

  if (!recording) {
    const description =
      live?.sessionNote ??
      (live?.sessionState === 'ended'
        ? 'The last recording has ended. Start a new recording to inspect live power and system activity.'
        : 'Start a recording to inspect live power and system activity.')
    return (
      <>
        <div className="page-title">
          <h1>Overview</h1>
          <span className="hint">What is the machine doing right now, and what evidence do we have?</span>
        </div>

        {error ? (
          <div className="error-box">
            <div className="code">{error.code}</div>
            <div>{error.message}</div>
            {error.detail ? (
              <details>
                <summary>Technical detail</summary>
                <pre className="mono-sm">{error.detail}</pre>
              </details>
            ) : null}
            {onRetry ? (
              <div style={{ marginTop: 8 }}>
                <button onClick={onRetry}>Retry connection</button>
              </div>
            ) : null}
          </div>
        ) : null}

        <section className="panel">
          <div className="content">
            <EmptyState
              title="No active recording"
              description={description}
              actions={
                <>
                  <input
                    placeholder="session label (optional)"
                    value={note}
                    onChange={(e) => setNote(e.target.value)}
                    style={{ width: 200 }}
                  />
                  <label className="muted check">
                    interval
                    <select value={intervalMs} onChange={(e) => setIntervalMs(Number(e.target.value))}>
                      <option value={250}>250 ms</option>
                      <option value={500}>500 ms</option>
                      <option value={1000}>1000 ms</option>
                      <option value={2000}>2000 ms</option>
                      <option value={5000}>5000 ms</option>
                    </select>
                  </label>
                  <label className="muted check">
                    preset
                    <select value={preset} onChange={(e) => setPreset(e.target.value)}>
                      <option value="normal">normal</option>
                      <option value="low">low</option>
                      <option value="deep">deep</option>
                    </select>
                  </label>
                  <button
                    className="primary"
                    disabled={controls.busy}
                    onClick={() => void onStart({ note: note.trim() || undefined, intervalMs, preset })}
                  >
                    <Activity size={14} /> {controls.busy ? 'Starting…' : 'Start monitoring'}
                  </button>
                </>
              }
              details={collectorReadiness(capabilities, live)}
            />
          </div>
        </section>

        {controls.error ? (
          <div className="error-box" style={{ marginTop: 12 }}>
            <div className="code">{controls.error.code}</div>
            <div>{controls.error.message}</div>
          </div>
        ) : null}

        {recentSession || onOpenDiagnostics ? (
          <div className="overview-links">
            {recentSession ? (
              <button className="ghost" onClick={() => onOpenSession(recentSession)}>
                Open last session — {recentSession.split(/[\\/]/).pop()}
              </button>
            ) : null}
            {onOpenDiagnostics ? (
              <button className="ghost" onClick={onOpenDiagnostics}>
                <Stethoscope size={13} /> Diagnostics
              </button>
            ) : null}
          </div>
        ) : null}
      </>
    )
  }

  return (
    <>
      <div className="page-title">
        <h1>Overview</h1>
        <span className="hint">What is the machine doing right now, and what evidence do we have?</span>
      </div>

      <SessionBanner live={live} />

      {error ? (
        <div className="error-box">
          <div className="code">{error.code}</div>
          <div>{error.message}</div>
          {error.detail ? (
            <details>
              <summary>Technical detail</summary>
              <pre className="mono-sm">{error.detail}</pre>
            </details>
          ) : null}
          {onRetry ? (
            <div style={{ marginTop: 8 }}>
              <button onClick={onRetry}>Retry connection</button>
            </div>
          ) : null}
        </div>
      ) : null}

      <div className="headline" style={{ marginBottom: 12 }}>
        {(live?.headline ?? []).map((m) => (
          <MetricCell key={m.key} metric={m} />
        ))}
        {!live ? <div className="cell">Waiting for the agent…</div> : null}
      </div>

      <div className="overview-grid">
        <section className="panel span-2">
          <header>
            {primary?.label ?? 'Power'} — recent samples in the live buffer ({primary?.points.length ?? 0})
            <span className="spacer" />
            <span className="muted inline-note">gaps are unavailable readings, not zero</span>
          </header>
          <div className="content flush">
            <TimeSeriesChart
              points={primary?.points ?? []}
              unit={primary?.unit ?? 'W'}
              label={primary?.label ?? 'Battery discharge'}
              height={200}
            />
          </div>
        </section>

        <section className="panel">
          <header>Current operating state</header>
          <div className="kv">
            <Row k="AC / battery">
              {battery?.state ?? '—'}
              {system?.ac ? (
                <>
                  {' '}
                  <EvidenceValue evidence={system.ac} compact />
                </>
              ) : null}
            </Row>
            <Row k="Battery charge">
              <EvidenceValue evidence={battery?.pct} />
            </Row>
            <Row k="Discharge power">
              <EvidenceValue evidence={battery?.discharge} />
            </Row>
            <Row k="Charge power">
              <EvidenceValue evidence={battery?.charge} />
            </Row>
            <Row k="Remaining energy">
              <EvidenceValue evidence={battery?.remainingWh} />
            </Row>
            <Row k="CPU utility">
              <EvidenceValue evidence={cpu?.utility} />
            </Row>
            <Row k="CPU package power">
              <EvidenceValue evidence={cpu?.packagePower} />
              {cpu?.packageDerived ? (
                <div>
                  <span className="muted mono-sm">derived cross-check: </span>
                  <EvidenceValue evidence={cpu.packageDerived} compact />
                </div>
              ) : null}
            </Row>
            <Row k="GPU">
              {live?.gpus.length ? (
                live.gpus.map((g) => (
                  <div key={g.name} style={{ marginBottom: 2 }}>
                    <span className="mono-sm">{g.name}</span>{' '}
                    <EvidenceValue evidence={g.utilization} compact />
                  </div>
                ))
              ) : (
                <span className="muted">no adapter reported</span>
              )}
            </Row>
            <Row k="Display">
              <EvidenceValue evidence={display?.brightness} />
            </Row>
            <Row k="Power scheme">{system?.scheme ?? '—'}</Row>
            <Row k="Foreground process">{system?.foregroundProcess ?? '—'}</Row>
          </div>
        </section>

        <section className="panel">
          <header>
            Collector / evidence health
            <span className="spacer" />
            <span className="muted inline-note">failures and staleness are explicit</span>
          </header>
          <div className="content flush">
            <div className="list">
              {(live?.collectors ?? []).map((c) => (
                <CollectorStateRow key={c.name} state={c} nowMs={live?.generatedAt} />
              ))}
              {!live?.collectors.length ? <div className="empty">No collector evidence yet.</div> : null}
            </div>
          </div>
        </section>

        <section className="panel">
          <header>
            Active session
            <span className="spacer" />
            {session ? <span className="mono-sm muted">{session.path}</span> : null}
          </header>
          <div className="content">
            <div className="kv" style={{ marginBottom: 12 }}>
              <Row k="Label">{session?.label || agent?.label || 'agent'}</Row>
              <Row k="Duration">{formatDuration(agent?.uptimeMs ?? 0)}</Row>
              <Row k="Recent samples (live buffer)">{session?.samples ?? 0}</Row>
              <Row k="Markers">{agent?.markersAccepted ?? 0}</Row>
              <Row k="Interval">{session?.intervalMs ?? '—'} ms</Row>
              <Row k="File size">{formatBytes(session?.bytes ?? 0)}</Row>
              <Row k="Started">{session ? formatDateTime(session.startWallMs) : '—'}</Row>
            </div>
            <div className="toolbar" style={{ marginBottom: 0 }}>
              {paused ? (
                <button className="primary" disabled={controls.busy} onClick={() => void controls.resume()}>
                  {controls.pending === 'resume' ? 'Resuming…' : 'Resume'}
                </button>
              ) : (
                <button disabled={controls.busy} onClick={() => void controls.pause()}>
                  {controls.pending === 'pause' ? 'Pausing…' : 'Pause'}
                </button>
              )}
              <input
                placeholder="marker text"
                value={markerText}
                onChange={(e) => setMarkerText(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') submitMarker()
                }}
                style={{ width: 200 }}
              />
              <button disabled={controls.busy || !markerText.trim()} onClick={submitMarker}>
                {controls.pending === 'marker' ? 'Adding…' : 'Add marker'}
              </button>
              <button className="danger" disabled={controls.busy} onClick={() => void controls.stop()}>
                {controls.pending === 'stop' ? 'Stopping…' : 'Stop session'}
              </button>
              {session ? (
                <button disabled={controls.busy} onClick={() => onOpenSession(session.path)}>
                  Open session
                </button>
              ) : null}
            </div>

            {controls.error ? (
              <div className="error-box" style={{ marginTop: 12 }}>
                <div className="code">{controls.error.code}</div>
                <div>{controls.error.message}</div>
                {controls.error.detail ? (
                  <details>
                    <summary>Technical detail</summary>
                    <pre className="mono-sm">{controls.error.detail}</pre>
                  </details>
                ) : null}
              </div>
            ) : null}
            {controls.message ? (
              <div className="muted mono-sm" style={{ marginTop: 8 }}>
                {controls.message}
              </div>
            ) : null}
          </div>
        </section>
      </div>
    </>
  )
}
