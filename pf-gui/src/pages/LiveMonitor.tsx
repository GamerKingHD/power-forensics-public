import { useEffect, useMemo, useRef, useState } from 'react'
import { Activity, Plus } from 'lucide-react'
import type { LiveEvidence, TimelinePoint } from '../api/types'
import { TimeSeriesChart } from '../components/chart/TimeSeriesChart'
import type { CursorInfo } from '../components/chart/TimeSeriesChart'
import { CollectorStateRow, EvidenceBadge, QualityIndicator } from '../components/evidence'
import { EmptyState } from '../components/layout/EmptyState'
import type { EmptyStateLine } from '../components/layout/EmptyState'
import { SplitPane } from '../components/layout/SplitPane'
import type { AgentControls } from '../hooks/useAgentControls'
import { displayUnit, formatAge, formatClock, formatDateTime, formatNumber } from '../lib/format'

interface Props {
  live: LiveEvidence | null
  controls: AgentControls
}

type WindowKey = 'all' | '1m' | '5m' | '15m' | '30m'

const WINDOWS: { key: WindowKey; label: string; ms: number | null }[] = [
  { key: '1m', label: '1m', ms: 60_000 },
  { key: '5m', label: '5m', ms: 5 * 60_000 },
  { key: '15m', label: '15m', ms: 15 * 60_000 },
  { key: '30m', label: '30m', ms: 30 * 60_000 },
  { key: 'all', label: 'All', ms: null },
]

const PREFERRED = ['battery_discharge_w', 'cpu_pkg_derived_w', 'gpu_util_pct', 'cpu_utility_pct']

export function LiveMonitorPage({ live, controls }: Props) {
  const panes = live?.panes ?? []

  // The Rust live tail is bounded by bytes, so each poll returns only the most
  // recent few ticks. Accumulate a bounded rolling history in the client so the
  // 1m/5m/15m/30m window buttons select real recorded time instead of zooming a
  // handful of seconds. The forensic values are unchanged; only the retained
  // window grows. Overlap between polls is dropped by timestamp.
  const HISTORY_CAP = 4000
  const historyRef = useRef(new Map<string, TimelinePoint[]>())
  const [historyTick, setHistoryTick] = useState(0)
  useEffect(() => {
    if (!panes.length) return
    let changed = false
    for (const p of panes) {
      const arr = historyRef.current.get(p.key) ?? []
      for (const pt of p.points) {
        const last = arr.at(-1)
        if (!last || pt.tMs > last.tMs) {
          arr.push(pt)
          changed = true
        }
      }
      if (arr.length > HISTORY_CAP) arr.splice(0, arr.length - HISTORY_CAP)
      historyRef.current.set(p.key, arr)
    }
    if (changed) setHistoryTick((v) => v + 1)
  }, [panes])

  const shownPanes = useMemo(
    () => panes.map((p) => ({ ...p, points: historyRef.current.get(p.key) ?? p.points })),
    // historyTick forces a refresh when the buffer grows.
    [panes, historyTick],
  )
  const byKey = useMemo(() => new Map(shownPanes.map((p) => [p.key, p])), [shownPanes])
  const [selected, setSelected] = useState<string[]>([])
  const [windowKey, setWindowKey] = useState<WindowKey>('5m')
  const [markerText, setMarkerText] = useState('')
  const [cursor, setCursor] = useState<{ key: string; info: CursorInfo } | null>(null)

  // Seed the selection once series arrive, preferring power-relevant domains.
  useEffect(() => {
    if (!panes.length) return
    setSelected((prev) => (prev.length ? prev : PREFERRED.filter((k) => byKey.has(k)).slice(0, 3)))
  }, [panes, byKey])

  const events = live?.events ?? []
  const agent = live?.agent

  const toggle = (key: string) =>
    setSelected((prev) => (prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]))

  const lastT = useMemo(
    () => shownPanes.reduce((acc, p) => Math.max(acc, p.points.at(-1)?.tMs ?? 0), 0),
    [shownPanes],
  )
  const range = useMemo<[number, number] | null>(() => {
    const w = WINDOWS.find((x) => x.key === windowKey)?.ms
    if (!w || !lastT) return null
    return [lastT - w, lastT]
  }, [windowKey, lastT])

  const submitMarker = () => {
    const text = markerText.trim()
    if (!text) return
    void controls.marker(text)
    setMarkerText('')
  }

  const cursorSeries = cursor ? byKey.get(cursor.key) : undefined

  const recording = live?.sessionState === 'recording' || live?.sessionState === 'paused'

  if (!recording) {
    const ready = (live?.collectors ?? []).filter((c) => c.state === 'available').length
    const total = live?.collectors.length ?? 0
    const metrics = shownPanes.slice(0, 6).map((p) => p.label)
    const details: EmptyStateLine[] = []
    details.push({ k: 'Collectors ready', v: total ? `${ready} / ${total}` : 'agent not attached' })
    if (metrics.length) details.push({ k: 'Available metrics', v: metrics.join(' · ') })
    return (
      <>
        <div className="page-title">
          <h1>Live Monitor</h1>
          <span className="hint">
            Independently sampled domains are shown separately. Gaps are unavailable samples, never zero.
          </span>
        </div>
        <section className="panel">
          <div className="content">
            <EmptyState
              title="No active recording"
              description={
                live?.sessionNote ??
                'Start a recording to watch independently sampled power and system series arrive in real time.'
              }
              actions={
                <button
                  className="primary"
                  disabled={controls.busy}
                  onClick={() => void controls.start({})}
                >
                  <Activity size={14} /> {controls.busy ? 'Starting…' : 'Start monitoring'}
                </button>
              }
              details={details}
            />
          </div>
        </section>
        {controls.error ? (
          <div className="error-box" style={{ marginTop: 12 }}>
            <div className="code">{controls.error.code}</div>
            <div>{controls.error.message}</div>
          </div>
        ) : null}
      </>
    )
  }

  return (
    <>
      <div className="page-title">
        <h1>Live Monitor</h1>
        <span className="hint">
          Independently sampled domains are shown separately. Gaps are unavailable samples, never zero.
        </span>
      </div>

      <div className="toolbar">
        <span className="muted">Window</span>
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
        <span className="spacer" />
        {live?.sessionState === 'recording' || live?.sessionState === 'paused' ? (
          <>
            <input
              placeholder="marker text"
              value={markerText}
              onChange={(e) => setMarkerText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') submitMarker()
              }}
              style={{ width: 180 }}
            />
            <button disabled={controls.busy || !markerText.trim()} onClick={submitMarker}>
              <Plus size={13} /> Marker
            </button>
          </>
        ) : (
          <span className="muted">
            {live?.sessionNote ?? 'Start monitoring from Overview to record a session.'}
          </span>
        )}
      </div>

      <SplitPane id="live" side="right" initial={320} min={240} max={620}>
        <div className="chart-stack">
          {selected.length ? (
            selected.map((key) => {
              const s = byKey.get(key)
              if (!s) return null
              return (
                <section className="panel" key={key}>
                  <header>
                    {s.label}
                    <span className="muted inline-note"> {s.unit}</span>
                    <span className="spacer" />
                    <span className="muted mono-sm">{s.points.length} recent samples</span>
                    <button className="ghost" onClick={() => toggle(key)} title="Hide series">
                      Hide
                    </button>
                  </header>
                  <div className="content flush">
                    <TimeSeriesChart
                      points={s.points}
                      unit={s.unit}
                      label={s.label}
                      height={selected.length > 2 ? 150 : 200}
                      range={range}
                      onCursor={(info) => setCursor(info ? { key, info } : null)}
                    />
                  </div>
                </section>
              )
            })
          ) : (
            <section className="panel">
              <div className="empty">Select one or more series from the panel on the right.</div>
            </section>
          )}

          <section className="panel">
            <header>
              Timeline
              <span className="spacer" />
              <span className="muted inline-note">markers, timeouts, recovery, discontinuity</span>
            </header>
            <div className="event-rail">
              {events.length ? (
                events
                  .slice(-16)
                  .map((e, i) => (
                    <span
                      key={`${e.wallMs}-${i}`}
                      className={`event-chip ${e.severity}`}
                      title={`${formatDateTime(e.wallMs)} · ${e.kind}${e.detail ? `: ${e.detail}` : ''}`}
                    >
                      {formatClock(e.wallMs)} {e.kind}
                      {e.detail ? `: ${e.detail}` : ''}
                    </span>
                  ))
              ) : (
                <span className="muted">none in the recent window</span>
              )}
            </div>
          </section>
        </div>

        <div className="side-stack">
          <section className="panel">
            <header>Series</header>
            <div className="content flush">
              <div className="list">
                {shownPanes.map((p) => (
                  <label className="row toggle" key={p.key}>
                    <input
                      type="checkbox"
                      checked={selected.includes(p.key)}
                      onChange={() => toggle(p.key)}
                    />
                    <span style={{ flex: 1 }}>{p.label}</span>
                    <span className="muted mono-sm">{p.unit}</span>
                  </label>
                ))}
                {!panes.length ? <div className="empty">No series yet.</div> : null}
              </div>
            </div>
          </section>

          <section className="panel">
            <header>Cursor</header>
            <div className="content">
              {cursor && cursorSeries ? (
                <div className="kv">
                  <div className="k">Time</div>
                  <div className="v">{formatDateTime(cursor.info.tMs)}</div>
                  <div className="k">Metric</div>
                  <div className="v">{cursorSeries.label}</div>
                  <div className="k">Value</div>
                  <div className="v">
                    {cursor.info.value === null
                      ? '— unavailable'
                      : `${formatNumber(cursor.info.value, cursorSeries.unit)}${
                          displayUnit(cursorSeries.unit) ? ` ${displayUnit(cursorSeries.unit)}` : ''
                        }`}
                  </div>
                  <div className="k">Provenance</div>
                  <div className="v">
                    <EvidenceBadge provenance={cursor.info.provenance} quality={cursor.info.quality} />
                    <QualityIndicator quality={cursor.info.quality} />
                  </div>
                  <div className="k">Age</div>
                  <div className="v">
                    {live?.generatedAt ? formatAge(Math.max(0, live.generatedAt - cursor.info.tMs)) : '—'}
                  </div>
                </div>
              ) : (
                <div className="muted mono-sm">Hover a chart to inspect a sample.</div>
              )}
            </div>
          </section>

          <section className="panel">
            <header>
              Collector health
              <span className="spacer" />
              {agent?.running ? <span className="badge recording">live</span> : null}
            </header>
            <div className="content flush">
              <div className="list">
                {(live?.collectors ?? []).map((c) => (
                  <CollectorStateRow key={c.name} state={c} nowMs={live?.generatedAt} />
                ))}
              </div>
            </div>
          </section>

          <section className="panel">
            <header>Recent events</header>
            <div className="content flush">
              <div className="list">
                {events.length ? (
                  [...events].reverse().map((e, i) => (
                    <div className="row" key={`${e.wallMs}-rev-${i}`}>
                      <span className="time">{formatClock(e.wallMs)}</span>
                      <span className={`badge ${e.severity === 'marker' ? 'marker' : e.severity}`}>
                        {e.kind}
                      </span>
                      <span className="muted mono-sm">{e.detail}</span>
                    </div>
                  ))
                ) : (
                  <div className="empty">No events recorded yet.</div>
                )}
              </div>
            </div>
          </section>
        </div>
      </SplitPane>
    </>
  )
}
