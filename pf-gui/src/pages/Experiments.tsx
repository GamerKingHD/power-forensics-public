import { useCallback, useEffect, useMemo, useState } from 'react'
import { BridgeFailure, bridge } from '../api/bridge'
import type {
  CreateExperimentRequest,
  ExperimentAnalysis,
  ExperimentRecord,
  ExperimentSummary,
  RunGroup,
  SessionListEntry,
  TreatmentDirection,
} from '../api/types'
import { usePolling } from '../hooks/usePolling'
import { EmptyState } from '../components/layout/EmptyState'
import { ReportExportBar } from '../components/report/ReportExport'
import {
  EXPERIMENT_METRICS,
  directionLabel,
  formatSeconds,
  groupLabel,
  metricByKey,
  planRuns,
  statusClass,
  validityClass,
  validityLabel,
} from '../lib/experiments'
import { formatDateTime } from '../lib/format'
import {
  ConfounderPanel,
  ExperimentCaveats,
  GroupSummaryPanel,
  PrimaryResultPanel,
  RunDotPlot,
  RunTablePanel,
  SecondaryPanel,
  ValidityStrip,
} from '../components/experiments/ExperimentPanels'

export interface CompareTarget {
  path: string
  fromMs?: number
  toMs?: number
}

interface Props {
  onAnalyze: (path: string, fromMs?: number, toMs?: number) => void
  onCompare: (a: CompareTarget, b: CompareTarget) => void
  onNotice: (msg: string | null) => void
  defaultRedact?: boolean
}

function toFailure(e: unknown): BridgeFailure {
  return e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e))
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

function NewExperimentForm({ onCreated }: { onCreated: (record: ExperimentRecord) => void }) {
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [question, setQuestion] = useState('')
  const [metric, setMetric] = useState(EXPERIMENT_METRICS[0].key)
  const [baselineLabel, setBaselineLabel] = useState('Baseline')
  const [treatmentLabel, setTreatmentLabel] = useState('Treatment')
  const [reps, setReps] = useState(3)
  const [settle, setSettle] = useState(30)
  const [measure, setMeasure] = useState(120)
  const [paired, setPaired] = useState(false)
  const [randomized, setRandomized] = useState(false)
  const [notes, setNotes] = useState('')

  const create = async () => {
    setBusy(true)
    setError(null)
    try {
      const m = metricByKey(metric)
      const req: CreateExperimentRequest = {
        name: name.trim() || 'Untitled experiment',
        question,
        primaryMetric: metric,
        primaryLabel: m?.label ?? metric,
        primaryUnit: m?.unit ?? '',
        direction: m?.defaultDirection ?? 'lower',
        pairing: paired ? 'paired' : 'unpaired',
        baselineLabel,
        treatmentLabel,
        settleS: settle,
        measureS: measure,
        repetitions: reps,
        randomized,
        collectors: null,
        preset: null,
        notes,
      }
      const record = await bridge.createExperiment(req)
      onCreated(record)
      setOpen(false)
    } catch (e) {
      setError(toFailure(e).message)
    } finally {
      setBusy(false)
    }
  }

  if (!open) {
    return (
      <div className="toolbar">
        <button className="primary" onClick={() => setOpen(true)}>
          New experiment
        </button>
        <span className="muted mono-sm">
          Define a question, then add existing sessions or run a guided sequence.
        </span>
      </div>
    )
  }

  return (
    <section className="panel">
      <header>New experiment</header>
      <div className="content experiment-form">
        <label className="compare-field">
          <span className="k">Name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="60 Hz vs 120 Hz" />
        </label>
        <label className="compare-field">
          <span className="k">Question</span>
          <input
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            placeholder="Does reducing refresh rate lower discharge power?"
          />
        </label>
        <label className="compare-field">
          <span className="k">Primary metric</span>
          <select value={metric} onChange={(e) => setMetric(e.target.value)}>
            {EXPERIMENT_METRICS.map((m) => (
              <option key={m.key} value={m.key}>
                {m.label} ({m.unit})
              </option>
            ))}
          </select>
        </label>
        <label className="compare-field">
          <span className="k">Baseline label</span>
          <input value={baselineLabel} onChange={(e) => setBaselineLabel(e.target.value)} />
        </label>
        <label className="compare-field">
          <span className="k">Treatment label</span>
          <input value={treatmentLabel} onChange={(e) => setTreatmentLabel(e.target.value)} />
        </label>
        <div className="compare-facts">
          <label className="muted check">
            <input type="number" min={1} max={20} value={reps} onChange={(e) => setReps(Number(e.target.value))} style={{ width: 56 }} /> repetitions per group
          </label>
          <label className="muted check">
            settle <input type="number" min={0} value={settle} onChange={(e) => setSettle(Number(e.target.value))} style={{ width: 64 }} /> s
          </label>
          <label className="muted check">
            measure <input type="number" min={1} value={measure} onChange={(e) => setMeasure(Number(e.target.value))} style={{ width: 64 }} /> s
          </label>
          <label className="muted check">
            <input type="checkbox" checked={paired} onChange={(e) => setPaired(e.target.checked)} /> paired
          </label>
          <label className="muted check">
            <input type="checkbox" checked={randomized} onChange={(e) => setRandomized(e.target.checked)} /> randomized order
          </label>
        </div>
        <label className="compare-field">
          <span className="k">Notes</span>
          <input value={notes} onChange={(e) => setNotes(e.target.value)} />
        </label>
        {error ? <div className="error-box">{error}</div> : null}
        <div className="toolbar">
          <button className="primary" disabled={busy} onClick={() => void create()}>
            {busy ? 'Creating…' : 'Create'}
          </button>
          <button onClick={() => setOpen(false)}>Cancel</button>
        </div>
      </div>
    </section>
  )
}

function Library({
  items,
  onOpen,
  onCreated,
  onDelete,
}: {
  items: ExperimentSummary[]
  onOpen: (id: string) => void
  onCreated: (r: ExperimentRecord) => void
  onDelete: (id: string) => void
}) {
  return (
    <>
      <div className="page-title">
        <h1>Experiments</h1>
        <span className="hint">A measurement lab notebook — evidence, not a scoreboard.</span>
      </div>
      <NewExperimentForm onCreated={onCreated} />
      {items.length === 0 ? (
        <section className="panel">
          <div className="content">
            <EmptyState
              title="No experiments yet"
              description="Measure whether a change is larger than normal run-to-run variation. Define a question, then add existing sessions or run a guided sequence."
              details={[
                { k: 'Baseline runs', v: 'the condition to compare against' },
                { k: 'Treatment runs', v: 'the changed condition' },
                { k: 'Repeated trials', v: 'separates effect from run-to-run noise' },
                { k: 'Confounder checks', v: 'flags conditions that differ across runs' },
              ]}
            />
          </div>
        </section>
      ) : (
      <section className="panel">
        <header>Experiment library</header>
        <div className="content flush">
          <div className="exp-library">
            <div className="exp-row exp-head">
              <span>Name</span>
              <span>Primary metric</span>
              <span>Groups</span>
              <span>Runs</span>
              <span>Status</span>
              <span>Last result</span>
              <span>Updated</span>
              <span>Actions</span>
            </div>
            {items.map((e) => (
              <div className="exp-row" key={e.id}>
                <span>
                  <button className="ghost" onClick={() => onOpen(e.id)}>
                    {e.name}
                  </button>
                  <span className="muted mono-sm"> {e.question}</span>
                </span>
                <span className="mono-sm">{e.primaryLabel || e.primaryMetric}</span>
                <span className="mono-sm">
                  {e.baselineLabel} / {e.treatmentLabel}
                </span>
                <span className="mono-sm">
                  {e.baselineRuns}+{e.treatmentRuns}
                  {e.missingRuns ? <span className="warn-text"> · {e.missingRuns} missing</span> : null}
                </span>
                <span>
                  <span className={`badge ${statusClass(e.status)}`}>{e.status}</span>
                </span>
                <span className="mono-sm">
                  {e.lastResult ? (
                    <>
                      <span className={`badge ${validityClass(e.lastResult.validity)}`}>
                        {validityLabel(e.lastResult.validity)}
                      </span>{' '}
                      {e.lastResult.classification.replace(/_/g, ' ')}
                    </>
                  ) : (
                    <span className="muted">not computed</span>
                  )}
                </span>
                <span className="mono-sm muted">{formatDateTime(e.updatedMs)}</span>
                <span>
                  <button onClick={() => onOpen(e.id)}>Open</button>{' '}
                  <button onClick={() => onDelete(e.id)}>Delete</button>
                </span>
              </div>
            ))}
          </div>
        </div>
      </section>
      )}
    </>
  )
}

// ---------------------------------------------------------------------------
// Run building
// ---------------------------------------------------------------------------

function nextRunId(runs: ExperimentRecord['runs'], group: RunGroup): string {
  const prefix = group === 'baseline' ? 'A' : 'B'
  const n = runs.filter((r) => r.group === group).length + 1
  return `${prefix}${n}`
}

function nextPairId(runs: ExperimentRecord['runs']): number {
  return runs.reduce((m, r) => Math.max(m, r.pairId ?? 0), 0) + 1
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

function ExperimentDetail({
  id,
  onBack,
  onAnalyze,
  onCompare,
  onNotice,
  reload,
  defaultRedact,
}: {
  id: string
  onBack: () => void
  onAnalyze: Props['onAnalyze']
  onCompare: Props['onCompare']
  onNotice: Props['onNotice']
  reload: number
  defaultRedact: boolean
}) {
  const sessions = usePolling(bridge.listSessions, 5000)
  const [draft, setDraft] = useState<ExperimentRecord | null>(null)
  const [analysis, setAnalysis] = useState<ExperimentAnalysis | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<BridgeFailure | null>(null)
  const [selectedSession, setSelectedSession] = useState('')

  const load = useCallback(async () => {
    setError(null)
    try {
      const record = await bridge.getExperiment(id)
      setDraft(record)
      const a = await bridge.analyzeExperiment(id)
      setAnalysis(a)
    } catch (e) {
      setError(toFailure(e))
    }
  }, [id])

  useEffect(() => {
    void load()
  }, [load, reload])

  const persist = useCallback(
    async (next: ExperimentRecord) => {
      setBusy(true)
      try {
        const saved = await bridge.saveExperiment(next)
        setDraft(saved)
        const a = await bridge.analyzeExperiment(saved.id)
        setAnalysis(a)
      } catch (e) {
        onNotice(`Save failed: ${toFailure(e).message}`)
      } finally {
        setBusy(false)
      }
    },
    [onNotice],
  )

  const addRun = useCallback(
    (group: RunGroup) => {
      if (!draft || !selectedSession) return
      const run = {
        id: nextRunId(draft.runs, group),
        label: nextRunId(draft.runs, group),
        group,
        order: draft.runs.length,
        pairId: draft.pairing === 'paired' ? nextPairId(draft.runs) : null,
        sessionPath: selectedSession,
        fromMs: null,
        toMs: null,
        included: true,
        source: 'existing' as const,
        notes: '',
        capturedMs: null,
      }
      void persist({ ...draft, runs: [...draft.runs, run] })
    },
    [draft, selectedSession, persist],
  )

  const toggleIncluded = useCallback(
    (runId: string, included: boolean) => {
      if (!draft) return
      void persist({
        ...draft,
        runs: draft.runs.map((r) => (r.id === runId ? { ...r, included } : r)),
      })
    },
    [draft, persist],
  )

  const reorder = useCallback(
    (randomized: boolean) => {
      if (!draft) return
      const runs = [...draft.runs].sort((a, b) => a.order - b.order)
      const seed = draft.orderSeed ?? Date.now()
      if (randomized) {
        let s = seed >>> 0
        const rand = () => {
          s = (Math.imul(s, 1664525) + 1013904223) >>> 0
          return s
        }
        for (let i = runs.length - 1; i > 0; i -= 1) {
          const j = rand() % (i + 1)
          const t = runs[i]
          runs[i] = runs[j]
          runs[j] = t
        }
      }
      const reordered = runs.map((r, i) => ({ ...r, order: i }))
      void persist({ ...draft, runs: reordered, randomized, orderSeed: seed })
    },
    [draft, persist],
  )

  const deleteRun = useCallback(
    (runId: string) => {
      if (!draft) return
      void persist({ ...draft, runs: draft.runs.filter((r) => r.id !== runId) })
    },
    [draft, persist],
  )

  const startGuided = useCallback(() => {
    if (!draft) return
    const seed = draft.orderSeed ?? Date.now()
    const plan = planRuns(draft.repetitions, draft.randomized, seed, draft.pairing === 'paired')
    let a = 0
    let b = 0
    const runs = plan.map((p, i) => {
      const id = p.group === 'baseline' ? `A${(a += 1)}` : `B${(b += 1)}`
      return {
        id,
        label: id,
        group: p.group,
        order: i,
        pairId: p.pairId,
        sessionPath: '',
        fromMs: null,
        toMs: null,
        included: true,
        source: 'guided' as const,
        notes: '',
        capturedMs: null,
      }
    })
    void persist({
      ...draft,
      runs,
      randomized: draft.randomized,
      orderSeed: seed,
      status: 'running',
      guided: { phase: 'prepare', runIndex: 0, runStartedMs: null, treatmentConfirmed: false, confirmations: [] },
    })
  }, [draft, persist])

  const pairedAvailable = useMemo(
    () => (analysis?.paired?.usablePairs ?? 0) >= 1,
    [analysis],
  )

  const pendingIds = useMemo(() => {
    const s = new Set<string>()
    if (draft?.status === 'running') {
      for (const r of draft.runs) if (!r.sessionPath) s.add(r.id)
    }
    return s
  }, [draft])

  if (error) {
    return (
      <div className="error-box">
        <div className="code">{error.code}</div>
        <div>{error.message}</div>
        <button onClick={onBack} style={{ marginTop: 6 }}>
          Back to library
        </button>
      </div>
    )
  }
  if (!draft) return <div className="empty">Loading experiment…</div>

  return (
    <>
      <div className="page-title">
        <button onClick={onBack}>← Experiments</button>
        <h1>{draft.name}</h1>
        <span className={`badge ${statusClass(draft.status)}`}>{draft.status}</span>
        <span className="hint mono-sm">{draft.question}</span>
        <span className="spacer" style={{ flex: 1 }} />
        <button disabled={busy} onClick={() => void persist(draft)}>
          Save
        </button>
      </div>

      <div className="experiment-design">
        <span>
          <strong>Primary</strong> {draft.primaryLabel} ({draft.primaryUnit})
        </span>
        <span>
          <strong>Baseline</strong> {draft.baselineLabel}
        </span>
        <span>
          <strong>Treatment</strong> {draft.treatmentLabel}
        </span>
        <span>
          <strong>Direction</strong> {directionLabel(draft.direction as TreatmentDirection)}
        </span>
        <span>
          <strong>Design</strong> {draft.pairing}
          {draft.randomized ? ' · randomized' : ' · interleaved'}
        </span>
        <span>
          <strong>Runs</strong> {draft.repetitions} + {draft.repetitions}
        </span>
        <span>
          <strong>Settle / measure</strong> {draft.settleS}s / {draft.measureS}s
        </span>
      </div>

      <ReportExportBar
        request={{ kind: 'experiment', id: draft.id }}
        defaultRedact={defaultRedact}
        onNotice={onNotice}
      />

      {analysis ? <ValidityStrip analysis={analysis} /> : <div className="empty">Computing experiment analysis…</div>}

      {draft.status !== 'running' ? (
        <section className="panel">
          <header>
            Runs <span className="hint">existing sessions; the session stays authoritative</span>
          </header>
          <div className="content">
            <div className="toolbar">
              <select value={selectedSession} onChange={(e) => setSelectedSession(e.target.value)} style={{ minWidth: 260 }}>
                <option value="">Choose a recorded session…</option>
                {(sessions.data ?? []).map((s: SessionListEntry) => (
                  <option key={s.path} value={s.path}>
                    {s.note || s.file} · {formatDateTime(s.startWallMs)}
                  </option>
                ))}
              </select>
              <button disabled={!selectedSession || busy} onClick={() => addRun('baseline')}>
                Add as baseline
              </button>
              <button disabled={!selectedSession || busy} onClick={() => addRun('treatment')}>
                Add as treatment
              </button>
              <span className="spacer" />
              <button disabled={!draft.runs.length || busy} onClick={() => reorder(false)}>
                Interleave order
              </button>
              <button disabled={!draft.runs.length || busy} onClick={() => reorder(true)}>
                Randomize order
              </button>
              <button className="primary" disabled={busy} onClick={startGuided}>
                Start guided experiment
              </button>
            </div>
            <div className="muted mono-sm">
              Interleaving (A B A B) reduces the chance that battery temperature, background activity or time
              drift affects one group more than the other.
            </div>
          </div>
        </section>
      ) : null}

      {draft.status === 'running' && draft.guided ? (
        <GuidedRunner
          draft={draft}
          onUpdate={(next) => void persist(next)}
          onNotice={onNotice}
        />
      ) : null}

      {analysis ? (
        <>
          <PrimaryResultPanel analysis={analysis} />
          <RunDotPlot analysis={analysis} />
          <GroupSummaryPanel analysis={analysis} />
          <RunTablePanel
            analysis={analysis}
            pendingIds={pendingIds}
            pairedAvailable={pairedAvailable}
            onToggleIncluded={toggleIncluded}
            onAnalyze={(r) => {
              if (!r.available) return
              const run = draft.runs.find((x) => x.id === r.id)
              if (run?.sessionPath) onAnalyze(run.sessionPath, run.fromMs ?? undefined, run.toMs ?? undefined)
            }}
            onComparePair={(r) => {
              const pair = draft.runs.find((x) => x.pairId === r.pairId && x.group !== r.group)
              const thisRun = draft.runs.find((x) => x.id === r.id)
              if (pair?.sessionPath && thisRun?.sessionPath) {
                onCompare(
                  { path: thisRun.sessionPath, fromMs: thisRun.fromMs ?? undefined, toMs: thisRun.toMs ?? undefined },
                  { path: pair.sessionPath, fromMs: pair.fromMs ?? undefined, toMs: pair.toMs ?? undefined },
                )
              }
            }}
          />
          <ConfounderPanel analysis={analysis} />
          <SecondaryPanel secondary={analysis.secondary} />
          <ExperimentCaveats analysis={analysis} />
          <div className="muted mono-sm" style={{ padding: '4px 0 12px' }}>
            Runs are never deleted here. Excluding a run only changes the experiment metadata and immediately
            recomputes the analysis.
            {draft.runs.length ? (
              <button style={{ marginLeft: 8 }} onClick={() => void load()}>
                Recompute
              </button>
            ) : null}
          </div>
        </>
      ) : null}

      <section className="panel">
        <header>Experiment metadata</header>
        <div className="content">
          <div className="toolbar">
            <label className="muted check">
              <input
                type="checkbox"
                checked={draft.randomized}
                onChange={(e) => void persist({ ...draft, randomized: e.target.checked, orderSeed: draft.orderSeed ?? Date.now() })}
              />{' '}
              randomized order
            </label>
            <label className="muted check">
              <input
                type="checkbox"
                checked={draft.pairing === 'paired'}
                onChange={(e) => void persist({ ...draft, pairing: e.target.checked ? 'paired' : 'unpaired' })}
              />{' '}
              paired design
            </label>
            <span className="muted mono-sm">created {formatDateTime(draft.createdMs)}</span>
            {draft.runs.length ? (
              <span>
                {draft.runs
                  .slice()
                  .sort((a, b) => a.order - b.order)
                  .map((r) => (
                    <button key={r.id} className="ghost mono-sm" title="Remove run reference" onClick={() => deleteRun(r.id)}>
                      {r.id}×
                    </button>
                  ))}
              </span>
            ) : null}
          </div>
        </div>
      </section>
    </>
  )
}

// ---------------------------------------------------------------------------
// Guided runner
// ---------------------------------------------------------------------------

function GuidedRunner({
  draft,
  onUpdate,
  onNotice,
}: {
  draft: ExperimentRecord
  onUpdate: (next: ExperimentRecord) => void
  onNotice: (msg: string | null) => void
}) {
  const guided = draft.guided
  const [now, setNow] = useState(() => Date.now())
  const [validation, setValidation] = useState<{ status: string; findings: string[] } | null>(null)
  const [marker, setMarker] = useState('')

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(t)
  }, [])

  if (!guided) return null
  const run = draft.runs.find((_, i) => i === guided.runIndex) ?? null
  const started = guided.runStartedMs ?? now
  const elapsed = Math.max(0, (now - started) / 1000)
  const phase = guided.phase

  const patch = (p: Record<string, unknown>) => onUpdate({ ...draft, guided: { ...guided, ...p } })

  const beginRun = async () => {
    try {
      await bridge.start({ preset: draft.preset ?? undefined, collectors: draft.collectors ?? undefined, note: `${draft.name} ${run?.id ?? ''}` })
      setValidation(null)
      patch({ phase: 'settle', runStartedMs: Date.now(), treatmentConfirmed: run?.group === 'baseline' })
    } catch (e) {
      onNotice(`Could not start recording: ${toFailure(e).message}`)
    }
  }

  const finishRun = async () => {
    try {
      await bridge.stop()
      const status = await bridge.agentStatus()
      const path = status.sessionPath ?? ''
      if (!path) {
        onNotice('Recording stopped but no session path was published.')
        patch({ phase: 'review' })
        return
      }
      const v = await bridge.validateExperimentRun({
        sessionPath: path,
        primaryMetric: draft.primaryMetric,
        primaryLabel: draft.primaryLabel,
        primaryUnit: draft.primaryUnit,
      })
      setValidation({ status: v.status, findings: v.findings })
      const runs = draft.runs.map((r, i) =>
        i === guided.runIndex
          ? { ...r, sessionPath: path, capturedMs: Date.now(), included: v.status !== 'invalid' }
          : r,
      )
      onUpdate({ ...draft, runs, guided: { ...guided, phase: 'review' } })
    } catch (e) {
      onNotice(`Run finalization failed: ${toFailure(e).message}`)
      patch({ phase: 'review' })
    }
  }

  // Phase timers: settle -> measuring -> stop.
  useEffect(() => {
    if (phase === 'settle' && elapsed >= draft.settleS) {
      patch({ phase: 'measuring' })
    } else if (phase === 'measuring' && elapsed >= draft.settleS + draft.measureS) {
      void finishRun()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phase, elapsed, draft.settleS, draft.measureS])

  const acceptContinue = () => {
    const nextIndex = guided.runIndex + 1
    const done = nextIndex >= draft.runs.length
    onUpdate({
      ...draft,
      status: done ? 'complete' : 'running',
      guided: { ...guided, phase: done ? 'done' : 'prepare', runIndex: nextIndex, runStartedMs: null, treatmentConfirmed: false },
    })
  }

  const repeatRun = () => {
    const runs = draft.runs.map((r, i) =>
      i === guided.runIndex ? { ...r, sessionPath: '', capturedMs: null } : r,
    )
    onUpdate({ ...draft, runs, guided: { ...guided, phase: 'prepare', runStartedMs: null } })
  }

  const abortExperiment = async () => {
    try {
      await bridge.stop()
    } catch {
      // no active agent is fine
    }
    onUpdate({ ...draft, status: 'aborted', guided: { ...guided, phase: 'done' } })
  }

  const remaining =
    phase === 'settle'
      ? draft.settleS - elapsed
      : phase === 'measuring'
        ? draft.settleS + draft.measureS - elapsed
        : 0

  return (
    <section className="panel guided-runner">
      <header>
        Guided run <span className="hint">the user performs the treatment manually</span>
      </header>
      <div className="content">
        <div className="guided-step">
          <strong>
            Run {Math.min(guided.runIndex + 1, draft.runs.length)} of {draft.runs.length}
          </strong>
          {run ? (
            <span>
              {' '}
              {groupLabel(run.group)}: {run.group === 'baseline' ? draft.baselineLabel : draft.treatmentLabel} ({run.id})
            </span>
          ) : null}
          <span className={`badge ${phase === 'measuring' ? 'recording' : 'derived'}`}> {phase.toUpperCase()}</span>
          {phase === 'settle' || phase === 'measuring' ? (
            <span className="mono-sm"> {formatSeconds(Math.max(0, remaining))} remaining</span>
          ) : null}
        </div>

        {phase === 'prepare' ? (
          <div className="guided-actions">
            <p className="muted">
              {run?.group === 'treatment'
                ? `Set the treatment condition (${draft.treatmentLabel}), then continue. The application will not change system settings for you.`
                : `Prepare the baseline condition (${draft.baselineLabel}), then continue.`}
            </p>
            <button className="primary" onClick={() => void beginRun()}>
              {run?.group === 'treatment' ? 'Confirm & start' : 'Start run'}
            </button>
          </div>
        ) : null}

        {phase === 'settle' || phase === 'measuring' ? (
          <div className="guided-actions">
            <div className="toolbar">
              <input
                placeholder="marker note"
                value={marker}
                onChange={(e) => setMarker(e.target.value)}
                style={{ width: 200 }}
              />
              <button
                onClick={() => {
                  void bridge.addMarker(marker || 'experiment marker')
                  setMarker('')
                }}
              >
                Add marker
              </button>
              <button onClick={() => void abortExperiment()}>Abort run / stop experiment</button>
            </div>
            <p className="muted mono-sm">
              Settle evidence is retained in the session; only the labelled measurement range is analysed.
            </p>
          </div>
        ) : null}

        {phase === 'review' ? (
          <div className="guided-actions">
            {validation ? (
              <div className={`quality-warning`}>
                Run quality: <strong>{validation.status.replace('_', ' ')}</strong>
                {validation.findings.length ? ` — ${validation.findings.join('; ')}` : ''}
              </div>
            ) : (
              <div className="muted">Validating run…</div>
            )}
            <div className="toolbar">
              <button className="primary" onClick={acceptContinue}>
                Accept & continue
              </button>
              <button onClick={repeatRun}>Repeat run</button>
              <button onClick={() => void abortExperiment()}>Stop experiment</button>
            </div>
          </div>
        ) : null}

        {phase === 'done' ? <div className="muted">Guided sequence complete.</div> : null}
      </div>
    </section>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function ExperimentsPage({ onAnalyze, onCompare, onNotice, defaultRedact = false }: Props) {
  const library = usePolling(bridge.listExperiments, 5000)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)

  const remove = useCallback(
    async (id: string) => {
      try {
        await bridge.deleteExperiment(id)
        library.refresh()
        onNotice('Experiment deleted.')
      } catch (e) {
        onNotice(`Delete failed: ${toFailure(e).message}`)
      }
    },
    [library, onNotice],
  )

  if (selectedId) {
    return (
      <ExperimentDetail
        id={selectedId}
        onBack={() => {
          setSelectedId(null)
          library.refresh()
        }}
        onAnalyze={onAnalyze}
        onCompare={onCompare}
        onNotice={onNotice}
        reload={reloadKey}
        defaultRedact={defaultRedact}
      />
    )
  }

  return (
    <>
      {library.error ? (
        <div className="error-box">
          <div className="code">{library.error.code}</div>
          <div>{library.error.message}</div>
        </div>
      ) : null}
      <Library
        items={library.data ?? []}
        onOpen={setSelectedId}
        onCreated={(r) => {
          library.refresh()
          setReloadKey((k) => k + 1)
          setSelectedId(r.id)
        }}
        onDelete={(id) => void remove(id)}
      />
    </>
  )
}
