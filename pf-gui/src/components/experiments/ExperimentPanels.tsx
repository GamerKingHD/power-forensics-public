import type {
  ExperimentAnalysis,
  RunOutcome,
  SecondaryComparison,
  UnavailableRun,
} from '../../api/types'
import {
  classificationClass,
  classificationLabel,
  confounderClass,
  validityClass,
  validityLabel,
} from '../../lib/experiments'
import { formatDuration } from '../../lib/format'
import { formatValue } from '../../lib/compare'

function pct(v: number | null): string {
  return v === null || v === undefined ? 'unknown' : `${(v * 100).toFixed(1)}%`
}

export function ValidityStrip({ analysis }: { analysis: ExperimentAnalysis }) {
  const warn =
    analysis.validity === 'confounded' ||
    analysis.validity === 'insufficient' ||
    analysis.validity === 'weak'
  return (
    <div className={`quality-strip experiment-validity${warn ? ' weak' : ''}`} aria-label="Experiment validity">
      <span className="qs">
        Result{' '}
        <span className={`badge ${classificationClass(analysis.classification)}`}>
          {classificationLabel(analysis.classification)}
        </span>
      </span>
      <span className="qs">
        Validity <span className={`badge ${validityClass(analysis.validity)}`}>{validityLabel(analysis.validity)}</span>
      </span>
      <span className="qs">
        Baseline runs <strong>{analysis.baseline.runsWithPrimary}</strong>
      </span>
      <span className="qs">
        Treatment runs <strong>{analysis.treatment.runsWithPrimary}</strong>
      </span>
      <span className="qs">
        Excluded <strong>{analysis.baseline.runsExcluded + analysis.treatment.runsExcluded}</strong>
      </span>
      <span className="qs">
        Pairing <strong>{analysis.pairing}</strong>
      </span>
      {analysis.unavailableRuns.length ? (
        <span className="qs warn-text">
          Unavailable runs <strong>{analysis.unavailableRuns.length}</strong>
        </span>
      ) : null}
      {analysis.validityReasons.map((r, i) => (
        <span className="quality-warning" key={i}>
          {r}
        </span>
      ))}
    </div>
  )
}

export function PrimaryResultPanel({ analysis }: { analysis: ExperimentAnalysis }) {
  const e = analysis.effect
  const unit = analysis.primaryUnit
  const delta = e.absoluteDelta
  const rel = e.relativeDelta
  return (
    <section className="panel experiment-primary">
      <header>
        Primary result <span className="hint">{analysis.primaryLabel} · direction is wording only</span>
      </header>
      <div className="content">
        <div className="effect-row">
          <div className="effect-cell">
            <span className="k">Baseline typical</span>
            <span className="v">{formatValue(e.baselineValue, unit)}</span>
          </div>
          <div className="effect-cell">
            <span className="k">Treatment typical</span>
            <span className="v">{formatValue(e.treatmentValue, unit)}</span>
          </div>
          <div className="effect-cell">
            <span className="k">Observed difference</span>
            <span className="v">
              {delta === null ? '—' : `${delta > 0 ? '+' : ''}${delta.toFixed(3)} ${unit}`}
              {rel !== null ? ` (${rel > 0 ? '+' : ''}${(rel * 100).toFixed(1)}%)` : ''}
            </span>
          </div>
          <div className="effect-cell">
            <span className="k">Run-to-run noise</span>
            <span className="v">
              {analysis.noise.estimate === null ? 'unknown' : `±${analysis.noise.estimate.toFixed(3)} ${unit}`}
            </span>
          </div>
          <div className="effect-cell">
            <span className="k">Effect / noise</span>
            <span className="v">{e.effectSize === null ? '—' : `${e.effectSize.toFixed(2)}×`}</span>
          </div>
          <div className="effect-cell">
            <span className="k">Bootstrap 5–95%</span>
            <span className="v">
              {e.confidenceInterval === null
                ? 'not enough runs'
                : `[${e.confidenceInterval[0].toFixed(2)}, ${e.confidenceInterval[1].toFixed(2)}]`}
            </span>
          </div>
        </div>
        <ul className="result-lines">
          {analysis.summaryLines.map((line, i) => (
            <li key={i}>{line}</li>
          ))}
        </ul>
        <div className="muted mono-sm">{analysis.noise.note}</div>
      </div>
    </section>
  )
}

export function GroupSummaryPanel({ analysis }: { analysis: ExperimentAnalysis }) {
  const rows = [analysis.baseline, analysis.treatment]
  return (
    <section className="panel">
      <header>
        Group summaries <span className="hint">the experimental unit is the run</span>
      </header>
      <div className="content">
        <table className="mini-table compare-table">
          <thead>
            <tr>
              <th>Group</th>
              <th className="num">runs (used/total)</th>
              <th className="num">valid</th>
              <th className="num">typ.</th>
              <th className="num">mean</th>
              <th className="num">spread</th>
              <th className="num">min–max</th>
              <th className="num">coverage</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((g) => (
              <tr key={g.group}>
                <td>{g.group}</td>
                <td className="num">
                  {g.runsWithPrimary}/{g.runsIncluded}
                  {g.runsExcluded ? ` (+${g.runsExcluded} excl)` : ''}
                </td>
                <td className="num">{g.runsValid}</td>
                <td className="num">{formatValue(g.median, analysis.primaryUnit)}</td>
                <td className="num">{formatValue(g.mean, analysis.primaryUnit)}</td>
                <td className="num">{formatValue(g.spread, analysis.primaryUnit)}</td>
                <td className="num">
                  {formatValue(g.min, analysis.primaryUnit)}–{formatValue(g.max, analysis.primaryUnit)}
                </td>
                <td className="num">{pct(g.coverageMedian)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}

/** Compact dot plot. Paired designs connect each A/B pair. No zero-axis bars. */
export function RunDotPlot({ analysis }: { analysis: ExperimentAnalysis }) {
  const runs = analysis.runs.filter((r) => r.included && r.primaryValue !== null)
  const values = runs.map((r) => r.primaryValue as number)
  const paired = analysis.paired
  if (values.length === 0) {
    return (
      <section className="panel">
        <header>Run distribution</header>
        <div className="content">
          <div className="empty">No run with a primary value to plot.</div>
        </div>
      </section>
    )
  }
  const lo = Math.min(...values)
  const hi = Math.max(...values)
  const span = hi - lo || 1
  const w = 520
  const h = 180
  const pad = 34
  const y = (v: number) => h - pad - ((v - lo) / span) * (h - 2 * pad)
  const xA = paired ? w * 0.36 : w * 0.32
  const xB = paired ? w * 0.64 : w * 0.68
  return (
    <section className="panel">
      <header>
        Run distribution <span className="hint">individual run outcomes, not samples</span>
      </header>
      <div className="content">
        <svg className="run-plot" viewBox={`0 0 ${w} ${h}`} role="img" aria-label="Run outcome distribution">
          <line x1={pad} y1={y(analysis.baseline.median ?? lo)} x2={w - pad} y2={y(analysis.baseline.median ?? lo)} className="plot-median" />
          {paired
            ? paired.pairs.map((p) => (
                <line
                  key={p.pairId}
                  x1={xA}
                  y1={y(p.baseline)}
                  x2={xB}
                  y2={y(p.treatment)}
                  className={p.delta < 0 ? 'plot-pair lower' : 'plot-pair higher'}
                />
              ))
            : null}
          {runs
            .filter((r) => r.group === 'baseline')
            .map((r) => (
              <circle key={r.id} cx={xA} cy={y(r.primaryValue as number)} r={4} className="plot-dot baseline" />
            ))}
          {runs
            .filter((r) => r.group === 'treatment')
            .map((r) => (
              <circle key={r.id} cx={xB} cy={y(r.primaryValue as number)} r={4} className="plot-dot treatment" />
            ))}
          <text x={xA} y={h - 8} className="plot-label">
            Baseline
          </text>
          <text x={xB} y={h - 8} className="plot-label">
            Treatment
          </text>
          <text x={pad} y={14} className="plot-label">
            {formatValue(hi, analysis.primaryUnit)}
          </text>
          <text x={pad} y={h - pad + 12} className="plot-label">
            {formatValue(lo, analysis.primaryUnit)}
          </text>
        </svg>
        <div className="muted mono-sm">
          Raw run values remain inspectable in the run table. Each dot is one run-level outcome.
        </div>
      </div>
    </section>
  )
}

export function ConfounderPanel({ analysis }: { analysis: ExperimentAnalysis }) {
  return (
    <section className="panel">
      <header>
        Quality & confounders <span className="hint">a changed variable is not automatically invalid</span>
      </header>
      <div className="content">
        <div className="list">
          {analysis.confounders.map((c) => (
            <div className="row" key={c.key}>
              <span className={`badge ${confounderClass(c.state)}`}>{c.state}</span>
              <span style={{ minWidth: 150 }}>{c.label}</span>
              <span className="muted" style={{ flex: 1 }}>
                {c.evidence}
              </span>
              {c.runs.length ? <span className="muted mono-sm">runs: {c.runs.join(', ')}</span> : null}
            </div>
          ))}
        </div>
      </div>
    </section>
  )
}

export function SecondaryPanel({ secondary }: { secondary: SecondaryComparison[] }) {
  if (!secondary.length) return null
  return (
    <section className="panel">
      <header>
        Secondary differences <span className="hint">observed alongside the primary result</span>
      </header>
      <div className="content">
        <table className="mini-table compare-table">
          <thead>
            <tr>
              <th>Metric</th>
              <th className="num">Baseline</th>
              <th className="num">Treatment</th>
              <th className="num">Δ</th>
              <th>Evidence</th>
            </tr>
          </thead>
          <tbody>
            {secondary.map((s) => (
              <tr key={s.metric}>
                <td>{s.label || s.metric}</td>
                <td className="num">{formatValue(s.baselineMedian, s.unit)}</td>
                <td className="num">{formatValue(s.treatmentMedian, s.unit)}</td>
                <td className="num">{s.delta === null ? '—' : `${s.delta > 0 ? '+' : ''}${s.delta.toFixed(3)}`}</td>
                <td className="muted">{s.note}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}

export function ExperimentCaveats({ analysis }: { analysis: ExperimentAnalysis }) {
  return (
    <section className="panel">
      <header>
        Caveats <span className="hint">what is not justified by this evidence</span>
      </header>
      <div className="content">
        {analysis.caveats.length ? (
          <div className="list">
            {analysis.caveats.map((c, i) => (
              <div className="row" key={i}>
                <span style={{ flex: 1 }}>{c}</span>
              </div>
            ))}
          </div>
        ) : (
          <div className="muted">No caveats were raised for this experiment.</div>
        )}
      </div>
    </section>
  )
}

export interface RunTableProps {
  analysis: ExperimentAnalysis
  pendingIds: Set<string>
  onToggleIncluded: (runId: string, included: boolean) => void
  onAnalyze: (run: RunOutcome) => void
  onComparePair: (run: RunOutcome) => void
  pairedAvailable: boolean
}

export function RunTablePanel({
  analysis,
  pendingIds,
  onToggleIncluded,
  onAnalyze,
  onComparePair,
  pairedAvailable,
}: RunTableProps) {
  const all = [...analysis.runs].sort((a, b) => a.order - b.order)
  const unavailable = [...analysis.unavailableRuns].sort((a, b) => a.order - b.order)
  return (
    <section className="panel">
      <header>
        Individual runs <span className="hint">include/exclude is metadata; sessions are never deleted</span>
      </header>
      <div className="content flush">
        <div className="proc-table">
          <div className="proc-row exp-row exp-head">
            <span>Run</span>
            <span>Group</span>
            <span className="num">Order</span>
            <span className="num">Primary</span>
            <span className="num">Coverage</span>
            <span className="num">Duration</span>
            <span>Quality</span>
            <span>Included</span>
            <span>Actions</span>
          </div>
          {all.map((r) => (
            <div className="proc-row exp-row" key={r.id}>
              <span>
                {r.label}
                {r.pairId !== null ? <span className="muted mono-sm"> · pair {r.pairId}</span> : null}
              </span>
              <span className={`badge ${r.group === 'baseline' ? 'derived' : 'measured'}`}>{r.group}</span>
              <span className="num mono-sm">{r.order}</span>
              <span className="num mono-sm">{formatValue(r.primaryValue, analysis.primaryUnit)}</span>
              <span className={`num mono-sm ${r.coverage !== null && r.coverage < 0.7 ? 'warn-text' : ''}`}>
                {pct(r.coverage)}
              </span>
              <span className="num mono-sm">{formatDuration(r.durationS * 1000)}</span>
              <span>
                <span
                  className={`badge ${
                    r.validation.status === 'accepted'
                      ? 'available'
                      : r.validation.status === 'invalid'
                        ? 'error'
                        : 'degraded'
                  }`}
                  title={r.validation.findings.join('; ') || 'accepted'}
                >
                  {r.validation.status.replace('_', ' ')}
                </span>
                {r.validation.findings.length ? (
                  <span className="muted mono-sm"> {r.validation.findings[0]}</span>
                ) : null}
              </span>
              <span>
                <input
                  type="checkbox"
                  checked={r.included}
                  aria-label={`Include run ${r.label}`}
                  onChange={(e) => onToggleIncluded(r.id, e.target.checked)}
                />
              </span>
              <span className="run-actions">
                <button onClick={() => onAnalyze(r)}>Analyze</button>
                {pairedAvailable && r.pairId !== null ? (
                  <button onClick={() => onComparePair(r)}>Compare pair</button>
                ) : null}
              </span>
            </div>
          ))}
          {unavailable.map((u: UnavailableRun) => (
            <div className="proc-row exp-row unavailable" key={u.id}>
              <span>{u.label}</span>
              <span className={`badge ${u.group === 'baseline' ? 'derived' : 'measured'}`}>{u.group}</span>
              <span className="num mono-sm">{u.order}</span>
              <span className="warn-text" style={{ gridColumn: 'span 5' }}>
                {pendingIds.has(u.id) ? 'not recorded yet' : `unavailable — ${u.reason}`}
              </span>
              <span />
              <span />
            </div>
          ))}
          {all.length === 0 && unavailable.length === 0 ? (
            <div className="empty">No runs yet. Add existing sessions or start a guided experiment.</div>
          ) : null}
        </div>
      </div>
    </section>
  )
}
