import type {
  CategoryChange,
  ChangeFact,
  Correlation,
  DomainStat,
  RangeEnergy,
} from '../../api/types'
import { displayUnit, formatNumber } from '../../lib/format'
import { directionLabel, strengthLabel } from '../../lib/analyze'

function fmt(v: number | null, unit: string): string {
  if (v === null || v === undefined) return '—'
  const n = formatNumber(v, unit)
  const u = displayUnit(unit)
  return u ? `${n} ${u}` : n
}

function secs(v: number): string {
  if (!Number.isFinite(v) || v <= 0) return '0s'
  if (v < 60) return `${v.toFixed(1)}s`
  return `${Math.floor(v / 60)}m ${Math.round(v % 60)}s`
}

const CONFIDENCE_BADGE: Record<string, string> = {
  high: 'available',
  medium: 'derived',
  low: 'degraded',
}

/**
 * "What changed here?" — before vs during/after. The backend decides the
 * facts; the UI only renders them with observational wording.
 */
export function ChangesPanel({
  changes,
  categorical,
}: {
  changes: ChangeFact[]
  categorical: CategoryChange[]
}) {
  return (
    <section className="panel">
      <header>
        What changed here?{' '}
        <span className="hint">observational comparison — not a causal claim</span>
      </header>
      <div className="content">
        {categorical.length ? (
          <div className="list">
            {categorical.map((c) => (
              <div className="row" key={`${c.domain}-${c.label}`}>
                <span className="badge derived">context</span>
                <span style={{ minWidth: 130 }}>{c.label}</span>
                <span className="muted mono-sm">
                  {c.before ?? '—'} → <strong>{c.during ?? '—'}</strong>
                  {c.after !== undefined && c.after !== c.during ? ` → ${c.after ?? '—'}` : ''}
                </span>
                <span className="muted" style={{ marginLeft: 'auto' }}>
                  {c.basis}
                </span>
              </div>
            ))}
          </div>
        ) : null}
        {changes.length ? (
          <div className="list">
            {changes.map((c) => (
              <div className="row change-row" key={c.key}>
                <span className={`badge ${CONFIDENCE_BADGE[c.confidence] ?? 'degraded'}`}>
                  {c.confidence}
                </span>
                <span style={{ minWidth: 170 }}>{c.label}</span>
                <span className="mono-sm">
                  before {fmt(c.reference, c.unit)} · during <strong>{fmt(c.during, c.unit)}</strong>
                  {c.after !== null ? ` · after ${fmt(c.after, c.unit)}` : ''}
                </span>
                <span className={`dir ${c.direction}`}>
                  {directionLabel(c.direction)}
                  {c.delta !== null ? ` ${c.delta > 0 ? '+' : ''}${fmt(c.delta, c.unit)}` : ''}
                </span>
                <span className="muted" style={{ marginLeft: 'auto' }}>
                  {c.basis} (n {c.nBefore}/{c.nDuring}
                  {c.nAfter ? `/${c.nAfter}` : ''})
                </span>
              </div>
            ))}
          </div>
        ) : categorical.length === 0 ? (
          <div className="empty">No statistically meaningful change was detected in this interval.</div>
        ) : null}
      </div>
    </section>
  )
}

export function EnergyPanel({ energy }: { energy: RangeEnergy }) {
  return (
    <section className="panel">
      <header>Range energy</header>
      <div className="content">
        <div className="kv">
          <div className="k">Discharge energy</div>
          <div className="v">
            {energy.dischargeWh === null
              ? energy.dischargePresent
                ? 'not integrable (unknown interval)'
                : 'no discharge recorded'
              : `${energy.dischargeWh.toFixed(4)} Wh`}
          </div>
          <div className="k">Charge energy</div>
          <div className="v">
            {energy.chargeWh === null
              ? energy.chargePresent
                ? 'not integrable (unknown interval)'
                : 'no charge recorded'
              : `${energy.chargeWh.toFixed(4)} Wh`}
          </div>
          <div className="k">Covered / unknown</div>
          <div className="v">
            {secs(energy.coveredS)} / {secs(energy.unknownS)}
          </div>
          <div className="k">Unobserved</div>
          <div className="v">{secs(energy.unobservedS)}</div>
          <div className="k">Discontinuities</div>
          <div className="v">{energy.discontinuities}</div>
        </div>
        {energy.crossesDiscontinuity ? (
          <div className="quality-warning">
            This interval crosses a clock discontinuity. Energy is qualified: only contiguous,
            known segments were integrated.
          </div>
        ) : null}
      </div>
    </section>
  )
}

export function DomainsPanel({ domains }: { domains: DomainStat[] }) {
  return (
    <section className="panel">
      <header>
        Subsystem evidence <span className="hint">selected interval</span>
      </header>
      <div className="content">
        <div className="domain-grid">
          {domains.map((d) => (
            <div className={`domain-card${d.available ? '' : ' unavailable'}`} key={d.domain}>
              <div className="domain-title">
                {d.domain}
                {!d.available ? <span className="badge unavailable">unavailable</span> : null}
              </div>
              {d.available ? (
                <table className="mini-table">
                  <tbody>
                    {d.metrics
                      .filter((m) => m.known > 0)
                      .map((m) => (
                        <tr key={m.key}>
                          <td title={`${m.provenance}; ${m.known}/${m.n} samples present`}>
                            {m.label}
                          </td>
                          <td className="num">{fmt(m.median, m.unit)}</td>
                          <td className="muted mono-sm">
                            {fmt(m.min, m.unit)}–{fmt(m.max, m.unit)}
                          </td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              ) : (
                <div className="muted mono-sm">no recorded evidence</div>
              )}
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}

export function CorrelationsPanel({ correlations }: { correlations: Correlation[] }) {
  return (
    <section className="panel">
      <header>
        Relationships <span className="hint">correlation only — not causation</span>
      </header>
      <div className="content">
        <div className="list">
          {correlations.map((c) => (
            <div className="row" key={`${c.x}-${c.y}`}>
              <span
                className={`badge ${
                  c.strength === 'insufficient'
                    ? 'unavailable'
                    : c.strength === 'strong' || c.strength === 'moderate'
                      ? 'available'
                      : 'degraded'
                }`}
              >
                {strengthLabel(c.strength)}
              </span>
              <span style={{ minWidth: 210 }}>{c.label}</span>
              <span className="mono-sm" style={{ minWidth: 90 }}>
                {c.r === null ? 'r —' : `r ${c.r.toFixed(2)}`}
              </span>
              <span className="muted mono-sm">
                n {c.n}, eff {c.effectiveN}, coverage {(c.coverage * 100).toFixed(0)}%
              </span>
              <span className="muted" style={{ marginLeft: 'auto' }}>
                {c.note}
              </span>
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}
