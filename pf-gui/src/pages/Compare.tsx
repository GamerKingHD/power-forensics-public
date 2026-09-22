import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { BridgeFailure, bridge } from '../api/bridge'
import type { ComparisonAnalysis, Series } from '../api/types'
import { usePolling } from '../hooks/usePolling'
import { TimelinePane } from '../components/analyze/TimelinePane'
import { ComparisonSetup, emptySide, type SideConfig } from '../components/compare/ComparisonSetup'
import { EmptyState } from '../components/layout/EmptyState'
import { ReportExportBar } from '../components/report/ReportExport'
import {
  CaveatPanel,
  CategoricalPanel,
  ComparabilityStrip,
  CorrelationComparisonPanel,
  DistributionPanel,
  DomainComparisonPanel,
  EnergyComparisonPanel,
  MetricComparisonTable,
  ProcessComparisonTable,
  QualityComparisonPanel,
  RankedDifferences,
} from '../components/compare/ComparisonReport'
import { TRACKS } from '../lib/analyze'
import {
  ALIGNMENTS,
  COLOR_A,
  COLOR_B,
  alignmentXMode,
  type AlignmentMode,
  type TimelineView,
  pctText,
  transformPoints,
} from '../lib/compare'
import { formatDateTime, formatDuration } from '../lib/format'

export interface CompareTargetSeed {
  path: string
  fromMs: number | null
  toMs: number | null
}

export interface CompareSeed {
  path: string
  fromMs: number | null
  toMs: number | null
  /** Optional side B for paired/experiment hand-offs. */
  b?: CompareTargetSeed
}

interface Props {
  seed: CompareSeed | null
  onAnalyze: (path: string, fromMs?: number, toMs?: number) => void
  onNotice: (msg: string | null) => void
  defaultRedact?: boolean
}

function toFailure(e: unknown): BridgeFailure {
  return e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
}

function validRange(s: SideConfig): boolean {
  return s.mode === 'whole' || (s.fromMs !== null && s.toMs !== null && s.toMs > s.fromMs)
}

function sideArg(s: SideConfig): { path: string; fromMs?: number; toMs?: number } {
  if (s.mode === 'whole') return { path: s.path as string }
  return { path: s.path as string, fromMs: s.fromMs ?? undefined, toMs: s.toMs ?? undefined }
}

export function ComparePage({ seed, onAnalyze, onNotice, defaultRedact = false }: Props) {
  const sessions = usePolling(bridge.listSessions, 5000)
  const [a, setA] = useState<SideConfig>(emptySide)
  const [b, setB] = useState<SideConfig>(emptySide)
  const [analysis, setAnalysis] = useState<ComparisonAnalysis | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<BridgeFailure | null>(null)
  const [alignment, setAlignment] = useState<AlignmentMode>('start')
  const [view, setView] = useState<TimelineView>('stacked')
  const [metricKey, setMetricKey] = useState('battery_discharge_w')
  const [seriesA, setSeriesA] = useState<Series | null>(null)
  const [seriesB, setSeriesB] = useState<Series | null>(null)
  const [timelineLoading, setTimelineLoading] = useState(false)
  const compareToken = useRef(0)
  const windowToken = useRef(0)

  // Pre-populate side A (and an optional side B for paired hand-offs) from an
  // Analyze range / session / experiment action.
  useEffect(() => {
    if (!seed) return
    setA({
      path: seed.path,
      mode: seed.fromMs !== null && seed.toMs !== null ? 'range' : 'whole',
      fromMs: seed.fromMs,
      toMs: seed.toMs,
    })
    if (seed.b) {
      setB({
        path: seed.b.path,
        mode: seed.b.fromMs !== null && seed.b.toMs !== null ? 'range' : 'whole',
        fromMs: seed.b.fromMs,
        toMs: seed.b.toMs,
      })
    }
  }, [seed?.path, seed?.fromMs, seed?.toMs, seed?.b?.path, seed?.b?.fromMs, seed?.b?.toMs])

  const canCompare = Boolean(a.path && b.path && validRange(a) && validRange(b))

  // Deep comparison runs only once both sides are valid; obsolete responses
  // are ignored so switching B or ranges can never show a stale result.
  useEffect(() => {
    if (!canCompare || !a.path || !b.path) {
      setAnalysis(null)
      setLoading(false)
      return
    }
    const token = ++compareToken.current
    setLoading(true)
    const t = setTimeout(() => {
      bridge
        .compareSessions(sideArg(a), sideArg(b))
        .then((res) => {
          if (token !== compareToken.current) return
          setAnalysis(res)
          setError(null)
          setLoading(false)
        })
        .catch((e) => {
          if (token !== compareToken.current) return
          setError(toFailure(e))
          setAnalysis(null)
          setLoading(false)
        })
    }, 150)
    return () => clearTimeout(t)
  }, [a, b, canCompare])

  // Bounded timeline windows for the selected metric, using the authoritative
  // bounds Rust resolved (not the raw configuration).
  useEffect(() => {
    if (!analysis) {
      setSeriesA(null)
      setSeriesB(null)
      return
    }
    const token = ++windowToken.current
    setTimelineLoading(true)
    void (async () => {
      try {
        const [wa, wb] = await Promise.all([
          bridge.sessionWindow(analysis.a.path, {
            fromMs: analysis.a.fromMs,
            toMs: analysis.a.toMs,
            maxPoints: 1200,
          }),
          bridge.sessionWindow(analysis.b.path, {
            fromMs: analysis.b.fromMs,
            toMs: analysis.b.toMs,
            maxPoints: 1200,
          }),
        ])
        if (token !== windowToken.current) return
        setSeriesA(wa.series.find((s) => s.key === metricKey) ?? null)
        setSeriesB(wb.series.find((s) => s.key === metricKey) ?? null)
        setTimelineLoading(false)
      } catch (e) {
        if (token !== windowToken.current) return
        setError(toFailure(e))
        setTimelineLoading(false)
      }
    })()
  }, [analysis, metricKey])

  const swap = useCallback(() => {
    setA(b)
    setB(a)
  }, [a, b])

  const copySummary = useCallback(async () => {
    if (!analysis) return
    const lines: string[] = [
      `A: ${analysis.a.label} — ${analysis.a.wholeSession ? 'whole session' : formatDateTime(analysis.a.fromMs)}`,
      `B: ${analysis.b.label} — ${analysis.b.wholeSession ? 'whole session' : formatDateTime(analysis.b.fromMs)}`,
      `Comparability: ${analysis.comparability.overall}`,
      `Coverage: A ${pctText(analysis.quality.coverageA)} / B ${pctText(analysis.quality.coverageB)} (weakest ${analysis.quality.weakestSide})`,
      '',
      'Headline differences (B - A):',
      ...analysis.headlineMetrics.map(
        (m) =>
          `${m.label}: A ${m.a ?? '—'}${m.a === null ? '' : ` ${m.unit}`} B ${m.b ?? '—'}${m.b === null ? '' : ` ${m.unit}`}` +
          ` delta ${m.absoluteDelta === null ? '—' : `${m.absoluteDelta.toFixed(3)}`}` +
          ` [${m.comparability}/${m.reliability.level}]`,
      ),
      '',
      'Caveats:',
      ...(analysis.caveats.length
        ? analysis.caveats.map((c) => `- [${c.severity}] ${c.message}`)
        : ['- none']),
    ]
    const text = lines.join('\n')
    try {
      await navigator.clipboard.writeText(text)
      onNotice('Comparison summary copied (coverage and caveats included).')
    } catch {
      onNotice('Clipboard unavailable; use Export JSON instead.')
    }
  }, [analysis, onNotice])

  const download = useCallback((name: string, mime: string, text: string) => {
    const blob = new Blob([text], { type: mime })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.download = name
    link.click()
    URL.revokeObjectURL(url)
  }, [])

  const exportCsv = useCallback(() => {
    if (!analysis) return
    const esc = (v: string | number | null) => {
      const s = v === null ? '' : String(v)
      return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
    }
    const header = [
      'metric', 'label', 'domain', 'unit', 'a', 'b', 'absolute_delta', 'relative_delta',
      'direction', 'comparability', 'reliability', 'coverage_a', 'coverage_b',
    ]
    const rows = analysis.headlineMetrics.map((m) =>
      [
        m.metric, m.label, m.domain, m.unit, m.a, m.b, m.absoluteDelta, m.relativeDelta,
        m.direction, m.comparability, m.reliability.level, m.coverageA, m.coverageB,
      ]
        .map(esc)
        .join(','),
    )
    const caveats = analysis.caveats.map((c) => ['caveat', c.scope, c.severity, c.message].map(esc).join(','))
    download('comparison.csv', 'text/csv', [header.join(','), ...rows, ...caveats].join('\n'))
  }, [analysis, download])

  const metricOptions = useMemo(() => {
    const base = TRACKS.map((t) => ({ key: t.seriesKey, label: t.label, unit: t.unit }))
    if (!analysis) return base
    const known = new Set(base.map((x) => x.key))
    for (const m of analysis.headlineMetrics) {
      if (!known.has(m.metric)) base.push({ key: m.metric, label: m.label, unit: m.unit })
    }
    return base
  }, [analysis])

  const xMode = alignmentXMode(alignment)
  const showTimeline = analysis && (seriesA || seriesB)

  return (
    <div className="compare">
      <div className="page-title">
        <h1>Compare</h1>
        <span className="hint">
          Observational comparison of two sessions or ranges — never a benchmark scoreboard.
        </span>
        <span className="spacer" style={{ flex: 1 }} />
        {analysis ? (
          <>
            <span className="badge derived" title="Direction alone is not improvement">
              direction is context-dependent
            </span>
            <button onClick={() => void copySummary()}>Copy summary</button>
            <button onClick={exportCsv}>Export metric CSV</button>
          </>
        ) : null}
      </div>

      {a.path && b.path ? (
        <ReportExportBar
          request={{
            kind: 'compare',
            path: a.path,
            fromMs: a.fromMs,
            toMs: a.toMs,
            pathB: b.path,
            fromMsB: b.fromMs,
            toMsB: b.toMs,
          }}
          defaultRedact={defaultRedact}
          onNotice={onNotice}
        />
      ) : null}

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

      <ComparisonSetup
        sessions={sessions.data ?? []}
        a={a}
        b={b}
        onChange={(side, cfg) => (side === 'a' ? setA(cfg) : setB(cfg))}
        onSwap={swap}
        onCompare={() => {
          // A manual nudge re-runs the debounced comparison.
          const token = ++compareToken.current
          if (!canCompare || !a.path || !b.path) return
          setLoading(true)
          bridge
            .compareSessions(sideArg(a), sideArg(b))
            .then((res) => {
              if (token !== compareToken.current) return
              setAnalysis(res)
              setError(null)
              setLoading(false)
            })
            .catch((e) => {
              if (token !== compareToken.current) return
              setError(toFailure(e))
              setLoading(false)
            })
        }}
        canCompare={canCompare}
        busy={loading}
        compact={Boolean(analysis)}
      />

      {!analysis && !loading && !error ? (
        <section className="panel">
          <div className="content">
            <EmptyState
              title="Compare evidence"
              description="Choose session A and session B (whole session or a range) to compare. Differences are observational and preserve evidence-quality caveats."
              details={[
                { k: 'Session A', v: a.path ? a.path.split(/[\\/]/).pop() : 'not selected' },
                { k: 'Session B', v: b.path ? b.path.split(/[\\/]/).pop() : 'not selected' },
              ]}
            />
          </div>
        </section>
      ) : null}
      {loading && !analysis ? <div className="empty">Running the Rust comparison…</div> : null}

      {analysis ? (
        <>
          <ComparabilityStrip analysis={analysis} />

          <div className="compare-top">
            <MetricComparisonTable metrics={analysis.headlineMetrics} />
            <CaveatPanel caveats={analysis.caveats} />
          </div>

          <section className="panel">
            <header>
              Synchronized comparison timeline{' '}
              <span className="hint">
                {ALIGNMENTS.find((x) => x.key === alignment)?.hint}
              </span>
              {timelineLoading ? <span className="muted mono-sm">loading…</span> : null}
            </header>
            <div className="toolbar analyze-toolbar">
              <div className="pill-tabs" role="group" aria-label="Comparison alignment">
                {ALIGNMENTS.map((x) => (
                  <button
                    key={x.key}
                    className={alignment === x.key ? 'active' : ''}
                    onClick={() => setAlignment(x.key)}
                    aria-pressed={alignment === x.key}
                  >
                    {x.label}
                  </button>
                ))}
              </div>
              <div className="pill-tabs" role="group" aria-label="Timeline view">
                <button className={view === 'stacked' ? 'active' : ''} onClick={() => setView('stacked')}>
                  Stacked A/B
                </button>
                <button
                  className={view === 'overlay' ? 'active' : ''}
                  onClick={() => setView('overlay')}
                  title="Only valid because both series share one metric and unit"
                >
                  Overlay
                </button>
              </div>
              <span className="spacer" />
              <select value={metricKey} onChange={(e) => setMetricKey(e.target.value)}>
                {metricOptions.map((m) => (
                  <option key={m.key} value={m.key}>
                    {m.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="content flush">
              {showTimeline ? (
                view === 'overlay' ? (
                  <TimelinePane
                    key="overlay"
                    points={transformPoints(seriesA?.points ?? [], analysis.a, alignment)}
                    overlayPoints={transformPoints(seriesB?.points ?? [], analysis.b, alignment)}
                    overlayLabel={`B — ${analysis.b.label}`}
                    overlayColor={COLOR_B}
                    xMode={xMode}
                    unit={seriesA?.unit ?? seriesB?.unit ?? ''}
                    label={`A — ${analysis.a.label}`}
                    color={COLOR_A}
                    height={150}
                    viewport={null}
                    selection={null}
                  />
                ) : (
                  <>
                    <div className="compare-pane-label">
                      <span className="side-tag" style={{ background: COLOR_A }} aria-hidden /> A — {analysis.a.label}
                    </div>
                    <TimelinePane
                      key="a"
                      points={transformPoints(seriesA?.points ?? [], analysis.a, alignment)}
                      xMode={xMode}
                      unit={seriesA?.unit ?? ''}
                      label={`A — ${analysis.a.label}`}
                      color={COLOR_A}
                      height={110}
                      viewport={null}
                      selection={null}
                    />
                    <div className="compare-pane-label">
                      <span className="side-tag" style={{ background: COLOR_B }} aria-hidden /> B — {analysis.b.label}
                    </div>
                    <TimelinePane
                      key="b"
                      points={transformPoints(seriesB?.points ?? [], analysis.b, alignment)}
                      xMode={xMode}
                      unit={seriesB?.unit ?? ''}
                      label={`B — ${analysis.b.label}`}
                      color={COLOR_B}
                      height={110}
                      viewport={null}
                      selection={null}
                    />
                  </>
                )
              ) : (
                <div className="empty">No {metricKey} evidence in one or both ranges.</div>
              )}
              {seriesA && seriesB ? (
                <div className="pane-actions">
                  <button
                    onClick={() =>
                      analysis.a.wholeSession
                        ? onAnalyze(analysis.a.path)
                        : onAnalyze(analysis.a.path, analysis.a.fromMs, analysis.a.toMs)
                    }
                  >
                    Analyze A {analysis.a.wholeSession ? '(whole)' : '(range)'}
                  </button>
                  <button
                    onClick={() =>
                      analysis.b.wholeSession
                        ? onAnalyze(analysis.b.path)
                        : onAnalyze(analysis.b.path, analysis.b.fromMs, analysis.b.toMs)
                    }
                  >
                    Analyze B {analysis.b.wholeSession ? '(whole)' : '(range)'}
                  </button>
                </div>
              ) : null}
            </div>
          </section>

          <RankedDifferences ranked={analysis.rankedChanges} />
          <EnergyComparisonPanel energy={analysis.energy} a={analysis.a.durationS} b={analysis.b.durationS} />
          <DomainComparisonPanel domains={analysis.domains} />
          <CategoricalPanel items={analysis.categoricalDifferences} />
          <ProcessComparisonTable processes={analysis.processes} />
          <DistributionPanel distributions={analysis.distributions} />
          <CorrelationComparisonPanel correlations={analysis.correlations} />
          <QualityComparisonPanel quality={analysis.quality} />

          <div className="muted mono-sm" style={{ padding: '4px 0 12px' }}>
            A: {analysis.a.label} ({analysis.a.wholeSession ? 'whole session' : `${formatDateTime(analysis.a.fromMs)} → ${formatDateTime(analysis.a.toMs)}`}, {formatDuration(analysis.a.durationS * 1000)}) · B:{' '}
            {analysis.b.label} ({analysis.b.wholeSession ? 'whole session' : `${formatDateTime(analysis.b.fromMs)} → ${formatDateTime(analysis.b.toMs)}`}, {formatDuration(analysis.b.durationS * 1000)})
          </div>
        </>
      ) : null}
    </div>
  )
}
