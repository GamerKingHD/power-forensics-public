import type { InterestingRegion, TimelineEvent } from '../../api/types'
import { formatDateTime } from '../../lib/format'
import { sessionClock } from './DetailsInspector'

const REGION_BADGE: Record<string, string> = {
  peak: 'degraded',
  sustained_increase: 'derived',
  marker: 'available',
  discontinuity: 'error',
}

const SEVERITY_BADGE: Record<string, string> = {
  marker: 'available',
  warning: 'degraded',
  error: 'error',
  info: 'muted',
}

/**
 * Candidate interesting regions found by the deterministic Rust detector.
 * They are candidates, not anomalies: no anomaly model exists.
 */
export function RegionList({
  regions,
  startWallMs,
  onJump,
}: {
  regions: InterestingRegion[]
  startWallMs: number
  onJump: (startMs: number, endMs: number) => void
}) {
  return (
    <section className="panel">
      <header>
        Interesting regions <span className="hint">deterministic candidates</span>
      </header>
      <div className="content flush">
        <div className="list">
          {regions.length ? (
            regions.map((r, i) => (
              <button
                className="row region-row"
                key={`${r.kind}-${r.startMs}-${i}`}
                onClick={() => onJump(r.startMs, r.endMs)}
                title={r.detail}
              >
                <span className={`badge ${REGION_BADGE[r.kind] ?? 'muted'}`}>{r.kind.replace('_', ' ')}</span>
                <span style={{ flex: 1, textAlign: 'left' }}>{r.label}</span>
                <span className="mono-sm muted">{sessionClock(r.startMs, startWallMs)}</span>
                <span className="mono-sm muted">score {r.score.toFixed(1)}</span>
              </button>
            ))
          ) : (
            <div className="empty">No candidate regions detected.</div>
          )}
        </div>
      </div>
    </section>
  )
}

/**
 * The event rail is a forensic track: exact recorded positions, never derived
 * in the frontend. Clicking centers the timeline and opens the event.
 */
export function EventRail({
  events,
  startWallMs,
  activeWallMs,
  onPick,
}: {
  events: TimelineEvent[]
  startWallMs: number
  activeWallMs: number | null
  onPick: (e: TimelineEvent) => void
}) {
  return (
    <div className="event-rail analyze-events" aria-label="Recorded events">
      {events.length ? (
        events.map((e, i) => (
          <button
            key={`${e.wallMs}-${i}`}
            className={`event-chip ${e.severity}${activeWallMs === e.wallMs ? ' active' : ''}`}
            onClick={() => onPick(e)}
            title={`${formatDateTime(e.wallMs)} · ${e.kind}${e.detail ? `: ${e.detail}` : ''}`}
          >
            <span className="mono-sm">{sessionClock(e.wallMs, startWallMs)}</span> {e.kind}
            {e.detail ? <span className="muted"> {e.detail}</span> : null}
          </button>
        ))
      ) : (
        <span className="muted">no events recorded</span>
      )}
    </div>
  )
}

export function EventBadge({ severity }: { severity: string }) {
  return <span className={`badge ${SEVERITY_BADGE[severity] ?? 'muted'}`}>{severity}</span>
}
