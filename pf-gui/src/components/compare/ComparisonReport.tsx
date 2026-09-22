import type {
  Caveat,
  CategoricalDifference,
  ComparisonAnalysis,
  CorrelationComparison,
  DistributionComparison,
  DomainComparison,
  EnergyComparison,
  MetricComparison,
  ProcessComparison,
  QualityComparison,
  RankedDifference,
} from '../../api/types'
import {
  comparabilityClass,
  comparabilityLabel,
  directionLabel,
  formatDelta,
  formatValue,
  pctText,
  reliabilityClass,
  reliabilityLabel,
} from '../../lib/compare'
import { formatDuration } from '../../lib/format'

/**
 * Comparability strip: the overall verdict plus every comparison-wide finding.
 * Weak evidence and incomparable conditions are deliberately prominent.
 */
export function ComparabilityStrip({ analysis }: { analysis: ComparisonAnalysis }) {
  const overall = analysis.comparability.overall
  const warn = overall === 'weak' || overall === 'not_comparable'
  return (
    <div className={`quality-strip comparability${warn ? ' weak' : ''}`} aria-label="Comparability">
      <span className="qs">
        Comparability{' '}
        <span className={`badge ${comparabilityClass(overall)}`}>{comparabilityLabel(overall)}</span>
      </span>
      <span className="qs">
        Coverage A{' '}
        <strong className={analysis.quality.coverageA !== null && analysis.quality.coverageA < 0.7 ? 'warn-text' : ''}>
          {pctText(analysis.quality.coverageA)}
        </strong>
      </span>
      <span className="qs">
        Coverage B{' '}
        <strong className={analysis.quality.coverageB !== null && analysis.quality.coverageB < 0.7 ? 'warn-text' : ''}>
          {pctText(analysis.quality.coverageB)}
        </strong>
      </span>
      <span className="qs">
        Weakest side <strong>{analysis.quality.weakestSide}</strong>
      </span>
      <span className="qs">
        Discontinuities <strong>{analysis.quality.discontinuitiesA}/{analysis.quality.discontinuitiesB}</strong>
      </span>
      {analysis.quality.staleCollectorsA.length || analysis.quality.staleCollectorsB.length ? (
        <span className="qs warn-text">
          No samples in range: A [{analysis.quality.staleCollectorsA.join(', ') || 'none'}] B [
          {analysis.quality.staleCollectorsB.join(', ') || 'none'}]
        </span>
      ) : null}
      {analysis.comparability.findings.map((f, i) => (
        <span key={`${f.metric ?? 'global'}-${i}`} className="quality-warning">
          {f.metric ? `${f.metric}: ` : ''}
          {f.reason}
        </span>
      ))}
    </div>
  )
}

export function QualityComparisonPanel({ quality }: { quality: QualityComparison }) {
  return (
    <section className="panel">
      <header>
        Side-by-side quality <span className="hint">evidence strength for each side</span>
      </header>
      <div className="content">
        <table className="mini-table compare-table">
          <thead>
            <tr>
              <th>Property</th>
              <th className="num">A</th>
              <th className="num">B</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>Coverage</td>
              <td className={`num ${quality.coverageA !== null && quality.coverageA < 0.7 ? 'warn-text' : ''}`}>
                {pctText(quality.coverageA)}
              </td>
              <td className={`num ${quality.coverageB !== null && quality.coverageB < 0.7 ? 'warn-text' : ''}`}>
                {pctText(quality.coverageB)}
              </td>
            </tr>
            <tr>
              <td>Discontinuities</td>
              <td className="num">{quality.discontinuitiesA}</td>
              <td className="num">{quality.discontinuitiesB}</td>
            </tr>
            <tr>
              <td>Timeouts</td>
              <td className="num">{quality.timeoutsA}</td>
              <td className="num">{quality.timeoutsB}</td>
            </tr>
            <tr>
              <td>Stale collectors</td>
              <td className="num">{quality.staleCollectorsA.join(', ') || 'none'}</td>
              <td className="num">{quality.staleCollectorsB.join(', ') || 'none'}</td>
            </tr>
          </tbody>
        </table>
        <div className="muted mono-sm" style={{ padding: '4px 0' }}>
          {quality.note}
        </div>
      </div>
    </section>
  )
}

/**
 * Headline comparison. Direction is neutral: an increase is not "worse".
 * A percentage is only shown when Rust supplied a defensible denominator.
 */
export function MetricComparisonTable({ metrics }: { metrics: MetricComparison[] }) {
  return (
    <section className="panel">
      <header>
        Headline differences <span className="hint">observational, not a scoreboard</span>
      </header>
      <div className="content flush">
        <div className="compare-grid compare-grid-head">
          <span>Metric</span>
          <span className="num">A</span>
          <span className="num">B</span>
          <span className="num">Δ (B − A)</span>
          <span>Evidence</span>
        </div>
        {metrics.map((m) => (
          <div className="compare-grid" key={m.metric}>
            <span>
              {m.label}
              <span className="muted mono-sm"> · {m.domain}</span>
            </span>
            <span className="num">{formatValue(m.a, m.unit)}</span>
            <span className="num">{formatValue(m.b, m.unit)}</span>
            <span className="num">
              {formatDelta(m.absoluteDelta, m.relativeDelta, m.unit)}
              {m.direction === 'unchanged' || m.direction === 'increased' || m.direction === 'decreased' ? (
                <span className="muted mono-sm"> {directionLabel(m.direction)}</span>
              ) : null}
            </span>
            <span className="compare-evidence">
              <span className={`badge ${comparabilityClass(m.comparability)}`} title={m.comparabilityReason}>
                {comparabilityLabel(m.comparability)}
              </span>{' '}
              <span className={`badge ${reliabilityClass(m.reliability.level)}`} title={m.reliability.note}>
                {reliabilityLabel(m.reliability.level)}
              </span>
              <span className="muted mono-sm">
                {' '}
                {m.evidence}; n {m.sampleCountA}/{m.sampleCountB}
              </span>
            </span>
          </div>
        ))}
      </div>
    </section>
  )
}

export function RankedDifferences({ ranked }: { ranked: RankedDifference[] }) {
  return (
    <section className="panel">
      <header>
        Largest observed differences{' '}
        <span className="hint">ranked by magnitude relative to the noise floor</span>
      </header>
      <div className="content">
        {ranked.length ? (
          <div className="list">
            {ranked.map((r) => (
              <div className="row" key={r.metric}>
                <span className={`badge ${comparabilityClass(r.comparability)}`}>
                  {comparabilityLabel(r.comparability)}
                </span>
                <span style={{ minWidth: 170 }}>{r.label}</span>
                <span className="mono-sm">
                  {formatDelta(r.delta, r.relativeDelta, r.unit)} {directionLabel(r.direction)}
                </span>
                <span className={`badge ${reliabilityClass(r.reliability)}`} style={{ marginLeft: 'auto' }}>
                  {reliabilityLabel(r.reliability)}
                </span>
              </div>
            ))}
          </div>
        ) : (
          <div className="empty">
            No comparable, reliable difference was observed. Unavailable or unreliable metrics are not ranked.
          </div>
        )}
        <div className="muted mono-sm" style={{ paddingTop: 6 }}>
          Direction is context-dependent: an increase is not automatically worse, and a decrease is not
          automatically an improvement.
        </div>
      </div>
    </section>
  )
}

export function EnergyComparisonPanel({ energy, a, b }: { energy: EnergyComparison; a: number; b: number }) {
  return (
    <section className="panel">
      <header>
        Energy <span className="hint">{energy.preferredBasis === 'total_energy' ? 'similar durations' : 'durations differ — normalized basis'}</span>
      </header>
      <div className="content">
        <table className="mini-table compare-table">
          <thead>
            <tr>
              <th>Quantity</th>
              <th className="num">A</th>
              <th className="num">B</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>Total discharge energy</td>
              <td className="num">{energy.totalWhA === null ? '—' : `${energy.totalWhA.toFixed(4)} Wh`}</td>
              <td className="num">{energy.totalWhB === null ? '—' : `${energy.totalWhB.toFixed(4)} Wh`}</td>
            </tr>
            <tr>
              <td>Average discharge power</td>
              <td className="num">{energy.avgPowerWA === null ? '—' : `${energy.avgPowerWA.toFixed(2)} W`}</td>
              <td className="num">{energy.avgPowerWB === null ? '—' : `${energy.avgPowerWB.toFixed(2)} W`}</td>
            </tr>
            <tr>
              <td>Energy per hour (normalized)</td>
              <td className="num">{energy.normalizedWhA === null ? '—' : `${energy.normalizedWhA.toFixed(3)} Wh/h`}</td>
              <td className="num">{energy.normalizedWhB === null ? '—' : `${energy.normalizedWhB.toFixed(3)} Wh/h`}</td>
            </tr>
            <tr>
              <td>Duration</td>
              <td className="num">{formatDuration(a * 1000)}</td>
              <td className="num">{formatDuration(b * 1000)}</td>
            </tr>
          </tbody>
        </table>
        <div className="quality-warning">{energy.note}</div>
      </div>
    </section>
  )
}

export function DomainComparisonPanel({ domains }: { domains: DomainComparison[] }) {
  return (
    <section className="panel">
      <header>
        Subsystem comparison <span className="hint">missing domains stay visible</span>
      </header>
      <div className="content">
        <div className="domain-grid">
          {domains.map((d) => (
            <div className={`domain-card${d.availableA && d.availableB ? '' : ' unavailable'}`} key={d.domain}>
              <div className="domain-title">
                {d.domain}
                {!d.availableA ? <span className="badge unavailable">A missing</span> : null}
                {!d.availableB ? <span className="badge unavailable">B missing</span> : null}
              </div>
              {d.unavailableReason ? (
                <div className="muted mono-sm">{d.unavailableReason}</div>
              ) : (
                <table className="mini-table">
                  <tbody>
                    {d.metrics.map((m) => (
                      <tr key={m.metric}>
                        <td>{m.label}</td>
                        <td className="num">{formatValue(m.aMedian, m.unit)}</td>
                        <td className="num">{formatValue(m.bMedian, m.unit)}</td>
                        <td className="num">
                          {m.delta === null ? '—' : formatDelta(m.delta, null, m.unit)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}

export function CategoricalPanel({ items }: { items: CategoricalDifference[] }) {
  return (
    <section className="panel">
      <header>
        Context differences <span className="hint">same / changed / unavailable — never forced deltas</span>
      </header>
      <div className="content">
        <div className="list">
          {items.map((c) => (
            <div className="row" key={c.key}>
              <span
                className={`badge ${
                  c.state === 'changed' ? 'derived' : c.state === 'same' ? 'available' : 'unavailable'
                }`}
              >
                {c.state}
              </span>
              <span style={{ minWidth: 150 }}>{c.label}</span>
              <span className="mono-sm">
                {c.a ?? '—'} → {c.state === 'same' ? '(same)' : (c.b ?? '—')}
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

export function ProcessComparisonTable({ processes }: { processes: ProcessComparison }) {
  const incomplete = processes.incomplete
  return (
    <section className="panel">
      <header>
        Process comparison{' '}
        <span className="hint">
          {processes.aTicks}/{processes.bTicks} snapshots in range
        </span>
      </header>
      <div className="content flush">
        {incomplete ? (
          <div className="quality-warning">
            Process evidence is incomplete on at least one side. An absent process is{' '}
            <strong>not observed</strong>, never proof it did not run.
            {processes.aIncomplete ? ' A incomplete.' : ''}
            {processes.bIncomplete ? ' B incomplete.' : ''}
          </div>
        ) : processes.aIncomplete === null || processes.bIncomplete === null ? (
          <div className="notice-inline">
            Process enumeration completeness was not recorded on at least one side.
          </div>
        ) : null}
        <div className="proc-table">
          <div className="proc-row proc-head">
            <span style={{ flex: 1 }}>Process</span>
            <span className="num">A CPU</span>
            <span className="num">B CPU</span>
            <span className="num">Δ</span>
            <span style={{ minWidth: 120 }}>Presence</span>
          </div>
          {processes.rows.map((p) => (
            <div className="proc-row" key={p.identity}>
              <span style={{ flex: 1 }}>
                {p.displayName}
                {p.aAmbiguous || p.bAmbiguous ? (
                  <span className="badge degraded" title="More than one executable matched this name">
                    ambiguous name
                  </span>
                ) : null}
              </span>
              <span className="num mono-sm">
                {p.aCpuMedianPct === null ? '—' : `${p.aCpuMedianPct.toFixed(1)}%`}
              </span>
              <span className="num mono-sm">
                {p.bCpuMedianPct === null ? '—' : `${p.bCpuMedianPct.toFixed(1)}%`}
              </span>
              <span className="num mono-sm">
                {p.delta === null ? '—' : `${p.delta > 0 ? '+' : ''}${p.delta.toFixed(1)} pp`}
              </span>
              <span className="mono-sm muted" title={`A ${p.aPresence}/${p.aTicks}, B ${p.bPresence}/${p.bTicks}`}>
                {p.presence === 'both' ? 'both' : p.presence === 'a_only' ? 'A only' : 'B only'} · {p.note}
              </span>
            </div>
          ))}
          {processes.rows.length === 0 ? (
            <div className="empty">No process evidence in either range.</div>
          ) : null}
        </div>
      </div>
    </section>
  )
}

export function DistributionPanel({ distributions }: { distributions: DistributionComparison[] }) {
  return (
    <section className="panel">
      <header>
        Distribution evidence <span className="hint">medians alone hide instability</span>
      </header>
      <div className="content">
        <table className="mini-table compare-table">
          <thead>
            <tr>
              <th>Metric</th>
              <th className="num">Side</th>
              <th className="num">median</th>
              <th className="num">p10</th>
              <th className="num">p90</th>
              <th className="num">spread</th>
              <th className="num">known/n</th>
            </tr>
          </thead>
          <tbody>
            {distributions.map((d) => (
              <RowGroup key={d.metric} d={d} />
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}

function RowGroup({ d }: { d: DistributionComparison }) {
  const row = (side: 'A' | 'B', s: DistributionComparison['a']) => (
    <tr>
      <td>{side === 'A' ? d.label : ''}</td>
      <td className="num">{side}</td>
      <td className="num">{formatValue(s.median, d.unit)}</td>
      <td className="num">{formatValue(s.p10, d.unit)}</td>
      <td className="num">{formatValue(s.p90, d.unit)}</td>
      <td className="num">{s.spread === null ? '—' : formatValue(s.spread, d.unit)}</td>
      <td className="num muted mono-sm">
        {s.known}/{s.n}
      </td>
    </tr>
  )
  return (
    <>
      {row('A', d.a)}
      {row('B', d.b)}
    </>
  )
}

export function CorrelationComparisonPanel({ correlations }: { correlations: CorrelationComparison[] }) {
  return (
    <section className="panel">
      <header>
        Relationship comparison <span className="hint">correlation only — not causation</span>
      </header>
      <div className="content">
        <div className="list">
          {correlations.map((c) => (
            <div className="row" key={`${c.x}-${c.y}`}>
              <span className={`badge ${c.comparable ? 'available' : 'unavailable'}`}>
                {c.comparable ? 'comparable' : 'insufficient'}
              </span>
              <span style={{ minWidth: 200 }}>{c.label}</span>
              <span className="mono-sm" style={{ minWidth: 150 }}>
                A r {c.rA === null ? '—' : c.rA.toFixed(2)} (eff {c.effectiveNA}) · B r{' '}
                {c.rB === null ? '—' : c.rB.toFixed(2)} (eff {c.effectiveNB})
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

export function CaveatPanel({ caveats }: { caveats: Caveat[] }) {
  if (!caveats.length) {
    return (
      <section className="panel">
        <header>What cannot legitimately be compared</header>
        <div className="content">
          <div className="muted">No caveats were raised for this comparison.</div>
        </div>
      </section>
    )
  }
  return (
    <section className="panel">
      <header>
        What cannot legitimately be compared{' '}
        <span className="hint">explicit, not hidden in a tooltip</span>
      </header>
      <div className="content">
        <div className="list">
          {caveats.map((c, i) => (
            <div className="row" key={`${c.scope}-${i}`}>
              <span
                className={`badge ${
                  c.severity === 'critical' ? 'error' : c.severity === 'warning' ? 'degraded' : 'muted'
                }`}
              >
                {c.severity}
              </span>
              <span className="muted mono-sm">{c.scope}</span>
              <span style={{ flex: 1 }}>{c.message}</span>
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}
