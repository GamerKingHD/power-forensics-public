import type { Provenance, Quality, RangeAnalysis } from '../../api/types'
import { EvidenceBadge } from '../evidence'
import { displayUnit, formatDateTime, formatNumber } from '../../lib/format'

export interface CursorReading {
  label: string
  unit: string
  value: number | null
  provenance: Provenance
  quality: Quality
  tMs: number
}

/** Session-relative clock, e.g. 00:05:00; wall clock stays one hover away. */
export function sessionClock(ms: number, startWallMs: number): string {
  const s = Math.max(0, Math.floor((ms - startWallMs) / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  return `${h.toString().padStart(2, '0')}:${m.toString().padStart(2, '0')}:${sec.toString().padStart(2, '0')}`
}

function valueText(r: CursorReading): string {
  if (r.value === null) return '—'
  const u = displayUnit(r.unit)
  return `${formatNumber(r.value, r.unit)}${u ? ` ${u}` : ''}`
}

export function DetailsInspector({
  cursorMs,
  startWallMs,
  readings,
  selection,
  analysis,
}: {
  cursorMs: number | null
  startWallMs: number
  readings: CursorReading[]
  selection: [number, number] | null
  analysis: RangeAnalysis | null
}) {
  return (
    <section className="panel inspector">
      <header>
        Inspector{' '}
        <span className="hint">{selection ? 'selection' : cursorMs !== null ? 'cursor' : 'idle'}</span>
      </header>
      <div className="content">
        {selection ? (
          <div className="kv">
            <div className="k">Selected start</div>
            <div className="v" title={formatDateTime(selection[0])}>
              {sessionClock(selection[0], startWallMs)} · {formatDateTime(selection[0])}
            </div>
            <div className="k">Selected end</div>
            <div className="v" title={formatDateTime(selection[1])}>
              {sessionClock(selection[1], startWallMs)} · {formatDateTime(selection[1])}
            </div>
            <div className="k">Duration</div>
            <div className="v">{((selection[1] - selection[0]) / 1000).toFixed(1)}s</div>
            {analysis ? (
              <>
                <div className="k">Coverage</div>
                <div className="v">
                  {analysis.quality.coverage === null
                    ? 'unknown'
                    : `${(analysis.quality.coverage * 100).toFixed(1)}%`}
                </div>
                <div className="k">Discharge energy</div>
                <div className="v">
                  {analysis.energy.dischargeWh === null
                    ? '—'
                    : `${analysis.energy.dischargeWh.toFixed(4)} Wh`}
                </div>
              </>
            ) : (
              <div className="muted mono-sm">Analyzing interval…</div>
            )}
          </div>
        ) : cursorMs !== null ? (
          <div className="cursor-block">
            <div className="cursor-time" title={formatDateTime(cursorMs)}>
              {sessionClock(cursorMs, startWallMs)} <span className="muted">{formatDateTime(cursorMs)}</span>
            </div>
            <div className="list">
              {readings.map((r) => (
                <div className="row cursor-reading" key={r.label} title={formatDateTime(r.tMs)}>
                  <span style={{ flex: 1 }}>{r.label}</span>
                  <span className={`mono-sm${r.value === null ? ' muted' : ''}`}>{valueText(r)}</span>
                  <EvidenceBadge provenance={r.provenance} quality={r.quality} />
                </div>
              ))}
              {readings.length === 0 ? <div className="empty">No visible tracks.</div> : null}
            </div>
          </div>
        ) : (
          <div className="empty">
            Hover the timeline to inspect a moment, or drag to select an interval.
          </div>
        )}
      </div>
    </section>
  )
}
