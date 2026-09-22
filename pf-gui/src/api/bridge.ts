// Typed wrappers for the narrow Rust bridge. The frontend never parses
// forensic JSON itself; every value and its provenance arrive pre-shaped.

import { invoke } from '@tauri-apps/api/core'
import type {
  AgentStatus,
  AnalyzeOverview,
  AppSettings,
  AppStatus,
  BuiltReport,
  CalibrationActiveEvent,
  CalibrationRecord,
  CapabilityReport,
  BridgeError,
  ComparisonAnalysis,
  CreateCalibrationRequest,
  CreateExperimentRequest,
  Diagnostics,
  ExperimentAnalysis,
  ExperimentRecord,
  ExperimentSummary,
  LiveEvidence,
  RangeAnalysis,
  ReportRequest,
  RunValidation,
  SavedReport,
  SessionListEntry,
  SessionSummary,
  SessionWindow,
} from './types'
export class BridgeFailure extends Error {
  code: string
  detail?: string

  constructor(code: string, message: string, detail?: string) {
    super(message)
    this.name = 'BridgeFailure'
    this.code = code
    this.detail = detail
  }
}

export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

function normalizeError(e: unknown): BridgeFailure {
  if (e instanceof BridgeFailure) return e
  if (typeof e === 'object' && e !== null && 'code' in e && 'message' in e) {
    const err = e as BridgeError
    return new BridgeFailure(err.code, err.message, err.detail ?? undefined)
  }
  return new BridgeFailure('unknown', typeof e === 'string' ? e : 'unexpected bridge failure')
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) {
    throw new BridgeFailure('bridge-unavailable', 'Not running inside the desktop shell.')
  }
  try {
    return (await invoke<T>(command, args)) as T
  } catch (e) {
    throw normalizeError(e)
  }
}

export const bridge = {
  appStatus: () => call<AppStatus>('get_app_status'),
  agentStatus: () => call<AgentStatus>('get_agent_status'),
  liveSnapshot: () => call<LiveEvidence>('get_live_snapshot'),
  liveEvidence: () => call<LiveEvidence>('get_live_snapshot'),
  capabilities: () => call<CapabilityReport>('get_capabilities'),
  listSessions: () => call<SessionListEntry[]>('list_sessions'),
  rebuildSessionIndex: () => call<SessionListEntry[]>('rebuild_session_index'),
  openSession: (path: string) => call<SessionSummary>('open_session_summary', { path }),
  sessionWindow: (
    path: string,
    opts?: { fromMs?: number; toMs?: number; maxPoints?: number; pid?: number },
  ) =>
    call<SessionWindow>('get_session_window', {
      path,
      fromMs: opts?.fromMs,
      toMs: opts?.toMs,
      maxPoints: opts?.maxPoints,
      pid: opts?.pid,
    }),
  analyzeOverview: (path: string) => call<AnalyzeOverview>('analyze_session_overview', { path }),
  analyzeRange: (path: string, fromMs: number, toMs: number) =>
    call<RangeAnalysis>('analyze_session_range', { path, fromMs, toMs }),
  compareSessions: (
    a: { path: string; fromMs?: number | null; toMs?: number | null },
    b: { path: string; fromMs?: number | null; toMs?: number | null },
  ) =>
    call<ComparisonAnalysis>('compare_sessions', {
      pathA: a.path,
      fromMsA: a.fromMs ?? undefined,
      toMsA: a.toMs ?? undefined,
      pathB: b.path,
      fromMsB: b.fromMs ?? undefined,
      toMsB: b.toMs ?? undefined,
    }),
  listExperiments: () => call<ExperimentSummary[]>('list_experiments'),
  getExperiment: (id: string) => call<ExperimentRecord>('get_experiment', { id }),
  createExperiment: (request: CreateExperimentRequest) =>
    call<ExperimentRecord>('create_experiment', { request }),
  saveExperiment: (record: ExperimentRecord) => call<ExperimentRecord>('save_experiment', { record }),
  deleteExperiment: (id: string) => call<void>('delete_experiment', { id }),
  analyzeExperiment: (id: string) => call<ExperimentAnalysis>('analyze_experiment', { id }),
  validateExperimentRun: (args: {
    sessionPath: string
    fromMs?: number | null
    toMs?: number | null
    primaryMetric: string
    primaryLabel: string
    primaryUnit: string
  }) =>
    call<RunValidation>('validate_experiment_run', {
      sessionPath: args.sessionPath,
      fromMs: args.fromMs ?? undefined,
      toMs: args.toMs ?? undefined,
      primaryMetric: args.primaryMetric,
      primaryLabel: args.primaryLabel,
      primaryUnit: args.primaryUnit,
    }),
  recoverSession: (path: string) => call<string>('recover_session', { path }),
  exportSession: (path: string) => call<string>('export_session', { path }),
  listCalibrations: () => call<CalibrationRecord[]>('list_calibrations'),
  getCalibration: (id: string) => call<CalibrationRecord>('get_calibration', { id }),
  createCalibration: (request: CreateCalibrationRequest) =>
    call<CalibrationRecord>('create_calibration', { request }),
  setActiveCalibration: (id: string) => call<CalibrationRecord>('set_active_calibration', { id }),
  calibrationHistory: (target: string) =>
    call<CalibrationActiveEvent[]>('calibration_history', { target }),
  generateReport: (request: ReportRequest) => call<BuiltReport>('generate_report', { request }),
  saveReport: (request: ReportRequest) => call<SavedReport>('save_report', { request }),
  exportSessionTidy: (path: string, redact: boolean, force: boolean) =>
    call<string>('export_session_tidy', { path, redact, force }),
  exportAnalysisJson: (request: ReportRequest) =>
    call<string>('export_analysis_json', { request }),
  getSettings: () => call<AppSettings>('get_settings'),
  saveSettings: (settings: AppSettings) => call<AppSettings>('save_app_settings', { settings }),
  diagnostics: () => call<Diagnostics>('get_diagnostics'),
  openLogDir: () => call<void>('open_log_dir'),
  start: (options: {
    intervalMs?: number
    collectors?: string
    preset?: string
    note?: string
  }) => call<AgentStatus>('start_monitoring', { options }),
  pause: () => call<AgentStatus>('pause_monitoring'),
  resume: () => call<AgentStatus>('resume_monitoring'),
  stop: () => call<AgentStatus>('stop_monitoring'),
  addMarker: (text: string) => call<AgentStatus>('add_marker', { text }),
}

export type Bridge = typeof bridge
