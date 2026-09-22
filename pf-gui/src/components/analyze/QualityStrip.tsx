import type { RangeQuality, SessionStats } from '../../api/types'

function pct(v: number | null): string {
  return v === null || v === undefined ? '—' : `${(v * 100).toFixed(1)}%`
}

function secs(v: number): string {
  if (!Number.isFinite(v) || v <= 0) return '0s'
  if (v < 60) return `${v.toFixed(1)}s`
  const m = Math.floor(v / 60)
  return `${m}m ${Math.round(v % 60)}s`
}

/**
 * Quality must follow the range: a session can be 98% covered while the
 * selected 30 s is mostly unknown. The strip states the interval's own
 * coverage and refuses to look strong over weak evidence.
 */
export function RangeQualityStrip({ quality }: { quality: RangeQuality }) {
  const weak = quality.coverage === null || quality.coverage < 0.9
  return (
    <div className={`quality-strip${weak ? ' weak' : ''}`} aria-label="Selected range evidence quality">
      <span className="qs">
        Coverage{' '}
        <strong className={weak ? 'warn-text' : ''}>
          {quality.coverage === null ? 'unknown' : pct(quality.coverage)}
        </strong>
      </span>
      <span className="qs">
        Unknown <strong>{secs(quality.unknownS)}</strong>
      </span>
      <span className="qs">
        Unobserved <strong>{secs(quality.unobservedS)}</strong>
      </span>
      <span className="qs">
        Discontinuities <strong>{quality.discontinuities}</strong>
      </span>
      <span className="qs">
        Timeouts <strong>{quality.timeouts}</strong>
      </span>
      {quality.recoveries > 0 ? (
        <span className="qs">
          Recoveries <strong>{quality.recoveries}</strong>
        </span>
      ) : null}
      {quality.errors > 0 ? (
        <span className="qs warn-text">
          Collector errors <strong>{quality.errors}</strong>
        </span>
      ) : null}
      <span className="qs">
        Events <strong>{quality.events}</strong>
      </span>
      <span className="qs">
        Markers <strong>{quality.markers}</strong>
      </span>
      <span className="qs">
        Battery samples <strong>{quality.samples}</strong>
      </span>
      {quality.staleCollectors.length ? (
        <span className="qs warn-text" title={quality.staleCollectors.join(', ')}>
          No samples in range: <strong>{quality.staleCollectors.join(', ')}</strong>
        </span>
      ) : null}
      {weak ? (
        <span className="quality-warning">
          Evidence for this interval is weak — treat any summary below as qualified.
        </span>
      ) : null}
    </div>
  )
}

/** Whole-session index statistics; explicitly says when a value is unknown. */
export function SessionStatsStrip({ stats }: { stats: SessionStats }) {
  return (
    <div className="quality-strip" aria-label="Whole-session statistics">
      <span className="qs">
        Samples <strong>{stats.totalSamples}</strong>
      </span>
      <span className="qs" title="Distinct record timestamps across all collectors">
        Record times <strong>{stats.ticks}</strong>
      </span>
      <span className="qs">
        Observed rate{' '}
        <strong>{stats.observedHz === null ? 'unknown' : `${stats.observedHz.toFixed(2)} Hz`}</strong>
      </span>
      <span className="qs">
        Events <strong>{stats.events}</strong>
      </span>
      <span className="qs">
        Markers <strong>{stats.markers}</strong>
      </span>
      <span className="qs">
        Timeouts <strong>{stats.timeouts}</strong>
      </span>
      <span className="qs">
        Recoveries <strong>{stats.recoveries}</strong>
      </span>
      <span className="qs">
        Discontinuities{' '}
        <strong>{stats.discontinuities === null ? 'unknown' : stats.discontinuities}</strong>
      </span>
    </div>
  )
}
