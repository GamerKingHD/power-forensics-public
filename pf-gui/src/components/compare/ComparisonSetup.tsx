import type { SessionListEntry } from '../../api/types'
import { formatDateTime, formatDuration } from '../../lib/format'
import { COLOR_A, COLOR_B } from '../../lib/compare'

export interface SideConfig {
  path: string | null
  mode: 'whole' | 'range'
  fromMs: number | null
  toMs: number | null
}

export function emptySide(): SideConfig {
  return { path: null, mode: 'whole', fromMs: null, toMs: null }
}

function sessionOf(sessions: SessionListEntry[], path: string | null) {
  return sessions.find((s) => s.path === path) ?? null
}

function SideEditor({
  title,
  color,
  side,
  other,
  sessions,
  recent,
  onChange,
}: {
  title: string
  color: string
  side: SideConfig
  other: SideConfig
  sessions: SessionListEntry[]
  recent: SessionListEntry[]
  onChange: (cfg: SideConfig) => void
}) {
  const s = sessionOf(sessions, side.path)
  const start = s?.startWallMs ?? 0
  const end = s?.endWallMs ?? 0
  const otherSession = sessionOf(sessions, other.path)
  const otherDuration = otherSession?.durationS ?? null

  const setRange = (from: number, to: number) => {
    onChange({ ...side, mode: 'range', fromMs: Math.max(start, from), toMs: Math.max(from + 1, to) })
  }
  const matchDuration = () => {
    if (!otherDuration || !s) return
    setRange(start, start + otherDuration * 1000)
  }

  const rangeMs = side.fromMs !== null && side.toMs !== null ? side.toMs - side.fromMs : 0

  return (
    <section className="panel compare-side">
      <header>
        <span className="side-tag" style={{ background: color }} aria-hidden />
        {title}
        {s ? <span className="muted mono-sm">{s.note || s.file}</span> : <span className="muted">no session selected</span>}
      </header>
      <div className="content">
        <label className="compare-field">
          <span className="k">Session</span>
          <select
            value={side.path ?? ''}
            onChange={(e) => onChange({ ...side, path: e.target.value || null, mode: 'whole', fromMs: null, toMs: null })}
          >
            <option value="">Choose a recording…</option>
            {sessions.map((x) => (
              <option key={x.path} value={x.path}>
                {x.note || x.file} · {formatDateTime(x.startWallMs)}
              </option>
            ))}
          </select>
        </label>

        {!s && recent.length ? (
          <div className="compare-recent">
            <span className="muted">Recent</span>
            {recent.map((x) => (
              <button
                key={x.path}
                className="ghost"
                onClick={() => onChange({ ...side, path: x.path, mode: 'whole', fromMs: null, toMs: null })}
                title={`${x.note || x.file} · ${formatDateTime(x.startWallMs)}`}
              >
                {x.note || x.file}
              </button>
            ))}
          </div>
        ) : null}

        <div className="pill-tabs" role="group" aria-label={`${title} scope`}>
          <button
            className={side.mode === 'whole' ? 'active' : ''}
            onClick={() => onChange({ ...side, mode: 'whole', fromMs: null, toMs: null })}
            disabled={!s}
          >
            Whole session
          </button>
          <button
            className={side.mode === 'range' ? 'active' : ''}
            onClick={() =>
              onChange({
                ...side,
                mode: 'range',
                fromMs: side.fromMs ?? start,
                toMs: side.toMs ?? Math.min(end, start + 5 * 60_000),
              })
            }
            disabled={!s}
          >
            Range
          </button>
        </div>

        {side.mode === 'range' && s ? (
          <div className="compare-range">
            <div className="pill-tabs">
              <button onClick={() => setRange(start, Math.min(end, start + 5 * 60_000))}>First 5 min</button>
              <button onClick={() => setRange(Math.max(start, end - 5 * 60_000), end)}>Last 5 min</button>
              <button onClick={matchDuration} disabled={!otherDuration} title="Match this range's duration to side B's">
                Match duration
              </button>
            </div>
            <div className="muted mono-sm">
              {formatDateTime(side.fromMs ?? start)} → {formatDateTime(side.toMs ?? end)} ·{' '}
              {formatDuration(rangeMs)}
            </div>
          </div>
        ) : null}

        {s ? (
          <div className="compare-facts mono-sm">
            <span>date {formatDateTime(s.startWallMs)}</span>
            <span>duration {formatDuration(s.durationS * 1000)}</span>
            <span>coverage {s.coveragePct === null ? 'unknown' : `${s.coveragePct.toFixed(1)}%`}</span>
            <span>interval {s.intervalMs} ms</span>
            <span
              className={
                s.coveragePct !== null && s.coveragePct < 70 ? 'warn-text' : undefined
              }
            >
              {s.mode}
              {s.recovered ? ' · recovered' : ''}
            </span>
            <span>collectors {s.collectors && s.collectors.length ? s.collectors.join(', ') : 'unknown'}</span>
          </div>
        ) : null}
      </div>
    </section>
  )
}

export function ComparisonSetup({
  sessions,
  a,
  b,
  onChange,
  onSwap,
  onCompare,
  canCompare,
  busy,
  compact = false,
}: {
  sessions: SessionListEntry[]
  a: SideConfig
  b: SideConfig
  onChange: (side: 'a' | 'b', cfg: SideConfig) => void
  onSwap: () => void
  onCompare: () => void
  canCompare: boolean
  busy: boolean
  compact?: boolean
}) {
  const recent = [...sessions].sort((x, y) => y.startWallMs - x.startWallMs).slice(0, 4)
  return (
    <div className={`compare-setup${compact ? ' compact' : ''}`}>
      <div className="compare-sides">
        <SideEditor
          title="A — baseline"
          color={COLOR_A}
          side={a}
          other={b}
          sessions={sessions}
          recent={recent}
          onChange={(cfg) => onChange('a', cfg)}
        />
        <div className="compare-swap">
          <button onClick={onSwap} disabled={!a.path && !b.path} title="Swap A and B">
            ⇄ Swap
          </button>
        </div>
        <SideEditor
          title="B — comparison"
          color={COLOR_B}
          side={b}
          other={a}
          sessions={sessions}
          recent={recent}
          onChange={(cfg) => onChange('b', cfg)}
        />
      </div>
      <div className="compare-actions">
        <button className="primary" onClick={onCompare} disabled={!canCompare || busy}>
          {busy ? 'Comparing…' : 'Compare'}
        </button>
        <span className="muted mono-sm">
          Differences are observational and preserve evidence-quality caveats.
        </span>
      </div>
    </div>
  )
}
