import { useCallback, useMemo, useState } from 'react'
import { TableVirtuoso } from 'react-virtuoso'
import { BridgeFailure, bridge } from '../api/bridge'
import type { SessionListEntry } from '../api/types'
import { usePolling } from '../hooks/usePolling'
import { formatBytes, formatDateTime, formatDuration } from '../lib/format'
import {
  filterAndSortSessions,
  type SessionModeFilter,
  type SessionSortKey,
} from '../lib/sessions'

interface Props {
  onOpen: (path: string) => void
  onAnalyze: (path: string) => void
  onCompare?: (path: string) => void
  onNotice: (msg: string | null) => void
}

function SessionPreview({ s, onOpen }: { s: SessionListEntry; onOpen: (p: string) => void }) {
  const cell = (k: string, v: React.ReactNode) => (
    <>
      <div className="k">{k}</div>
      <div className="v">{v}</div>
    </>
  )
  return (
    <section className="panel preview">
      <header>
        Details — {s.note || s.file}
        <span className="spacer" />
        <span className="mono-sm muted">{formatBytes(s.bytes)}</span>
        <button onClick={() => onOpen(s.path)}>Open session</button>
      </header>
      <div className="kv">
        {cell('File', <span className="mono-sm">{s.path}</span>)}
        {cell('State', s.mode)}
        {cell('Start', formatDateTime(s.startWallMs))}
        {cell('Duration', formatDuration(s.durationS * 1000))}
        {cell('Samples', s.samples)}
        {cell('Interval', `${s.intervalMs} ms`)}
        {cell('Energy (discharge)', s.dischargeWh === null ? '—' : `${s.dischargeWh.toFixed(3)} Wh`)}
        {cell(
          'Coverage',
          s.coveragePct === null ? '—' : `${s.coveragePct.toFixed(1)}%`,
        )}
        {cell('Discontinuities', s.discontinuities ?? '—')}
        {cell('Markers', s.markers ?? 'unknown')}
        {cell(
          'Collectors',
          s.collectors && s.collectors.length ? s.collectors.join(', ') : 'unknown',
        )}
      </div>
    </section>
  )
}

export function SessionsPage({ onOpen, onAnalyze, onCompare, onNotice }: Props) {
  const sessions = usePolling(bridge.listSessions, 5000)
  const [query, setQuery] = useState('')
  const [mode, setMode] = useState<SessionModeFilter>('all')
  const [lowCoverage, setLowCoverage] = useState(false)
  const [hasDiscontinuities, setHasDiscontinuities] = useState(false)
  const [sortKey, setSortKey] = useState<SessionSortKey>('startWallMs')
  const [asc, setAsc] = useState(false)
  const [busyPath, setBusyPath] = useState<string | null>(null)
  const [selected, setSelected] = useState<SessionListEntry | null>(null)
  const [rebuilding, setRebuilding] = useState(false)

  const rows = useMemo(
    () =>
      filterAndSortSessions(sessions.data ?? [], {
        query,
        mode,
        sortKey,
        asc,
        lowCoverage,
        hasDiscontinuities,
      }),
    [sessions.data, query, mode, sortKey, asc, lowCoverage, hasDiscontinuities],
  )

  const sortBy = useCallback(
    (key: SessionSortKey) => {
      if (key === sortKey) setAsc((v) => !v)
      else {
        setSortKey(key)
        setAsc(key === 'note' || key === 'mode' || key === 'file')
      }
    },
    [sortKey],
  )

  const act = useCallback(
    async (path: string, fn: (p: string) => Promise<string>, label: string) => {
      setBusyPath(path)
      try {
        const result = await fn(path)
        onNotice(`${label}: ${result}`)
      } catch (e) {
        const err = e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
        onNotice(`${label} failed [${err.code}]: ${err.message}`)
      } finally {
        setBusyPath(null)
      }
    },
    [onNotice],
  )

  const rebuild = useCallback(async () => {
    setRebuilding(true)
    try {
      await bridge.rebuildSessionIndex()
      sessions.refresh()
      onNotice('Session index rebuilt from the authoritative JSONL.')
    } catch (e) {
      const err = e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
      onNotice(`Index rebuild failed [${err.code}]: ${err.message}`)
    } finally {
      setRebuilding(false)
    }
  }, [onNotice, sessions])

  const th = (key: SessionSortKey, label: string, extra?: string) => (
    <th
      className={extra}
      onClick={() => sortBy(key)}
      title={`Sort by ${label}`}
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          sortBy(key)
        }
      }}
    >
      {label}
      {sortKey === key ? (asc ? ' ▲' : ' ▼') : ''}
    </th>
  )

  return (
    <>
      <div className="page-title">
        <h1>Sessions</h1>
        <span className="hint">Recorded evidence discovered in the session store</span>
      </div>

      {sessions.error ? (
        <div className="error-box">
          <div className="code">{sessions.error.code}</div>
          <div>{sessions.error.message}</div>
          {sessions.error.detail ? (
            <details>
              <summary>Technical detail</summary>
              <pre className="mono-sm">{sessions.error.detail}</pre>
            </details>
          ) : null}
        </div>
      ) : null}

      <div className="toolbar">
        <input
          placeholder="search file, label or path  (Ctrl+F)"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          style={{ width: 260 }}
        />
        <div className="pill-tabs">
          {(['all', 'ok', 'incomplete', 'recovered'] as const).map((m) => (
            <button key={m} className={mode === m ? 'active' : ''} onClick={() => setMode(m)}>
              {m}
            </button>
          ))}
        </div>
        <label className="muted check">
          <input
            type="checkbox"
            checked={lowCoverage}
            onChange={(e) => setLowCoverage(e.target.checked)}
          />
          low coverage
        </label>
        <label className="muted check">
          <input
            type="checkbox"
            checked={hasDiscontinuities}
            onChange={(e) => setHasDiscontinuities(e.target.checked)}
          />
          discontinuities
        </label>
        <span className="spacer" />
        <span className="count">
          {rows.length} session(s)
          {sessions.loading ? ' · loading…' : ''}
        </span>
        <button onClick={rebuild} disabled={rebuilding} title="Rebuild the derived index from JSONL">
          {rebuilding ? 'Rebuilding…' : 'Rebuild index'}
        </button>
        <button onClick={sessions.refresh}>Refresh</button>
      </div>

      <section className="panel">
        <div className="content flush" style={{ height: selected ? 'calc(100vh - 430px)' : 'calc(100vh - 250px)' }}>
          <TableVirtuoso
            data={rows}
            style={{ height: '100%' }}
            components={{
              Table: (props) => <table {...props} className="grid" />,
            }}
            fixedHeaderContent={() => (
              <tr>
                {th('startWallMs', 'Start')}
                {th('durationS', 'Duration')}
                {th('note', 'Label')}
                {th('mode', 'State')}
                {th('samples', 'Samples', 'num')}
                {th('dischargeWh', 'Energy Wh', 'num')}
                {th('coveragePct', 'Coverage', 'num')}
                {th('discontinuities', 'Disc.', 'num')}
                {th('markers', 'Markers', 'num')}
                <th className="nosort">Collectors</th>
                {th('bytes', 'Size', 'num')}
                {th('file', 'File')}
                <th className="nosort actions-head">Actions</th>
              </tr>
            )}
            itemContent={(_index, s) => (
              <>
                <td title={formatDateTime(s.startWallMs)}>{formatDateTime(s.startWallMs)}</td>
                <td>{formatDuration(s.durationS * 1000)}</td>
                <td className="name label-cell">
                  <button
                    className={`ghost${selected?.path === s.path ? ' row-selected' : ''}`}
                    onClick={() => setSelected(s)}
                    aria-pressed={selected?.path === s.path}
                  >
                    {s.note || s.file}
                  </button>
                </td>
                <td>
                  <span
                    className={`badge ${
                      s.mode === 'recorded' ? 'available' : s.mode === 'recovered' ? 'derived' : 'degraded'
                    }`}
                  >
                    {s.mode}
                  </span>
                </td>
                <td className="num">{s.samples}</td>
                <td className="num">{s.dischargeWh === null ? '—' : s.dischargeWh.toFixed(3)}</td>
                <td className="num">{s.coveragePct === null ? '—' : `${s.coveragePct.toFixed(1)}%`}</td>
                <td className="num">{s.discontinuities ?? '—'}</td>
                <td className="num">{s.markers === null ? '—' : s.markers}</td>
                <td className="muted mono-sm collectors-cell" title={s.collectors?.join(', ')}>
                  {s.collectors && s.collectors.length ? s.collectors.join(', ') : '—'}
                </td>
                <td className="num">{formatBytes(s.bytes)}</td>
                <td className="file-cell" title={s.path}>{s.file}</td>
                <td className="actions-cell">
                  <button className="ghost" onClick={() => onOpen(s.path)}>
                    Open
                  </button>{' '}
                  <button className="ghost" onClick={() => onAnalyze(s.path)} title="Open the Analyze workspace">
                    Analyze
                  </button>{' '}
                  {onCompare ? (
                    <button className="ghost" onClick={() => onCompare(s.path)} title="Compare this session against a baseline">
                      Compare
                    </button>
                  ) : null}{' '}
                  {s.mode === 'incomplete' ? (
                    <button
                      disabled={busyPath === s.path}
                      onClick={() => void act(s.path, bridge.recoverSession, 'Recover')}
                      title="Rebuild a footer on a copy; the original is untouched"
                    >
                      Recover
                    </button>
                  ) : null}{' '}
                  <button
                    className="ghost"
                    disabled={busyPath === s.path}
                    onClick={() => void act(s.path, bridge.exportSession, 'Export')}
                    title="Export long-format CSV with provenance, next to the session"
                  >
                    Export
                  </button>
                </td>
              </>
            )}
          />
        </div>
      </section>

      {selected ? <SessionPreview s={selected} onOpen={onOpen} /> : null}
    </>
  )
}
