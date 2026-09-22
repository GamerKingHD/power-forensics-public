import type { CollectorState, EvidenceValue as EvidenceValueType, MetricEvidence, Provenance, Quality } from '../../api/types'
import { displayUnit, formatAge, formatNumber, provenanceLabel, qualityLabel } from '../../lib/format'

function badgeClass(p: Provenance): string {
  return `badge ${p}`
}

/** One short line per provenance kind, shown on hover. No essay. */
const PROVENANCE_HELP: Record<Provenance, string> = {
  measured: 'Measured — direct collector observation',
  derived: 'Derived — computed from recorded evidence',
  estimated: 'Estimated — model/calibration based',
  unavailable: 'Unavailable — not observed',
}

/** Provenance chip. Color is never the only signal: the label is always text. */
export function EvidenceBadge({
  provenance,
  quality,
  source,
  reason,
  ageMs,
}: {
  provenance: Provenance
  quality?: Quality
  source?: string
  reason?: string | null
  ageMs?: number | null
}) {
  const title = [
    PROVENANCE_HELP[provenance],
    source ? `source: ${source}` : undefined,
    quality ? `quality: ${qualityLabel(quality)}` : undefined,
    ageMs !== undefined && ageMs !== null ? `age: ${formatAge(ageMs)}` : undefined,
    reason ? `reason: ${reason}` : undefined,
  ]
    .filter(Boolean)
    .join(' · ')
  return (
    <span className={badgeClass(provenance)} title={title || undefined}>
      {provenanceLabel(provenance)}
    </span>
  )
}

/** Secondary quality marker (stale/repeated/error/unknown). Hidden when fresh. */
export function QualityIndicator({ quality }: { quality: Quality }) {
  if (quality === 'fresh') return null
  const cls = quality === 'stale' ? 'stale' : quality === 'error' ? 'error' : 'warn'
  return (
    <span className={`badge ${cls}`} title={`Sample quality: ${qualityLabel(quality)}`}>
      {qualityLabel(quality)}
    </span>
  )
}

/**
 * Subtle age of an independently sampled reading. Renders nothing when age is
 * unknown. The exact age is always available in the title for inspection.
 */
export function AgeIndicator({ ageMs }: { ageMs: number | null | undefined }) {
  const text = formatAge(ageMs)
  if (!text) return null
  return (
    <span className="ev-age" title={`Sampled ${text}`}>
      {text}
    </span>
  )
}

/** The human reason an evidence value is absent. Renders nothing when present. */
export function AvailabilityReason({ evidence }: { evidence: EvidenceValueType }) {
  if (evidence.value !== null && evidence.value !== undefined) return null
  const kind = evidence.absenceKind ? ` (${evidence.absenceKind})` : ''
  return (
    <span className="ev-reason" title={`${evidence.provenance}${kind}`}>
      {evidence.reason ?? 'no reason recorded'}
      {kind}
    </span>
  )
}

/**
 * A single provenance-aware reading. An unavailable value renders as a dash
 * with its reason; it is never shown as 0.
 */
export function EvidenceValue({
  evidence,
  compact = false,
}: {
  evidence: EvidenceValueType | null | undefined
  compact?: boolean
}) {
  if (!evidence) {
    return (
      <div className="evidence">
        <div className="ev-main">
          <span className="ev-value absent">—</span>
          <span className="badge unavailable">Unavailable</span>
        </div>
      </div>
    )
  }
  const absent = evidence.value === null || evidence.value === undefined
  const stale = evidence.quality === 'stale'
  return (
    <div className="evidence">
      <div className="ev-main">
        <span className={`ev-value${absent ? ' absent' : ''}${stale ? ' is-stale' : ''}`}>
          {absent ? '—' : formatNumber(evidence.value as number, evidence.unit)}
          {!absent && displayUnit(evidence.unit) ? (
            <span className="muted"> {displayUnit(evidence.unit)}</span>
          ) : null}
        </span>
        {!compact ? (
          <>
            <EvidenceBadge
              provenance={evidence.provenance}
              quality={evidence.quality}
              source={evidence.source}
              reason={evidence.reason}
              ageMs={evidence.ageMs}
            />
            <QualityIndicator quality={evidence.quality} />
            <AgeIndicator ageMs={evidence.ageMs} />
          </>
        ) : null}
      </div>
      {!compact ? <AvailabilityReason evidence={evidence} /> : null}
    </div>
  )
}

/** Headline cell used by Overview. */
export function MetricCell({ metric }: { metric: MetricEvidence }) {
  return (
    <div className="cell">
      <span className="label">{metric.label}</span>
      <EvidenceValue evidence={metric.evidence} />
    </div>
  )
}

const STATE_CLASS: Record<string, string> = {
  available: 'ok',
  degraded: 'warn',
  unavailable: 'error',
  stale: 'stale',
  timeout: 'warn',
  recording: 'record',
}

const STATE_BADGE: Record<string, string> = {
  available: 'available',
  degraded: 'degraded',
  unavailable: 'error',
  stale: 'stale',
  timeout: 'degraded',
  recording: 'recording',
}

/** Collector health with a state word, not just a colored dot. */
export function CollectorStateRow({ state, nowMs }: { state: CollectorState; nowMs?: number }) {
  const cls = STATE_CLASS[state.state] ?? 'muted'
  const badge = STATE_BADGE[state.state] ?? 'unavailable'
  const label = state.state.charAt(0).toUpperCase() + state.state.slice(1)
  const age =
    nowMs && state.lastSampleWallMs ? formatAge(Math.max(0, nowMs - state.lastSampleWallMs)) : ''
  return (
    <>
      <div className="row collector-row" title={state.reason ?? undefined}>
        <span className={`dot ${cls}`} aria-hidden />
        <span style={{ minWidth: 90 }}>{state.name}</span>
        <span className={`badge ${badge}`}>{label}</span>
        <span className="muted mono-sm" style={{ marginLeft: 'auto' }}>
          {state.samples} smp
          {state.observedHz ? ` · ${state.observedHz.toFixed(2)} Hz` : ''}
          {state.failures ? ` · ${state.failures} fail` : ''}
          {age ? ` · ${age}` : ''}
        </span>
        {state.elevationWouldHelp ? <span className="badge warn">elevation helps</span> : null}
      </div>
      {state.reason ? (
        <div className="row">
          <span className="muted mono-sm">{state.reason}</span>
        </div>
      ) : null}
    </>
  )
}
