import { useCallback, useEffect, useMemo, useState } from 'react'
import { bridge } from '../api/bridge'
import type {
  CalibrationRecord,
  CalibrationReference,
  CalibrationDatasetRef,
  CreateCalibrationRequest,
  ReferenceKind,
  SessionListEntry,
} from '../api/types'
import { EmptyState } from '../components/layout/EmptyState'
import { ReportExportBar } from '../components/report/ReportExport'
import { formatDateTime } from '../lib/format'

const TARGET = 'display_power'

const REFERENCE_KINDS: { value: ReferenceKind; label: string }[] = [
  { value: 'external_meter', label: 'External meter (user-provided)' },
  { value: 'bench_meter', label: 'Bench meter (user-provided)' },
  { value: 'oem_reading', label: 'OEM reading (user-provided)' },
  { value: 'manual_reference', label: 'Manual reference (user-provided)' },
  { value: 'fitted_against_estimate', label: 'Fitted against another estimate (not independent)' },
]

function qualityBadge(q: string) {
  if (q === 'validated') return 'available'
  if (q === 'usable_with_caveats') return 'degraded'
  if (q === 'out_of_domain') return 'warn'
  return 'error'
}

function emptyReference(): CalibrationReference {
  return { input: 50, reference: 0, kind: 'external_meter', sourceLabel: '', note: '' }
}

export function CalibrationPage({ onNotice }: { onNotice: (msg: string | null) => void }) {
  const [records, setRecords] = useState<CalibrationRecord[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      const list = await bridge.listCalibrations()
      setRecords(list)
      setSelected((cur) => cur ?? list.find((r) => r.active)?.id ?? list[0]?.id ?? null)
    } catch (e) {
      setError((e as { message?: string }).message ?? 'failed to load calibrations')
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const current = useMemo(() => records.find((r) => r.id === selected) ?? null, [records, selected])

  const activate = async (id: string) => {
    try {
      await bridge.setActiveCalibration(id)
      onNotice(`Active calibration: ${id}`)
      await refresh()
    } catch (e) {
      onNotice((e as { message?: string }).message ?? 'failed to set active calibration')
    }
  }

  return (
    <div className="calibration">
      <div className="page-title">
        <h1>Calibration</h1>
        <span className="hint">
          Calibrated values remain estimates; calibration never promotes an estimate to a measurement.
        </span>
        <span className="spacer" style={{ flex: 1 }} />
        <button onClick={() => setCreating((v) => !v)}>{creating ? 'Cancel' : 'New calibration'}</button>
        <button onClick={() => void refresh()}>Refresh</button>
      </div>

      {error ? <div className="error-box"><div className="message">{error}</div></div> : null}

      {creating ? (
        <CreateForm
          onCreated={async (rec) => {
            setCreating(false)
            onNotice(`Calibration ${rec.id} created (${rec.quality})`)
            await refresh()
            setSelected(rec.id)
          }}
          onNotice={onNotice}
        />
      ) : null}

      {records.length === 0 && !creating ? (
        <section className="panel">
          <div className="content">
            <EmptyState
              title="Calibration"
              description="Fit an estimated display-power model against independent reference measurements."
              actions={
                <button className="primary" onClick={() => setCreating(true)}>
                  Create display calibration
                </button>
              }
              details={[
                { k: 'What is calibratable', v: 'Display power' },
                { k: 'Reference', v: 'External / bench / OEM / manual' },
                { k: 'Evidence classification', v: 'Calibrated values remain Estimated' },
              ]}
            />
          </div>
        </section>
      ) : (
        <div className="calibration-columns">
          <section className="panel">
            <header>Available calibrations</header>
            <div className="content flush">
              {records.length === 0 ? (
                <div className="empty">No calibrations yet. Create one from external-meter reference values.</div>
              ) : (
                <div className="list">
                  {records.map((r) => (
                    <button
                      key={r.id}
                      className={`row calib-row${r.id === selected ? ' active' : ''}`}
                      onClick={() => setSelected(r.id)}
                    >
                      <span className={`badge ${qualityBadge(r.quality)}`}>{r.quality}</span>
                      <span className="name">{r.id}</span>
                      {r.active ? <span className="badge accepted">active</span> : null}
                      <span className="muted mono-sm">{formatDateTime(r.createdAtMs)}</span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          </section>

          {current ? (
            <CalibrationDetail rec={current} onActivate={() => void activate(current.id)} onNotice={onNotice} />
          ) : (
            <section className="panel">
              <header>Calibration details</header>
              <div className="content muted">Select a calibration to inspect its model, quality and evidence.</div>
            </section>
          )}
        </div>
      )}
    </div>
  )
}

function CalibrationDetail({
  rec,
  onActivate,
  onNotice,
}: {
  rec: CalibrationRecord
  onActivate: () => void
  onNotice: (msg: string | null) => void
}) {
  return (
    <section className="panel">
      <header>
        {rec.id}{' '}
        <span className={`badge ${qualityBadge(rec.quality)}`}>{rec.quality}</span>
        <span className="spacer" />
        {!rec.active ? (
          <button onClick={onActivate} className="primary">
            Set active
          </button>
        ) : (
          <span className="badge accepted">active</span>
        )}
      </header>
      <div className="content">
        <div className="kv">
          <div className="k">Target</div>
          <div className="v">{rec.target}</div>
          <div className="k">Evidence class</div>
          <div className="v">
            <span className="badge derived">{rec.evidenceClass}</span>{' '}
            <span className="muted">a calibrated value is an estimate, not a measurement</span>
          </div>
          <div className="k">Reference</div>
          <div className="v">
            {REFERENCE_KINDS.find((k) => k.value === rec.referenceKind)?.label ?? rec.referenceKind}{' '}
            {rec.independentReference ? null : <span className="badge warn">not independent</span>}
          </div>
          <div className="k">Model</div>
          <div className="v mono-sm">
            {rec.modelKind}: {rec.model.slopeWPerPct.toFixed(4)} W/% above {rec.model.minBrightness.toFixed(1)}%,
            intercept {rec.model.interceptW.toFixed(2)} W, R² {rec.model.r2Fit.toFixed(3)}
          </div>
          <div className="k">Applicability</div>
          <div className="v mono-sm">
            {rec.applicability.machine} · panel {rec.applicability.panel} · {rec.applicability.inputMin.toFixed(1)}–
            {rec.applicability.inputMax.toFixed(1)}% · {rec.applicability.powerSource ?? 'power source unspecified'}
          </div>
          <div className="k">Version</div>
          <div className="v mono-sm">
            revision {rec.revision} · schema v{rec.schemaVersion} · {formatDateTime(rec.createdAtMs)}
          </div>
        </div>

        {rec.qualityReasons.length ? (
          <div className="notice" style={{ marginTop: 8 }}>
            {rec.qualityReasons.join(' · ')}
          </div>
        ) : null}

        <h3>Independent validation</h3>
        {rec.validation ? (
          <table className="data-table">
            <thead>
              <tr>
                <th scope="col">n</th>
                <th scope="col">MAE (W)</th>
                <th scope="col">RMSE (W)</th>
                <th scope="col">Median abs (W)</th>
                <th scope="col">Max abs (W)</th>
                <th scope="col">R²</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>{rec.validation.n}</td>
                <td>{rec.validation.mae.toFixed(3)}</td>
                <td>{rec.validation.rmse.toFixed(3)}</td>
                <td>{rec.validation.medianAbsError.toFixed(3)}</td>
                <td>{rec.validation.maxAbsError.toFixed(3)}</td>
                <td>{rec.validation.r2 === null ? 'n/a' : rec.validation.r2.toFixed(3)}</td>
              </tr>
            </tbody>
          </table>
        ) : (
          <p className="muted">Independent validation unavailable: too few points for a held-out split.</p>
        )}

        <h3>Residuals</h3>
        <table className="data-table">
          <thead>
            <tr>
              <th scope="col">Input</th>
              <th scope="col">Predicted (W)</th>
              <th scope="col">Reference (W)</th>
              <th scope="col">Abs error</th>
              <th scope="col">Rel error</th>
              <th scope="col">Held out</th>
            </tr>
          </thead>
          <tbody>
            {rec.residuals.map((r, i) => (
              <tr key={i}>
                <td>{r.input.toFixed(1)}</td>
                <td>{r.predicted.toFixed(3)}</td>
                <td>{r.reference.toFixed(3)}</td>
                <td>{r.absError.toFixed(3)}</td>
                <td>{r.relativeError === null ? 'n/a' : r.relativeError.toFixed(3)}</td>
                <td>{r.heldOut ? 'yes' : ''}</td>
              </tr>
            ))}
          </tbody>
        </table>

        {rec.exclusions.length ? (
          <>
            <h3>Excluded evidence</h3>
            <table className="data-table">
              <thead>
                <tr>
                  <th scope="col">Session</th>
                  <th scope="col">Reason</th>
                </tr>
              </thead>
              <tbody>
                {rec.exclusions.map((e, i) => (
                  <tr key={i}>
                    <td>{e.sessionId}</td>
                    <td>{e.reason}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        ) : null}

        <div style={{ marginTop: 10 }}>
          <ReportExportBar request={{ kind: 'calibration', id: rec.id }} onNotice={onNotice} />
        </div>
      </div>
    </section>
  )
}

function CreateForm({
  onCreated,
  onNotice,
}: {
  onCreated: (rec: CalibrationRecord) => void
  onNotice: (msg: string | null) => void
}) {
  const [referenceKind, setReferenceKind] = useState<ReferenceKind>('external_meter')
  const [references, setReferences] = useState<CalibrationReference[]>([
    { ...emptyReference(), input: 0, reference: 5 },
    { ...emptyReference(), input: 50, reference: 7.5 },
    { ...emptyReference(), input: 100, reference: 10 },
  ])
  const [notes, setNotes] = useState('')
  const [activate, setActivate] = useState(true)
  const [sessions, setSessions] = useState<SessionListEntry[]>([])
  const [datasetIds, setDatasetIds] = useState<string[]>([])
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    bridge.listSessions().then(setSessions).catch(() => setSessions([]))
  }, [])

  const update = (i: number, patch: Partial<CalibrationReference>) => {
    setReferences((rows) => rows.map((r, idx) => (idx === i ? { ...r, ...patch } : r)))
  }

  const submit = async () => {
    setBusy(true)
    try {
      const dataset: CalibrationDatasetRef[] = datasetIds.map((sessionId) => ({
        sessionId,
        role: 'raw_evidence',
        note: '',
      }))
      const request: CreateCalibrationRequest = {
        target: TARGET,
        referenceKind,
        references: references.map((r) => ({ ...r, kind: referenceKind })),
        dataset,
        exclusions: [],
        notes,
        machine: null,
        panel: null,
        powerSource: null,
        activate,
      }
      const rec = await bridge.createCalibration(request)
      onCreated(rec)
    } catch (e) {
      const err = e as { code?: string; message?: string }
      onNotice(`${err.code ?? 'error'}: ${err.message ?? 'calibration failed'}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="panel">
      <header>Create calibration — display power</header>
      <div className="content">
        <p className="muted">
          Enter independent reference values (external meter, bench meter, OEM reading). Every value is external/user
          evidence, never labelled as measured by power-forensics.
        </p>
        <div className="kv">
          <div className="k">Reference kind</div>
          <div className="v">
            <select value={referenceKind} onChange={(e) => setReferenceKind(e.target.value as ReferenceKind)}>
              {REFERENCE_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        <table className="data-table">
          <thead>
            <tr>
              <th scope="col">Brightness %</th>
              <th scope="col">Reference W</th>
              <th scope="col">Source label</th>
              <th scope="col">Note</th>
              <th scope="col" />
            </tr>
          </thead>
          <tbody>
            {references.map((r, i) => (
              <tr key={i}>
                <td>
                  <input
                    type="number"
                    min={0}
                    max={100}
                    value={r.input}
                    onChange={(e) => update(i, { input: Number(e.target.value) })}
                    aria-label={`reference ${i + 1} brightness`}
                  />
                </td>
                <td>
                  <input
                    type="number"
                    value={r.reference}
                    onChange={(e) => update(i, { reference: Number(e.target.value) })}
                    aria-label={`reference ${i + 1} watts`}
                  />
                </td>
                <td>
                  <input value={r.sourceLabel} onChange={(e) => update(i, { sourceLabel: e.target.value })} />
                </td>
                <td>
                  <input value={r.note} onChange={(e) => update(i, { note: e.target.value })} />
                </td>
                <td>
                  <button onClick={() => setReferences((rows) => rows.filter((_, idx) => idx !== i))} disabled={references.length <= 2}>
                    Remove
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <button onClick={() => setReferences((rows) => [...rows, emptyReference()])}>Add point</button>

        <h3>Dataset references (optional)</h3>
        <p className="muted">Raw sessions are referenced, not duplicated. The raw evidence stays authoritative.</p>
        <div className="calibration-datasets">
          {sessions.slice(0, 40).map((s) => (
            <label key={s.path} className="dataset-item">
              <input
                type="checkbox"
                checked={datasetIds.includes(s.file)}
                onChange={() =>
                  setDatasetIds((ids) => (ids.includes(s.file) ? ids.filter((x) => x !== s.file) : [...ids, s.file]))
                }
              />
              <span className="mono-sm">{s.file}</span>
              <span className="muted">{s.recovered ? 'recovered' : s.status}</span>
            </label>
          ))}
        </div>

        <div className="kv" style={{ marginTop: 8 }}>
          <div className="k">Notes</div>
          <div className="v">
            <input value={notes} onChange={(e) => setNotes(e.target.value)} style={{ width: '100%' }} />
          </div>
          <div className="k">Activate</div>
          <div className="v">
            <label>
              <input type="checkbox" checked={activate} onChange={(e) => setActivate(e.target.checked)} /> make this the
              active calibration
            </label>
          </div>
        </div>

        <div style={{ marginTop: 10 }}>
          <button className="primary" onClick={submit} disabled={busy || references.length < 2}>
            {busy ? 'Fitting…' : 'Fit and save calibration'}
          </button>
        </div>
      </div>
    </section>
  )
}
