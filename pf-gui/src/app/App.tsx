import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Activity,
  ChevronsLeft,
  ChevronsRight,
  FlaskConical,
  Files,
  Gauge,
  GitCompare,
  Search,
  Settings,
  Stethoscope,
  Wrench,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import { bridge } from '../api/bridge'
import type { AppSettings, CapabilityReport, LiveEvidence } from '../api/types'
import { usePolling } from '../hooks/usePolling'
import type { AsyncState } from '../hooks/usePolling'
import { useAgentControls } from '../hooks/useAgentControls'
import { formatDuration } from '../lib/format'
import { CommandPalette, type Command } from '../components/CommandPalette'
import { EvidenceHelp } from '../components/EvidenceHelp'
import { OverviewPage } from '../pages/Overview'
import { LiveMonitorPage } from '../pages/LiveMonitor'
import { SessionsPage } from '../pages/Sessions'
import { SessionDetailsPage } from '../pages/SessionDetails'
import { AnalyzePage } from '../pages/Analyze'
import { ComparePage, type CompareSeed } from '../pages/Compare'
import { CalibrationPage } from '../pages/Calibration'
import { DiagnosticsPage } from '../pages/Diagnostics'
import { ExperimentsPage } from '../pages/Experiments'
import { SettingsPage } from '../pages/Settings'
import { seedAnalyzeRange } from '../lib/analyze'
import { loadRecents, loadState, pushRecent, saveState } from '../lib/recents'

/**
 * Development-only chart lifecycle inspector. Reads the compact state the
 * shared uPlot hook records (`window.__pfCharts`): one line per live plot with
 * its measured size, committed x scale and data length. This is the narrow
 * helper used to diagnose fresh-mount rendering in the real WebView. It is
 * compiled out of production builds.
 */
function DevChartInspector() {
  const [text, setText] = useState('')
  useEffect(() => {
    const tick = () => {
      const charts =
        (window as unknown as { __pfCharts?: Record<string, unknown> }).__pfCharts ?? {}
      setText(JSON.stringify(charts, null, 1))
    }
    tick()
    const t = setInterval(tick, 1000)
    return () => clearInterval(t)
  }, [])
  return (
    <pre
      style={{
        position: 'fixed',
        left: 0,
        bottom: 26,
        maxHeight: 200,
        maxWidth: 640,
        overflow: 'auto',
        margin: 0,
        padding: '4px 8px',
        background: '#000',
        color: '#6f6',
        fontSize: 9,
        zIndex: 9999,
        whiteSpace: 'pre-wrap',
      }}
    >
      {text}
    </pre>
  )
}


export type Page =
  | 'overview'
  | 'live'
  | 'sessions'
  | 'analyze'
  | 'compare'
  | 'experiments'
  | 'calibration'
  | 'diagnostics'
  | 'settings'

interface NavEntry {
  id: Page
  label: string
  icon: LucideIcon
}

const NAV: NavEntry[] = [
  { id: 'overview', label: 'Overview', icon: Gauge },
  { id: 'live', label: 'Live Monitor', icon: Activity },
  { id: 'sessions', label: 'Sessions', icon: Files },
  { id: 'analyze', label: 'Analyze', icon: Search },
  { id: 'compare', label: 'Compare', icon: GitCompare },
  { id: 'experiments', label: 'Experiments', icon: FlaskConical },
  { id: 'calibration', label: 'Calibration', icon: Wrench },
  { id: 'diagnostics', label: 'Diagnostics', icon: Stethoscope },
  { id: 'settings', label: 'Settings', icon: Settings },
]

const PAGE_IDS: Page[] = [
  'overview',
  'live',
  'sessions',
  'analyze',
  'compare',
  'experiments',
  'calibration',
  'diagnostics',
  'settings',
]

/** Ctrl+1..7 map to the primary workflow screens. */
const SHORTCUT_PAGES: Record<string, Page> = {
  '1': 'overview',
  '2': 'live',
  '3': 'sessions',
  '4': 'analyze',
  '5': 'compare',
  '6': 'experiments',
  '7': 'calibration',
}

function isEditable(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el) return false
  const tag = el.tagName
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable
}

function collectorSummary(states: { state: string }[]) {
  const count = (s: string) => states.filter((c) => c.state === s).length
  return {
    total: states.length,
    available: count('available'),
    degraded: count('degraded'),
    unavailable: count('unavailable') + count('stale') + count('timeout'),
  }
}

export function App() {
  const restored = loadState()
  const [page, setPage] = useState<Page>(() =>
    restored.page && PAGE_IDS.includes(restored.page as Page) ? (restored.page as Page) : 'overview',
  )
  const [selectedSession, setSelectedSession] = useState<string | null>(
    restored.selectedSession ?? null,
  )
  const [compareSeed, setCompareSeed] = useState<CompareSeed | null>(null)
  const [collapsed, setCollapsed] = useState(() => localStorage.getItem('pf.sidebar.collapsed') === '1')
  const [notice, setNotice] = useState<string | null>(null)
  const [settings, setSettings] = useState<AppSettings | null>(null)
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [helpOpen, setHelpOpen] = useState(false)
  const [recentSessions, setRecentSessions] = useState<string[]>(() => loadRecents<string>('sessions'))

  useEffect(() => {
    localStorage.setItem('pf.sidebar.collapsed', collapsed ? '1' : '0')
  }, [collapsed])

  // Settings are backend-owned; a corrupt file falls back to defaults and can
  // never block startup.
  useEffect(() => {
    bridge
      .getSettings()
      .then(setSettings)
      .catch(() => setSettings(null))
  }, [])

  const chartInterval = settings?.chartIntervalMs ?? 1000

  // Theme is real: a data attribute swaps the CSS variables.
  useEffect(() => {
    const theme = settings?.theme ?? 'system'
    if (theme === 'system') {
      delete document.documentElement.dataset.theme
    } else {
      document.documentElement.dataset.theme = theme
    }
  }, [settings?.theme])

  // Restore the practical surface after restart: last page and selected
  // session only. Recording/experiment actions are never auto-resumed.
  useEffect(() => {
    saveState({ page, selectedSession })
  }, [page, selectedSession])

  const app = usePolling(bridge.appStatus, 15000)
  const live = usePolling(bridge.liveSnapshot, chartInterval, !!app.data)
  const capabilities = usePolling(bridge.capabilities, 30000, !!app.data)
  const controls = useAgentControls(live.refresh)

  const agent = live.data?.agent ?? null
  const sessionState = live.data?.sessionState
  const degraded = (live.data?.collectors ?? []).filter(
    (c) => c.state === 'degraded' || c.state === 'stale' || c.state === 'timeout',
  )
  const unavailable = (live.data?.collectors ?? []).filter((c) => c.state === 'unavailable')

  // Restrained in-app notification for meaningful lifecycle events. Polling
  // state changes (including every sample) are not toasted.
  const lastSessionState = useRef<string | undefined>(undefined)
  useEffect(() => {
    const prev = lastSessionState.current
    lastSessionState.current = sessionState
    if (prev === 'recording' && sessionState === 'ended') setNotice('Recording finished.')
  }, [sessionState])

  // Desktop keyboard: Ctrl+1..7 navigate; Ctrl+K palette. Shortcuts never fire
  // while typing into an input, textarea, select or contenteditable.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return
      if (isEditable(e.target)) return
      if (e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPaletteOpen((v) => !v)
        return
      }
      const target = SHORTCUT_PAGES[e.key]
      if (target) {
        e.preventDefault()
        setPage(target)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const rememberSession = useCallback((path: string) => {
    setRecentSessions(pushRecent<string>('sessions', path, (a, b) => a === b))
  }, [])

  const openSession = useCallback(
    (path: string) => {
      setSelectedSession(path)
      rememberSession(path)
      setPage('sessions')
    },
    [rememberSession],
  )

  // Analyze preserves the current selected session across navigation; a
  // selected session is explicit, never inferred from filesystem ordering.
  const openAnalyze = useCallback(
    (path: string) => {
      setSelectedSession(path)
      rememberSession(path)
      setPage('analyze')
    },
    [rememberSession],
  )

  // Compare -> Analyze must not lose the selected range: seed the exact
  // timestamps into Analyze's own saved-view slot before navigating.
  const openAnalyzeRange = useCallback((path: string, fromMs?: number, toMs?: number) => {
    if (fromMs !== undefined && toMs !== undefined) seedAnalyzeRange(path, fromMs, toMs)
    setSelectedSession(path)
    setPage('analyze')
  }, [])

  const openCompare = useCallback((seed: CompareSeed | null) => {
    setCompareSeed(seed)
    setPage('compare')
  }, [])

  const onStart = useCallback(
    (opts: { intervalMs?: number; collectors?: string; preset?: string; note?: string }) =>
      controls.start(opts),
    [controls],
  )

  const startWithDefaults = useCallback(
    () =>
      controls.start({
        preset: settings?.defaultPreset ?? undefined,
        collectors: settings?.defaultCollectors ?? undefined,
      }),
    [controls, settings],
  )

  const addMarkerPrompt = useCallback(() => {
    const text = window.prompt('Marker text')
    if (text) void controls.marker(text)
  }, [controls])

  const commands: Command[] = useMemo(
    () => [
      ...NAV.map((n) => ({ id: `go-${n.id}`, label: `Go to ${n.label}`, run: () => setPage(n.id) })),
      { id: 'start', label: 'Start monitoring', run: () => void startWithDefaults() },
      { id: 'marker', label: 'Add marker', run: addMarkerPrompt },
      { id: 'help', label: 'Show evidence model', run: () => setHelpOpen(true) },
      ...recentSessions.map((path) => ({
        id: `recent-${path}`,
        label: `Open recent session: ${path.split(/[\\/]/).pop()}`,
        hint: path,
        run: () => openAnalyze(path),
      })),
    ],
    [recentSessions, startWithDefaults, addMarkerPrompt, openAnalyze],
  )

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">
          power-forensics <small>workbench</small>
        </span>
        <span className="context" aria-label="Current screen and recording state">
          <strong>{NAV.find((n) => n.id === page)?.label ?? 'Overview'}</strong>
          {sessionState === 'recording' ? (
            <>
              <span className="dot record" aria-hidden />
              <span className="badge recording">Recording</span>
              <span className="mono-sm">{formatDuration(agent?.uptimeMs ?? 0)}</span>
            </>
          ) : sessionState === 'paused' ? (
            <>
              <span className="dot warn" aria-hidden />
              <span className="badge warn">Paused</span>
              <span className="mono-sm">{formatDuration(agent?.uptimeMs ?? 0)}</span>
            </>
          ) : sessionState === 'outside-dir' ? (
            <>
              <span className="dot warn" aria-hidden />
              <span className="badge warn">Session outside directory</span>
            </>
          ) : sessionState === 'ended' ? (
            <>
              <span className="dot muted" aria-hidden />
              <span className="badge degraded">Session ended</span>
            </>
          ) : (
            <>
              <span className="dot muted" aria-hidden />
              <span className="muted">No active recording</span>
            </>
          )}
          {degraded.length ? (
            <span className="badge warn" title={degraded.map((c) => `${c.name}: ${c.state}`).join(', ')}>
              {degraded.length} degraded
            </span>
          ) : null}
        </span>
        <span className="spacer" />
        <button
          className="topstat"
          onClick={() => setHelpOpen(true)}
          title="What Measured / Derived / Estimated / Unavailable mean"
        >
          Evidence model
        </button>
        <button className="topstat" onClick={() => setPaletteOpen(true)} title="Ctrl+K">
          ⌘ Commands
        </button>
      </header>

      <div className={`body${collapsed ? ' collapsed' : ''}`}>
        <nav className="sidebar" aria-label="Primary">
          {NAV.map((entry) => {
            const Icon = entry.icon
            const active = page === entry.id
            return (
              <button
                key={entry.id}
                className={`navitem${active ? ' active' : ''}`}
                onClick={() => setPage(entry.id)}
                title={collapsed ? entry.label : undefined}
                aria-current={active ? 'page' : undefined}
              >
                <Icon size={15} aria-hidden />
                {!collapsed ? <span className="navlabel">{entry.label}</span> : null}
              </button>
            )
          })}
          <button
            className="navitem collapse"
            onClick={() => setCollapsed((c) => !c)}
            title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          >
            {collapsed ? <ChevronsRight size={15} /> : <ChevronsLeft size={15} />}
            {!collapsed ? <span className="navlabel">Collapse</span> : null}
          </button>
        </nav>

        <main className="main">
          {!app.data && !app.error ? <div className="empty">Connecting to the desktop shell…</div> : null}
          {app.error ? (
            <div className="error-box">
              <div className="code">{app.error.code}</div>
              <div>{app.error.message}</div>
              {app.error.detail ? (
                <details>
                  <summary>Detail</summary>
                  <pre className="mono-sm">{app.error.detail}</pre>
                </details>
              ) : null}
            </div>
          ) : null}
          {notice ? (
            <div className="notice">
              {notice}{' '}
              <button onClick={() => setNotice(null)} style={{ marginLeft: 8 }}>
                Dismiss
              </button>
            </div>
          ) : null}

          {app.data ? (
            <PageContent
              page={page}
              live={live}
              controls={controls}
              capabilities={capabilities.data}
              recentSession={recentSessions[0] ?? null}
              onStart={onStart}
              selectedSession={selectedSession}
              compareSeed={compareSeed}
              defaultRedact={settings?.redactionDefault ?? false}
              onOpenSession={openSession}
              onAnalyzeSession={openAnalyze}
              onAnalyzeRange={openAnalyzeRange}
              onCompare={openCompare}
              onSelectSession={setSelectedSession}
              onCloseSession={() => setSelectedSession(null)}
              onNotice={setNotice}
              onOpenDiagnostics={() => setPage('diagnostics')}
              onBrowseSessions={() => setPage('sessions')}
            />
          ) : null}
        </main>
      </div>

      <CommandPalette open={paletteOpen} commands={commands} onClose={() => setPaletteOpen(false)} />
      {helpOpen ? <EvidenceHelp onClose={() => setHelpOpen(false)} /> : null}
      {import.meta.env.DEV ? <DevChartInspector /> : null}

      <footer className="statusbar">
        <span>
          Agent:{' '}
          {agent?.available ? (agent.running ? 'running' : 'idle') : 'unavailable'}
          {agent?.paused ? ' (paused)' : ''}
        </span>
        <span className="sep">│</span>
        <span>Elevation: {app.data?.elevated ? 'yes' : 'no'}</span>
        <span className="sep">│</span>
        <span>
          Session:{' '}
          {live.data?.session
            ? `${live.data.session.label || 'agent'} · ${formatDuration(live.data.session.elapsedMs)}`
            : sessionState === 'outside-dir'
              ? 'outside configured directory'
              : sessionState === 'ended'
                ? 'ended'
                : 'none'}
        </span>
        <span className="sep">│</span>
        <span>Collectors: {collectorSummary(live.data?.collectors ?? []).available}/{collectorSummary(live.data?.collectors ?? []).total} available</span>
        <span className="sep">│</span>
        <span>Interval: {live.data?.session?.intervalMs ?? '—'} ms</span>
        <span className="sep">│</span>
        <span>Warnings: {degraded.length + unavailable.length}</span>
        <span className="spacer" style={{ flex: 1 }} />
        <span>
          v{app.data?.guiVersion ?? '—'} · tool {app.data?.toolVersion ?? '—'}
        </span>
      </footer>
    </div>
  )
}

type LiveState = AsyncState<LiveEvidence> & { refresh: () => void }

interface PageContentProps {
  page: Page
  live: LiveState
  controls: ReturnType<typeof useAgentControls>
  capabilities: CapabilityReport | null
  recentSession: string | null
  onStart: (opts: {
    intervalMs?: number
    collectors?: string
    preset?: string
    note?: string
  }) => Promise<void>
  selectedSession: string | null
  compareSeed: CompareSeed | null
  defaultRedact: boolean
  onOpenSession: (path: string) => void
  onAnalyzeSession: (path: string) => void
  onAnalyzeRange: (path: string, fromMs?: number, toMs?: number) => void
  onCompare: (seed: CompareSeed | null) => void
  onSelectSession: (path: string | null) => void
  onCloseSession: () => void
  onNotice: (msg: string | null) => void
  onOpenDiagnostics: () => void
  onBrowseSessions: () => void
}

function PageContent(props: PageContentProps) {
  const {
    page,
    live,
    controls,
    capabilities,
    recentSession,
    onStart,
    selectedSession,
    compareSeed,
    defaultRedact,
    onOpenSession,
    onAnalyzeSession,
    onAnalyzeRange,
    onCompare,
    onSelectSession,
    onCloseSession,
    onNotice,
    onOpenDiagnostics,
    onBrowseSessions,
  } = props
  switch (page) {
    case 'overview':
      return (
        <OverviewPage
          live={live.data}
          error={live.error}
          controls={controls}
          capabilities={capabilities}
          recentSession={recentSession}
          onStart={onStart}
          onOpenSession={onOpenSession}
          onRetry={live.refresh}
          onOpenDiagnostics={onOpenDiagnostics}
        />
      )
    case 'live':
      return <LiveMonitorPage live={live.data} controls={controls} />
    case 'sessions':
      return selectedSession ? (
        <SessionDetailsPage
          path={selectedSession}
          onBack={onCloseSession}
          onAnalyze={onAnalyzeSession}
          onCompare={(path) => onCompare({ path, fromMs: null, toMs: null })}
          onNotice={onNotice}
        />
      ) : (
        <SessionsPage
          onOpen={onOpenSession}
          onAnalyze={onAnalyzeSession}
          onCompare={(path) => onCompare({ path, fromMs: null, toMs: null })}
          onNotice={onNotice}
        />
      )
    case 'analyze':
      return (
        <AnalyzePage
          sessionPath={selectedSession}
          onSelectSession={onSelectSession}
          onOpenDetails={onOpenSession}
          onCompareRange={(path, fromMs, toMs) => onCompare({ path, fromMs, toMs })}
          onCompareSession={(path) => onCompare({ path, fromMs: null, toMs: null })}
          onNotice={onNotice}
          defaultRedact={defaultRedact}
          onBrowseSessions={onBrowseSessions}
        />
      )
    case 'compare':
      return (
        <ComparePage
          seed={compareSeed}
          onAnalyze={onAnalyzeRange}
          onNotice={onNotice}
          defaultRedact={defaultRedact}
        />
      )
    case 'calibration':
      return <CalibrationPage onNotice={onNotice} />
    case 'diagnostics':
      return <DiagnosticsPage />
    case 'settings':
      return <SettingsPage onNotice={onNotice} />
    case 'experiments':
      return (
        <ExperimentsPage
          onAnalyze={onAnalyzeRange}
          onCompare={(a, b) =>
            onCompare({
              path: a.path,
              fromMs: a.fromMs ?? null,
              toMs: a.toMs ?? null,
              b: { path: b.path, fromMs: b.fromMs ?? null, toMs: b.toMs ?? null },
            })
          }
          onNotice={onNotice}
          defaultRedact={defaultRedact}
        />
      )
  }
}
