import { useEffect, useMemo, useState } from 'react'
import { ArrowLeft } from 'lucide-react'
import { BridgeFailure, bridge } from '../api/bridge'
import type { SessionSummary, SessionWindow } from '../api/types'
import { CollectorStateRow, EvidenceValue } from '../components/evidence'
import { TimeSeriesChart } from '../components/chart/TimeSeriesChart'
import { SplitPane } from '../components/layout/SplitPane'
import { formatDateTime, formatDuration } from '../lib/format'

interface Props {
  path: string
  onBack: () => void
  onAnalyze: (path: string) => void
  onCompare?: (path: string) => void
  onNotice: (msg: string | null) => void
}

type WindowKey = 'all' | '1m' | '5m' | '15m' | '30m'

const WINDOWS: { key: WindowKey; label: string; ms: number | null }[] = [
  { key: '1m', label: '1m', ms: 60_000 },
  { key: '5m', label: '5m', ms: 5 * 60_000 },
  { key: '15m', label: '15m', ms: 15 * 60_000 },
  { key: '30m', label: '30m', ms: 30 * 60_000 },
  { key: 'all', label: 'All / Session', ms: null },
]

function Row({ k, children }: { k: string; children: React.ReactNode }) {
  return (
    <>
      <div className="k">{k}</div>
      <div className="v">{children}</div>
    </>
  )
}

function pct(v: number | null | undefined): string {
  return v === null || v === undefined ? '—' : `${v.toFixed(1)}%`
}

export function SessionDetailsPage({ path, onBack, onAnalyze, onCompare, onNotice }: Props) {
  const [summary, setSummary] = useState<SessionSummary | null>(null)
  const [window_, setWindow] = useState<SessionWindow | null>(null)
  const [error, setError] = useState<BridgeFailure | null>(null)
  const [seriesKey, setSeriesKey] = useState('battery_discharge_w')
  const [windowKey, setWindowKey] = useState<WindowKey>('all')

  useEffect(() => {
    let cancelled = false
    setSummary(null)
    setWindow(null)
    setError(null)
    void (async () => {
      try {
        const s = await bridge.openSession(path)
        if (cancelled) return
        setSummary(s)
      } catch (e) {
        if (!cancelled) setError(e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e)))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [path])

  const range = useMemo<[number, number] | null>(() => {
    if (!summary) return null
    const ms = WINDOWS.find((w) => w.key === windowKey)?.ms
    if (!ms) return null
    const end = summary.endWallMs
    return [end - ms, end]
  }, [summary, windowKey])

  useEffect(() => {
    if (!summary) return
    let cancelled = false
    void (async () => {
      try {
        const w = await bridge.sessionWindow(path, {
          fromMs: range ? range[0] : 0,
          toMs: range ? range[1] : 0,
          maxPoints: 2000,
        })
        if (!cancelled) setWindow(w)
      } catch (e) {
        if (!cancelled) setError(e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e)))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [path, summary, range])

  const series = useMemo(
    () => window_?.series.find((s) => s.key === seriesKey) ?? window_?.series[0],
    [window_, seriesKey],
  )

  const exportCsv = async () => {
    try {
      const out = await bridge.exportSession(path)
      onNotice(`Exported: ${out}`)
    } catch (e) {
      const err = e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
      onNotice(`Export failed [${err.code}]: ${err.message}`)
    }
  }

  const stateLabel = summary?.recovered
    ? 'RECOVERED SESSION VIEW'
    : summary?.hasFooter
      ? 'RECORDED'
      : 'TORN / INCOMPLETE'

  return (
    <>
      <div className="page-title">
        <button onClick={onBack}>
          <ArrowLeft size={13} /> Sessions
        </button>
        <h1>{summary?.note || summary?.label || 'Session'}</h1>
        <span className="hint mono-sm">{path}</span>
        <span className="spacer" style={{ flex: 1 }} />
        <button className="primary" onClick={() => onAnalyze(path)}>
          Analyze session
        </button>
        {onCompare ? <button onClick={() => onCompare(path)}>Compare</button> : null}
        <button onClick={() => void exportCsv()}>Export CSV</button>
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
        </div>
      ) : null}

      {summary ? (
        <>
          <div className={`session-banner ${summary.recovered ? 'warn' : summary.hasFooter ? 'ok' : 'warn'}`}>
            <span
              className={`dot ${summary.recovered ? 'warn' : summary.hasFooter ? 'ok' : 'warn'}`}
              aria-hidden
            />
            <strong>{stateLabel}</strong>
            {summary.status !== 'ok' ? <span className="muted">{summary.status}</span> : null}
          </div>

          <div className="quality-strip">
            <span className="qs">
              Coverage <strong>{pct(summary.footer.dischargeCoveragePct)}</strong>
            </span>
            <span className="qs">
              Unknown <strong>{summary.footer.dischargeUnknownS ?? 0}s</strong>
            </span>
            <span className="qs">
              Unobserved <strong>{summary.footer.dischargeUnobservedS ?? 0}s</strong>
            </span>
            <span className="qs">
              Discontinuities <strong>{summary.footer.dischargeDiscontinuities ?? 0}</strong>
            </span>
            <span className="qs">
              Timeouts <strong>{summary.footer.collectorTimeouts ?? 0}</strong>
            </span>
            <span className="qs">
              Markers <strong>{summary.markers}</strong>
            </span>
            <span className="qs">
              Interval <strong>{summary.intervalMs} ms</strong>
            </span>
          </div>

          <SplitPane id="session-details" side="left" initial={420} min={320} max={720}>
            <div className="side-stack">
              <section className="panel">
                <header>Energy</header>
                <div className="kv">
                  <Row k="Discharge">
                    {summary.footer.dischargeWh === null ? '—' : `${summary.footer.dischargeWh.toFixed(3)} Wh`}
                  </Row>
                  <Row k="Charge">
                    {summary.footer.chargeWh === null ? '—' : `${summary.footer.chargeWh.toFixed(3)} Wh`}
                  </Row>
                  <Row k="Median discharge">
                    {summary.footer.dischargeMedianW === null
                      ? '—'
                      : `${summary.footer.dischargeMedianW.toFixed(2)} W`}
                  </Row>
                  <Row k="Started">{formatDateTime(summary.startWallMs)}</Row>
                  <Row k="Ended">{formatDateTime(summary.endWallMs)}</Row>
                  <Row k="Duration">{formatDuration(summary.durationS * 1000)}</Row>
                </div>
              </section>

              <section className="panel">
                <header>Battery</header>
                <div className="kv">
                  <Row k="State">{summary.battery?.state ?? '—'}</Row>
                  <Row k="Charge">
                    <EvidenceValue evidence={summary.battery?.pct} />
                  </Row>
                  <Row k="Discharge">
                    <EvidenceValue evidence={summary.battery?.discharge} />
                  </Row>
                  <Row k="Charge power">
                    <EvidenceValue evidence={summary.battery?.charge} />
                  </Row>
                  <Row k="Remaining">
                    <EvidenceValue evidence={summary.battery?.remainingWh} />
                  </Row>
                  <Row k="Health">
                    <EvidenceValue evidence={summary.battery?.healthPct} />
                  </Row>
                </div>
              </section>

              <section className="panel">
                <header>CPU / GPU / Display</header>
                <div className="kv">
                  <Row k="CPU utility">
                    <EvidenceValue evidence={summary.cpu?.utility} />
                  </Row>
                  <Row k="CPU package">
                    <EvidenceValue evidence={summary.cpu?.packagePower} />
                  </Row>
                  <Row k="CPU derived">
                    <EvidenceValue evidence={summary.cpu?.packageDerived} />
                  </Row>
                  {summary.gpus.length ? (
                    summary.gpus.map((g) => (
                      <Row key={g.name} k={g.name}>
                        <EvidenceValue evidence={g.utilization} compact /> {g.awake}
                      </Row>
                    ))
                  ) : (
                    <Row k="GPU">no adapter recorded</Row>
                  )}
                  <Row k="Display brightness">
                    {summary.display ? (
                      <EvidenceValue evidence={summary.display.brightness} />
                    ) : (
                      <span className="muted">no display sample recorded</span>
                    )}
                  </Row>
                </div>
              </section>

              <section className="panel">
                <header>Network / Storage</header>
                <div className="kv">
                  <Row k="Network RX">
                    <EvidenceValue evidence={summary.system.netRx} />
                  </Row>
                  <Row k="Network TX">
                    <EvidenceValue evidence={summary.system.netTx} />
                  </Row>
                  <Row k="Disk activity">
                    <EvidenceValue evidence={summary.system.storageActivity} />
                  </Row>
                </div>
              </section>

              <section className="panel">
                <header>Collector health</header>
                <div className="content flush">
                  <div className="list">
                    {summary.collectors.map((c) => (
                      <CollectorStateRow key={c.name} state={c} nowMs={summary.endWallMs} />
                    ))}
                  </div>
                </div>
              </section>
            </div>

            <div className="side-stack">
              <section className="panel">
                <header>
                  Timeline
                  <span className="spacer" />
                  <div className="pill-tabs">
                    {WINDOWS.map((w) => (
                      <button
                        key={w.key}
                        className={windowKey === w.key ? 'active' : ''}
                        onClick={() => setWindowKey(w.key)}
                      >
                        {w.label}
                      </button>
                    ))}
                  </div>
                  <select value={series?.key ?? ''} onChange={(e) => setSeriesKey(e.target.value)}>
                    {window_?.series.map((s) => (
                      <option key={s.key} value={s.key}>
                        {s.label}
                      </option>
                    ))}
                  </select>
                </header>
                <div className="content flush">
                  <TimeSeriesChart
                    points={series?.points ?? []}
                    unit={series?.unit ?? 'W'}
                    label={series?.label ?? ''}
                    height={280}
                  />
                </div>
                {window_?.downsampled ? (
                  <div className="muted mono-sm" style={{ padding: '4px 10px' }}>
                    downsampled to {window_.maxPoints} points (min/max spikes preserved)
                  </div>
                ) : null}
                <div className="event-rail">
                  {summary.events.length ? (
                    summary.events
                      .slice(-16)
                      .map((e, i) => (
                        <span
                          key={`${e.wallMs}-${i}`}
                          className={`event-chip ${e.severity}`}
                          title={`${formatDateTime(e.wallMs)} · ${e.kind}${e.detail ? `: ${e.detail}` : ''}`}
                        >
                          {formatDateTime(e.wallMs)} {e.kind}
                        </span>
                      ))
                  ) : (
                    <span className="muted">no events recorded</span>
                  )}
                </div>
              </section>

              <section className="panel">
                <header>Processes (last snapshot)</header>
                <div className="content flush">
                  <div className="list">
                    {summary.processes.length ? (
                      summary.processes.map((p) => (
                        <div className="row" key={`${p.pid}-${p.name}`}>
                          <span className="mono-sm" style={{ minWidth: 60 }}>
                            {p.pid}
                          </span>
                          <span style={{ flex: 1 }}>{p.name}</span>
                          <span className="mono-sm muted">
                            <EvidenceValue evidence={p.cpuPct} compact />
                          </span>
                        </div>
                      ))
                    ) : (
                      <div className="empty">No process snapshot recorded.</div>
                    )}
                  </div>
                </div>
              </section>

              <section className="panel">
                <header>Events ({summary.events.length})</header>
                <div className="content flush">
                  <div className="list">
                    {summary.events.length ? (
                      summary.events.map((e, i) => (
                        <div className="row" key={`${e.wallMs}-ev-${i}`}>
                          <span className="time">{formatDateTime(e.wallMs)}</span>
                          <span className={`event-chip ${e.severity}`}>{e.kind}</span>
                          <span className="muted">{e.detail}</span>
                        </div>
                      ))
                    ) : (
                      <div className="empty">No events recorded.</div>
                    )}
                  </div>
                </div>
              </section>
            </div>
          </SplitPane>
        </>
      ) : !error ? (
        <div className="empty">Loading session…</div>
      ) : null}
    </>
  )
}
