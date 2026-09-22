import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Search } from 'lucide-react'
import { BridgeFailure, bridge } from '../api/bridge'
import type {
  AnalyzeOverview,
  RangeAnalysis,
  Series,
  TimelineEvent,
  TimelinePoint,
} from '../api/types'
import { usePolling } from '../hooks/usePolling'
import { TimelinePane } from '../components/analyze/TimelinePane'
import { RangeQualityStrip, SessionStatsStrip } from '../components/analyze/QualityStrip'
import {
  ChangesPanel,
  CorrelationsPanel,
  DomainsPanel,
  EnergyPanel,
} from '../components/analyze/EvidencePanels'
import { ProcessPanel } from '../components/analyze/ProcessPanel'
import { DetailsInspector, type CursorReading } from '../components/analyze/DetailsInspector'
import { EventRail, RegionList } from '../components/analyze/AnalyzeNav'
import { SplitPane } from '../components/layout/SplitPane'
import { ReportExportBar } from '../components/report/ReportExport'
import { QUICK_WINDOWS, TRACKS } from '../lib/analyze'
import { formatDateTime, formatDuration } from '../lib/format'

interface Props {
  sessionPath: string | null
  onSelectSession: (path: string | null) => void
  onOpenDetails: (path: string) => void
  onCompareRange?: (path: string, fromMs: number, toMs: number) => void
  onCompareSession?: (path: string) => void
  onNotice?: (msg: string | null) => void
  defaultRedact?: boolean
  onBrowseSessions?: () => void
}

interface SavedView {
  viewport: [number, number] | null
  selection: [number, number] | null
  pid: number | null
}

function loadSaved(path: string): SavedView | null {
  try {
    const raw = sessionStorage.getItem(`pf.analyze.${path}`)
    if (!raw) return null
    const v = JSON.parse(raw) as SavedView
    return { viewport: v.viewport ?? null, selection: v.selection ?? null, pid: v.pid ?? null }
  } catch {
    return null
  }
}

function toFailure(e: unknown): BridgeFailure {
  return e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
}

function nearestPoint(points: TimelinePoint[], tMs: number): TimelinePoint | null {
  if (points.length === 0) return null
  let lo = 0
  let hi = points.length - 1
  while (lo < hi) {
    const mid = (lo + hi) >> 1
    if (points[mid].tMs < tMs) lo = mid + 1
    else hi = mid
  }
  const a = points[lo]
  const b = points[Math.max(0, lo - 1)]
  return Math.abs(a.tMs - tMs) <= Math.abs(b.tMs - tMs) ? a : b
}

function AnalyzePicker({
  onSelect,
  onBrowseSessions,
}: {
  onSelect: (path: string) => void
  onBrowseSessions?: () => void
}) {
  const sessions = usePolling(bridge.listSessions, 5000)
  const recent = useMemo(
    () => [...(sessions.data ?? [])].sort((a, b) => b.startWallMs - a.startWallMs).slice(0, 8),
    [sessions.data],
  )
  return (
    <div className="analyze-picker">
      <div className="page-title">
        <h1>Analyze</h1>
        <span className="hint">Choose a recording to investigate.</span>
        <span className="spacer" style={{ flex: 1 }} />
        {onBrowseSessions ? (
          <button onClick={onBrowseSessions}>Browse all sessions</button>
        ) : null}
      </div>
      {sessions.error ? (
        <div className="error-box">
          <div className="code">{sessions.error.code}</div>
          <div>{sessions.error.message}</div>
        </div>
      ) : null}
      <section className="panel">
        <header>
          Recent recordings
          <span className="spacer" />
          <span className="muted inline-note">Select a recording explicitly — Analyze never opens the newest by default.</span>
        </header>
        <div className="content flush">
          <div className="list">
            {recent.map((s) => (
              <div className="row analyze-pick-row" key={s.path}>
                <span
                  className={`badge ${
                    s.recovered ? 'derived' : s.status === 'ok' ? 'available' : 'degraded'
                  }`}
                >
                  {s.mode}
                </span>
                <span className="name" style={{ flex: 1 }}>
                  {s.note || s.file}
                </span>
                <span className="muted mono-sm">{formatDateTime(s.startWallMs)}</span>
                <span className="muted mono-sm">{formatDuration(s.durationS * 1000)}</span>
                <span className="muted mono-sm">
                  {s.coveragePct === null ? '—' : `${s.coveragePct.toFixed(1)}%`}
                </span>
                <button className="primary" onClick={() => onSelect(s.path)}>
                  <Search size={12} /> Analyze
                </button>
              </div>
            ))}
            {sessions.data && sessions.data.length === 0 ? (
              <div className="empty">No recordings yet. Start a recording from Overview.</div>
            ) : null}
            {sessions.data && sessions.data.length > recent.length ? (
              <div className="row">
                <span className="muted mono-sm">
                  {sessions.data.length - recent.length} older recording(s) — browse all sessions to see them.
                </span>
              </div>
            ) : null}
          </div>
        </div>
      </section>
    </div>
  )
}

export function AnalyzePage({
  sessionPath,
  onSelectSession,
  onOpenDetails,
  onCompareRange,
  onCompareSession,
  onNotice,
  defaultRedact = false,
  onBrowseSessions,
}: Props) {
  const [overview, setOverview] = useState<AnalyzeOverview | null>(null)
  const [overviewError, setOverviewError] = useState<BridgeFailure | null>(null)
  const [loadingOverview, setLoadingOverview] = useState(false)

  const [viewport, setViewport] = useState<[number, number] | null>(null)
  const [selection, setSelection] = useState<[number, number] | null>(null)
  const [enabled, setEnabled] = useState<Set<string>>(
    () => new Set(TRACKS.filter((t) => t.defaultEnabled).map((t) => t.key)),
  )
  const [cursorMs, setCursorMs] = useState<number | null>(null)
  const [activeEvent, setActiveEvent] = useState<TimelineEvent | null>(null)
  const [pid, setPid] = useState<number | null>(null)
  const [displaySeries, setDisplaySeries] = useState<Series[]>([])
  const [analysis, setAnalysis] = useState<RangeAnalysis | null>(null)
  const [analysisLoading, setAnalysisLoading] = useState(false)
  const [windowError, setWindowError] = useState<BridgeFailure | null>(null)
  const windowToken = useRef(0)
  const rangeToken = useRef(0)

  useEffect(() => {
    if (!sessionPath) {
      setOverview(null)
      return
    }
    let cancelled = false
    setLoadingOverview(true)
    setOverviewError(null)
    setOverview(null)
    setSelection(null)
    setViewport(null)
    setPid(null)
    setCursorMs(null)
    setActiveEvent(null)
    setAnalysis(null)
    setDisplaySeries([])
    bridge
      .analyzeOverview(sessionPath)
      .then((o) => {
        if (cancelled) return
        setOverview(o)
        setDisplaySeries(o.series)
        setLoadingOverview(false)
        // Restore this session's investigation state if the user briefly
        // navigated away; a bare reset would lose their viewport/selection.
        const saved = loadSaved(sessionPath)
        if (saved) {
          if (saved.viewport) setViewport(saved.viewport)
          if (saved.selection) setSelection(saved.selection)
          if (typeof saved.pid === 'number') setPid(saved.pid)
        }
      })
      .catch((e) => {
        if (cancelled) return
        setOverviewError(toFailure(e))
        setLoadingOverview(false)
      })
    return () => {
      cancelled = true
    }
  }, [sessionPath])

  // Bounded window fetch for a zoomed viewport or a selected process. The
  // request is padded and debounced; obsolete responses are ignored.
  useEffect(() => {
    if (!overview || !sessionPath) return
    if (viewport === null && pid === null) {
      setDisplaySeries(overview.series)
      return
    }
    const token = ++windowToken.current
    const from = viewport ? viewport[0] : overview.startWallMs
    const to = viewport ? viewport[1] : overview.endWallMs
    const pad = Math.max(30_000, to - from)
    const t = setTimeout(() => {
      bridge
        .sessionWindow(sessionPath, {
          fromMs: Math.max(0, from - pad),
          toMs: to + pad,
          maxPoints: 1600,
          pid: pid ?? undefined,
        })
        .then((w) => {
          if (token !== windowToken.current) return
          setDisplaySeries(w.series)
          setWindowError(null)
        })
        .catch((e) => {
          if (token !== windowToken.current) return
          setWindowError(toFailure(e))
        })
    }, 120)
    return () => clearTimeout(t)
  }, [overview, viewport, pid, sessionPath])

  // Range analysis: debounced, backend-only, obsolete responses ignored.
  useEffect(() => {
    if (!selection || !overview || !sessionPath) {
      setAnalysis(null)
      setAnalysisLoading(false)
      return
    }
    const token = ++rangeToken.current
    setAnalysisLoading(true)
    const t = setTimeout(() => {
      bridge
        .analyzeRange(sessionPath, selection[0], selection[1])
        .then((a) => {
          if (token !== rangeToken.current) return
          setAnalysis(a)
          setAnalysisLoading(false)
        })
        .catch(() => {
          if (token !== rangeToken.current) return
          setAnalysis(null)
          setAnalysisLoading(false)
        })
    }, 120)
    return () => clearTimeout(t)
  }, [selection, overview, sessionPath])

  useEffect(() => {
    if (!sessionPath || !overview) return
    try {
      sessionStorage.setItem(
        `pf.analyze.${sessionPath}`,
        JSON.stringify({ viewport, selection, pid }),
      )
    } catch {
      // Persistence is best-effort; never fail rendering over it.
    }
  }, [sessionPath, overview, viewport, selection, pid])

  const seriesByKey = useMemo(() => {
    const m = new Map<string, Series>()
    for (const s of displaySeries) m.set(s.key, s)
    return m
  }, [displaySeries])

  const sessionBounds = useMemo<[number, number] | null>(() => {
    if (!overview) return null
    return [overview.startWallMs, overview.endWallMs]
  }, [overview])

  const jumpTo = useCallback(
    (center: number) => {
      if (!overview) return
      const span = viewport ? viewport[1] - viewport[0] : 60_000
      const half = Math.min(Math.max(span / 2, 15_000), 120_000)
      let from = center - half
      let to = center + half
      if (from < overview.startWallMs) {
        from = overview.startWallMs
        to = Math.min(overview.endWallMs, from + 2 * half)
      }
      if (to > overview.endWallMs) {
        to = overview.endWallMs
        from = Math.max(overview.startWallMs, to - 2 * half)
      }
      setViewport([from, to])
    },
    [overview, viewport],
  )

  const pickEvent = useCallback(
    (e: TimelineEvent) => {
      setActiveEvent(e)
      jumpTo(e.wallMs)
    },
    [jumpTo],
  )

  const jumpRegion = useCallback(
    (startMs: number, endMs: number) => {
      if (!overview) return
      const pad = Math.max(5_000, (endMs - startMs) * 0.5)
      const from = Math.max(overview.startWallMs, startMs - pad)
      const to = Math.min(overview.endWallMs, endMs + pad)
      setViewport([from, to])
      setSelection([startMs, endMs])
      setActiveEvent(null)
    },
    [overview],
  )

  const toggleTrack = useCallback((key: string) => {
    setEnabled((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  }, [])

  const cursorReadings = useMemo<CursorReading[]>(() => {
    if (cursorMs === null) return []
    const out: CursorReading[] = []
    for (const track of TRACKS) {
      if (!enabled.has(track.key)) continue
      const series = seriesByKey.get(track.seriesKey)
      if (!series) continue
      const p = nearestPoint(series.points, cursorMs)
      if (!p) continue
      out.push({
        label: track.label,
        unit: track.unit,
        value: p.value,
        provenance: p.provenance,
        quality: p.quality,
        tMs: p.tMs,
      })
    }
    if (pid !== null) {
      const series = seriesByKey.get(`proc_pid_${pid}_cpu_pct`)
      const p = series ? nearestPoint(series.points, cursorMs) : null
      if (p) {
        out.push({
          label: `Process ${pid} CPU`,
          unit: '%',
          value: p.value,
          provenance: p.provenance,
          quality: p.quality,
          tMs: p.tMs,
        })
      }
    }
    return out
  }, [cursorMs, enabled, seriesByKey, pid])

  if (!sessionPath) return <AnalyzePicker onSelect={onSelectSession} onBrowseSessions={onBrowseSessions} />

  const enabledTracks = TRACKS.filter((t) => enabled.has(t.key))
  const processSeries = pid !== null ? seriesByKey.get(`proc_pid_${pid}_cpu_pct`) : undefined

  const domainForTrack: Record<string, string> = {
    power: 'power',
    cpu: 'cpu',
    cpu_pkg: 'cpu',
    gpu: 'gpu',
    display: 'display',
    network: 'network',
    storage: 'storage',
  }
  const trackDomain = (key: string) =>
    overview?.domains.find((d) => d.domain === domainForTrack[key])
  const trackAvailable = (key: string) => trackDomain(key)?.available ?? true

  return (
    <div className="analyze">
      <div className="page-title">
        <h1>{overview?.note || overview?.label || 'Analyze'}</h1>
        {overview ? (
          <span className={`badge ${overview.recovered ? 'derived' : overview.status === 'ok' ? 'available' : 'degraded'}`}>
            {overview.recovered ? 'recovered' : overview.status}
          </span>
        ) : null}
        <span className="hint mono-sm" title={sessionPath}>
          {formatDateTime(overview?.startWallMs ?? 0)} · {formatDuration((overview?.durationS ?? 0) * 1000)}
        </span>
        <span className="spacer" style={{ flex: 1 }} />
        <button onClick={() => onSelectSession(null)} title="Choose another session">
          Change session
        </button>
        <button onClick={() => onOpenDetails(sessionPath)}>Session details</button>
        {onCompareSession ? (
          <button onClick={() => onCompareSession(sessionPath)} title="Compare this whole session">
            Compare session
          </button>
        ) : null}
      </div>

      <ReportExportBar
        request={{
          kind: 'analyze',
          path: sessionPath,
          fromMs: selection?.[0] ?? overview?.startWallMs ?? null,
          toMs: selection?.[1] ?? overview?.endWallMs ?? null,
        }}
        sessionPath={sessionPath}
        defaultRedact={defaultRedact}
        onNotice={onNotice ?? (() => {})}
      />

      {overviewError ? (
        <div className="error-box">
          <div className="code">{overviewError.code}</div>
          <div>{overviewError.message}</div>
          {overviewError.detail ? (
            <details>
              <summary>Technical detail</summary>
              <pre className="mono-sm">{overviewError.detail}</pre>
            </details>
          ) : null}
        </div>
      ) : null}
      {loadingOverview ? <div className="empty">Building session index…</div> : null}

      {overview ? (
        <>
          <SessionStatsStrip stats={overview.stats} />
          <div className="domain-availability">
            {overview.domains.map((d) => (
              <span key={d.domain} className={`badge ${d.available ? 'available' : 'unavailable'}`} title={d.reason ?? undefined}>
                {d.domain}
              </span>
            ))}
          </div>

          <div className="toolbar analyze-toolbar">
            <div className="pill-tabs" role="group" aria-label="Timeline tracks">
              {TRACKS.map((t) => (
                <button
                  key={t.key}
                  className={`${enabled.has(t.key) ? 'active' : ''}${
                    trackAvailable(t.key) ? '' : ' track-off'
                  }`}
                  onClick={() => toggleTrack(t.key)}
                  aria-pressed={enabled.has(t.key)}
                  title={trackAvailable(t.key) ? undefined : trackDomain(t.key)?.reason ?? 'no evidence recorded'}
                >
                  {t.label}
                </button>
              ))}
            </div>
            <span className="muted mono-sm">wheel zoom · drag select · shift-drag pan · dbl-click reset</span>
            <span className="spacer" />
            <div className="pill-tabs">
              {QUICK_WINDOWS.map((w) => (
                <button
                  key={w.key}
                  onClick={() =>
                    setViewport(
                      w.ms === null
                        ? null
                        : [Math.max(overview.startWallMs, overview.endWallMs - w.ms), overview.endWallMs],
                    )
                  }
                >
                  {w.label}
                </button>
              ))}
            </div>
            <button onClick={() => setViewport(null)}>Reset view</button>
            <button onClick={() => setSelection(null)} disabled={!selection}>
              Clear selection
            </button>
            {onCompareRange ? (
              <button
                className="primary"
                disabled={!selection}
                onClick={() => selection && onCompareRange(sessionPath, selection[0], selection[1])}
                title="Compare the selected interval against a baseline"
              >
                Compare this range
              </button>
            ) : null}
          </div>

          {windowError ? (
            <div className="notice">Window request failed [{windowError.code}]: {windowError.message}</div>
          ) : null}

          <section className="panel timeline-panel">
            <header>
              Synchronized timeline{' '}
              <span className="hint">
                selection and viewport are separate; a session-relative clock with wall time on hover
              </span>
              {analysisLoading ? <span className="muted mono-sm">analyzing…</span> : null}
            </header>
            <div className="content flush">
              {enabledTracks.map((t) => {
                const series = seriesByKey.get(t.seriesKey)
                const domain = trackDomain(t.key)
                return (
                  <div className="track" key={t.key}>
                    <div className="track-head">
                      <span className="track-name">{t.label}</span>
                      <span className="track-unit">{t.unit}</span>
                      <span className="spacer" />
                      <span className="muted mono-sm">{series?.points.length ?? 0} samples</span>
                      {domain && !domain.available ? (
                        <span className="badge unavailable" title={domain.reason ?? undefined}>
                          no evidence
                        </span>
                      ) : null}
                    </div>
                    <TimelinePane
                      points={series?.points ?? []}
                      unit={t.unit}
                      label={t.label}
                      color={t.color}
                      height={t.key === 'power' ? 150 : 74}
                      viewport={viewport}
                      selection={selection}
                      events={overview.events}
                      bounds={sessionBounds}
                      onViewport={setViewport}
                      onSelect={setSelection}
                      onCursor={setCursorMs}
                    />
                  </div>
                )
              })}
              {processSeries ? (
                <div className="track">
                  <div className="track-head">
                    <span className="track-name">{processSeries.label}</span>
                    <span className="track-unit">{processSeries.unit}</span>
                    <span className="spacer" />
                    <span className="muted mono-sm">{processSeries.points.length} samples</span>
                  </div>
                  <TimelinePane
                    points={processSeries.points}
                    unit={processSeries.unit}
                    label={processSeries.label}
                    color="#f778ba"
                    height={74}
                    viewport={viewport}
                    selection={selection}
                    events={overview.events}
                    bounds={sessionBounds}
                    onViewport={setViewport}
                    onSelect={setSelection}
                    onCursor={setCursorMs}
                  />
                </div>
              ) : null}
              {enabledTracks.length === 0 && !processSeries ? (
                <div className="empty">All tracks are hidden. Enable one above.</div>
              ) : null}
            </div>
          </section>

          <section className="panel">
            <header>
              Event rail <span className="hint">exact recorded positions</span>
            </header>
            <EventRail
              events={overview.events}
              startWallMs={overview.startWallMs}
              activeWallMs={activeEvent?.wallMs ?? null}
              onPick={pickEvent}
            />
            {activeEvent ? (
              <div className="event-detail">
                <strong>{activeEvent.kind}</strong> · {formatDateTime(activeEvent.wallMs)} ·{' '}
                {activeEvent.detail || 'no detail recorded'}
                <button style={{ marginLeft: 8 }} onClick={() => setActiveEvent(null)}>
                  Close
                </button>
              </div>
            ) : null}
          </section>

          <section className="panel">
            <header>
              Overview <span className="hint">drag to move the viewport</span>
            </header>
            <div className="content flush">
              <TimelinePane
                points={seriesByKey.get('battery_discharge_w')?.points ?? []}
                unit="W"
                label="Whole session power"
                color="#4aa8ff"
                height={54}
                viewport={null}
                selection={viewport}
                events={overview.events}
                bounds={sessionBounds}
                showAxis={false}
                onSelect={(s) => setViewport(s)}
              />
            </div>
          </section>

          <SplitPane id="analyze-evidence" side="right" initial={420} min={340} max={720}>
            <div className="side-stack">
              {analysis ? (
                <>
                  <RangeQualityStrip quality={analysis.quality} />
                  <EnergyPanel energy={analysis.energy} />
                  <ChangesPanel changes={analysis.changes} categorical={analysis.categorical} />
                  <DomainsPanel domains={analysis.domains} />
                  <CorrelationsPanel correlations={analysis.correlations} />
                </>
              ) : (
                <section className="panel">
                  <header>Selected interval evidence</header>
                  <div className="content">
                    <div className="empty">Drag across a timeline to select an interval T1 → T2.</div>
                  </div>
                </section>
              )}
            </div>
            <div className="side-stack">
              <DetailsInspector
                cursorMs={cursorMs}
                startWallMs={overview.startWallMs}
                readings={cursorReadings}
                selection={selection}
                analysis={analysis}
              />
              <ProcessPanel
                processes={analysis?.processes ?? null}
                selectedPid={pid}
                onSelect={setPid}
              />
              <RegionList
                regions={overview.regions}
                startWallMs={overview.startWallMs}
                onJump={jumpRegion}
              />
            </div>
          </SplitPane>
        </>
      ) : null}
    </div>
  )
}
