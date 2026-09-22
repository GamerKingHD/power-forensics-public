// Frontend models mirrored from the narrow Rust GUI API (`pf-gui::dto`).
//
// These are the only shapes the UI consumes. Forensic values are always
// `EvidenceValue`; the UI must never collapse one to `number | null`.

export type Provenance = 'measured' | 'derived' | 'estimated' | 'unavailable'
export type Quality = 'fresh' | 'repeated' | 'stale' | 'error' | 'unknown'
export type AbsenceKind = 'unsupported' | 'transient' | 'not-sampled' | 'stale'

export interface EvidenceValue {
  value: number | null
  provenance: Provenance
  quality: Quality
  absenceKind?: AbsenceKind | null
  reason?: string | null
  source: string
  collector?: string
  unit: string
  wallMs: number
  monoMs: number
  /**
   * Age of this reading relative to the enclosing view's reference time
   * (`generatedAt` for live, session end for a recording). Computed in Rust;
   * `undefined`/`null` means age is unknown, never zero.
   */
  ageMs?: number | null
}

export interface BridgeError {
  code: string
  message: string
  detail?: string | null
}

export interface AppStatus {
  guiVersion: string
  toolVersion: string
  sessionsDir: string
  elevated: boolean
  agentPipe: string
  agentBinary: string | null
  platform: string
}

export interface AgentStatus {
  available: boolean
  running: boolean
  paused: boolean
  label: string
  uptimeMs: number
  startedWallMs?: number
  /** Authoritative active session path published by the agent, or null. */
  sessionPath?: string | null
  markersAccepted: number
  markersDropped: number
  reason?: string | null
}

export type CollectorHealthState =
  | 'available'
  | 'degraded'
  | 'unavailable'
  | 'stale'
  | 'timeout'
  | 'recording'

export interface CollectorState {
  name: string
  state: CollectorHealthState
  reason?: string | null
  lastSampleWallMs?: number | null
  samples: number
  failures: number
  observedHz?: number | null
  requiresAdmin: boolean
  elevationWouldHelp: boolean
}

export interface MetricEvidence {
  key: string
  label: string
  unit: string
  evidence: EvidenceValue
}

export interface TimelinePoint {
  tMs: number
  value: number | null
  provenance: Provenance
  quality: Quality
}

export interface TimelineEvent {
  wallMs: number
  monoMs: number
  kind: string
  detail: string
  severity: 'info' | 'marker' | 'warning' | 'error'
}

export interface Series {
  key: string
  label: string
  unit: string
  points: TimelinePoint[]
}

export interface SessionWindow {
  path: string
  intervalMs: number
  fromMs: number
  toMs: number
  maxPoints: number
  downsampled: boolean
  sourcePoints: number
  series: Series[]
}

export interface LiveBattery {
  state: string
  pct?: EvidenceValue | null
  discharge?: EvidenceValue | null
  charge?: EvidenceValue | null
  remainingWh?: EvidenceValue | null
  runtimeS?: EvidenceValue | null
  healthPct?: EvidenceValue | null
  fullChargeWh?: EvidenceValue | null
  designWh?: EvidenceValue | null
  ac?: EvidenceValue | null
}

export interface LiveCpu {
  utility?: EvidenceValue | null
  packagePower?: EvidenceValue | null
  packageDerived?: EvidenceValue | null
  freqMhz?: EvidenceValue | null
  c3Pct?: EvidenceValue | null
  coreCount: number
}

export interface LiveGpuAdapter {
  name: string
  discrete: boolean
  utilization?: EvidenceValue | null
  memoryMb?: EvidenceValue | null
  power?: EvidenceValue | null
  awake: string
}

export interface DisplayInfo {
  name: string
  primary: boolean
  width?: number | null
  height?: number | null
  refreshHz?: number | null
}

export interface LiveDisplay {
  count: number
  brightness?: EvidenceValue | null
  displays: DisplayInfo[]
}

export interface LiveSystem {
  ac?: EvidenceValue | null
  scheme?: string | null
  foregroundProcess?: string | null
  processCount: number
  netRx?: EvidenceValue | null
  netTx?: EvidenceValue | null
  storageActivity?: EvidenceValue | null
  storageRead?: EvidenceValue | null
  storageWrite?: EvidenceValue | null
}

export interface LiveSession {
  path: string
  label: string
  intervalMs: number
  startWallMs: number
  elapsedMs: number
  samples: number
  bytes: number
  batterySamples: number
}

export type SessionState = 'recording' | 'paused' | 'none' | 'outside-dir' | 'ended' | 'unreadable'

/**
 * Live view of the running session. Not an atomic machine snapshot: every
 * domain is sampled independently and its readings carry their own age.
 */
export interface LiveSnapshot {
  agent: AgentStatus
  generatedAt: number
  sessionState: SessionState
  sessionNote?: string | null
  sessionOutsideDir: boolean
  session?: LiveSession | null
  headline: MetricEvidence[]
  battery?: LiveBattery | null
  cpu?: LiveCpu | null
  gpus: LiveGpuAdapter[]
  display?: LiveDisplay | null
  system: LiveSystem
  collectors: CollectorState[]
  panes: Series[]
  events: TimelineEvent[]
  markers: number
}

/** Milestone-1 name for the same live view. */
export type LiveEvidence = LiveSnapshot

export interface SessionListEntry {
  file: string
  path: string
  label: string
  note: string
  startWallMs: number
  endWallMs: number
  durationS: number
  status: 'ok' | 'incomplete' | 'no-header' | 'unreadable'
  recovered: boolean
  mode: string
  samples: number
  intervalMs: number
  dischargeMedianW: number | null
  dischargeWh: number | null
  chargeWh: number | null
  cpuMedianPct: number | null
  coveragePct: number | null
  discontinuities: number | null
  markers: number | null
  collectors?: string[]
  /** Whole-session summary from the disposable index; null when unknown. */
  stats?: SessionStats | null
  bytes: number
  mtimeS: number | null
}

export interface FooterSummary {
  samples: number | null
  dischargeWh: number | null
  chargeWh: number | null
  dischargeMedianW: number | null
  dischargeCoveragePct: number | null
  dischargeUnknownS: number | null
  dischargeUnobservedS: number | null
  dischargeDiscontinuities: number | null
  cpuUtilityMedianPct: number | null
  cpuPkgDerivedMedianW: number | null
  collectorTimeouts: number | null
  sessionBytes: number | null
  recovered: boolean
}

export interface ProcessRow {
  pid: number
  name: string
  cpuPct?: EvidenceValue | null
}

export interface SessionSummary {
  path: string
  label: string
  note: string
  status: string
  recovered: boolean
  startWallMs: number
  endWallMs: number
  durationS: number
  intervalMs: number
  samples: number
  markers: number
  battery?: LiveBattery | null
  cpu?: LiveCpu | null
  gpus: LiveGpuAdapter[]
  display?: LiveDisplay | null
  system: LiveSystem
  processes: ProcessRow[]
  collectors: CollectorState[]
  events: TimelineEvent[]
  footer: FooterSummary
  hasFooter: boolean
}

export interface CapabilityField {
  name: string
  unit: string
  source: string
  provenance: string
  intervalMs: number
  requiresAdmin: boolean
  notes: string
  state: 'available' | 'unavailable'
  reason: string
  elevationWouldHelp: boolean
}

export interface CollectorCapability {
  name: string
  state: 'available' | 'degraded' | 'unavailable'
  available: number
  unavailable: number
  fields: CapabilityField[]
}

export interface CapabilityReport {
  elevated: boolean
  collectors: CollectorCapability[]
  available: number
  degraded: number
  unavailable: number
}

// ---------------------------------------------------------------------------
// Analyze workspace (milestone 2). Mirrors `pf-gui::dto` analyze types.
// Every number here was computed in Rust; the UI only renders it.
// ---------------------------------------------------------------------------

export interface CollectorSampleCount {
  name: string
  samples: number
}

export interface SessionStats {
  totalSamples: number
  ticks: number
  collectorSamples: CollectorSampleCount[]
  firstSampleWallMs: number | null
  lastSampleWallMs: number | null
  observedHz: number | null
  events: number
  markers: number
  timeouts: number
  recoveries: number
  errors: number
  discontinuities: number | null
  durationS: number | null
}

export interface RangeEnergy {
  dischargeWh: number | null
  chargeWh: number | null
  coveredS: number
  unknownS: number
  unobservedS: number
  discontinuities: number
  crossesDiscontinuity: boolean
  dischargePresent: boolean
  chargePresent: boolean
}

export interface RangeQuality {
  spanS: number
  coveredS: number
  unknownS: number
  unobservedS: number
  coverage: number | null
  discontinuities: number
  timeouts: number
  recoveries: number
  errors: number
  events: number
  markers: number
  samples: number
  collectorCounts: CollectorSampleCount[]
  staleCollectors: string[]
}

export interface MetricStat {
  key: string
  label: string
  unit: string
  n: number
  known: number
  median: number | null
  min: number | null
  max: number | null
  p10: number | null
  p90: number | null
  provenance: Provenance
}

export interface DomainStat {
  domain: string
  available: boolean
  metrics: MetricStat[]
}

export interface ProcessEntry {
  pid: number
  name: string
  ppid: number
  cpuMedianPct: number | null
  cpuMaxPct: number | null
  presence: number
  ticks: number
  firstSeenMs: number | null
  lastSeenMs: number | null
  startUnixMs: number | null
}

export interface ProcessAnalysis {
  ticks: number
  entries: ProcessEntry[]
  totalThreads: number | null
  inaccessible: number | null
  truncated: boolean | null
  incomplete: boolean | null
  note: string
}

export type ChangeDirection = 'increased' | 'decreased' | 'appeared' | 'disappeared'

export interface ChangeFact {
  domain: string
  key: string
  label: string
  unit: string
  reference: number | null
  during: number | null
  after: number | null
  delta: number | null
  direction: ChangeDirection
  confidence: 'high' | 'medium' | 'low'
  basis: string
  nBefore: number
  nDuring: number
  nAfter: number
}

export interface CategoryChange {
  domain: string
  label: string
  before: string | null
  during: string | null
  after: string | null
  basis: string
}

export type CorrelationStrength = 'insufficient' | 'negligible' | 'weak' | 'moderate' | 'strong'

export interface Correlation {
  x: string
  y: string
  label: string
  n: number
  effectiveN: number
  r: number | null
  coverage: number
  strength: CorrelationStrength
  note: string
}

export interface RangeAnalysis {
  path: string
  fromMs: number
  toMs: number
  durationS: number
  quality: RangeQuality
  energy: RangeEnergy
  domains: DomainStat[]
  processes: ProcessAnalysis
  changes: ChangeFact[]
  categorical: CategoryChange[]
  correlations: Correlation[]
}

// ---------------------------------------------------------------------------
// Compare workspace (milestone 3). Mirrors `pf-gui::dto` comparison types.
// Comparability is per-metric; there is deliberately no single boolean.
// ---------------------------------------------------------------------------

export type ComparabilityLevel =
  | 'compatible'
  | 'compatible_with_caveats'
  | 'weak'
  | 'not_comparable'

export type Direction =
  | 'increased'
  | 'decreased'
  | 'unchanged'
  | 'unavailable'
  | 'missing_a'
  | 'missing_b'

export type ReliabilityLevel =
  | 'distinguishable'
  | 'weak_evidence'
  | 'within_noise'
  | 'insufficient'

export interface ComparisonContext {
  powerScheme: string | null
  powerSource: string | null
  refreshHz: string | null
  displayCount: number | null
  gpuAdapters: string[]
  effectiveCadenceMs: number | null
}

export interface ComparisonSide {
  path: string
  label: string
  note: string
  status: 'ok' | 'incomplete'
  recovered: boolean
  wholeSession: boolean
  fromMs: number
  toMs: number
  durationS: number
  intervalMs: number
  coverage: number | null
  energy: RangeEnergy
  quality: RangeQuality
  context: ComparisonContext
}

export interface ComparabilityFinding {
  metric: string | null
  level: ComparabilityLevel
  reason: string
}

export interface Comparability {
  overall: ComparabilityLevel
  findings: ComparabilityFinding[]
}

export interface Reliability {
  level: ReliabilityLevel
  note: string
  noiseFloor: number | null
  effectSize: number | null
}

export interface DistributionSummary {
  n: number
  known: number
  min: number | null
  p10: number | null
  median: number | null
  p90: number | null
  max: number | null
  spread: number | null
}

export interface DistributionComparison {
  metric: string
  label: string
  unit: string
  a: DistributionSummary
  b: DistributionSummary
}

export interface MetricComparison {
  metric: string
  label: string
  domain: string
  unit: string
  a: number | null
  b: number | null
  absoluteDelta: number | null
  relativeDelta: number | null
  direction: Direction
  sampleCountA: number
  sampleCountB: number
  coverageA: number | null
  coverageB: number | null
  provenanceA: Provenance
  provenanceB: Provenance
  comparability: ComparabilityLevel
  comparabilityReason: string
  evidence: string
  reliability: Reliability
}

export interface RankedDifference {
  metric: string
  label: string
  domain: string
  unit: string
  delta: number
  relativeDelta: number | null
  direction: Direction
  relevance: number
  comparability: ComparabilityLevel
  reliability: ReliabilityLevel
  basis: string
}

export interface DomainMetricComparison {
  metric: string
  label: string
  unit: string
  aMedian: number | null
  bMedian: number | null
  delta: number | null
  knownA: number
  knownB: number
}

export interface DomainComparison {
  domain: string
  availableA: boolean
  availableB: boolean
  unavailableReason: string | null
  metrics: DomainMetricComparison[]
}

export interface ProcessDifference {
  identity: string
  displayName: string
  aCpuMedianPct: number | null
  bCpuMedianPct: number | null
  delta: number | null
  presence: 'both' | 'a_only' | 'b_only'
  aPresence: number
  bPresence: number
  aTicks: number
  bTicks: number
  aAmbiguous: boolean
  bAmbiguous: boolean
  note: string
}

export interface ProcessComparison {
  aTicks: number
  bTicks: number
  aIncomplete: boolean | null
  bIncomplete: boolean | null
  aTotalThreads: number | null
  bTotalThreads: number | null
  incomplete: boolean
  rows: ProcessDifference[]
}

export interface CategoricalDifference {
  key: string
  label: string
  a: string | null
  b: string | null
  state: 'same' | 'changed' | 'unavailable'
  note: string
}

export interface CorrelationComparison {
  x: string
  y: string
  label: string
  rA: number | null
  rB: number | null
  nA: number
  nB: number
  effectiveNA: number
  effectiveNB: number
  comparable: boolean
  note: string
}

export interface Caveat {
  scope: string
  severity: 'info' | 'warning' | 'critical'
  message: string
}

export interface EnergyComparison {
  totalWhA: number | null
  totalWhB: number | null
  avgPowerWA: number | null
  avgPowerWB: number | null
  normalizedWhA: number | null
  normalizedWhB: number | null
  durationRatio: number | null
  durationsSimilar: boolean
  preferredBasis: 'total_energy' | 'average_power'
  note: string
}

export interface QualityComparison {
  coverageA: number | null
  coverageB: number | null
  discontinuitiesA: number
  discontinuitiesB: number
  timeoutsA: number
  timeoutsB: number
  staleCollectorsA: string[]
  staleCollectorsB: string[]
  weakestSide: 'A' | 'B' | 'equal' | 'unknown'
  note: string
}

export interface ComparisonAnalysis {
  a: ComparisonSide
  b: ComparisonSide
  comparability: Comparability
  quality: QualityComparison
  energy: EnergyComparison
  headlineMetrics: MetricComparison[]
  rankedChanges: RankedDifference[]
  domains: DomainComparison[]
  processes: ProcessComparison
  categoricalDifferences: CategoricalDifference[]
  distributions: DistributionComparison[]
  correlations: CorrelationComparison[]
  caveats: Caveat[]
}

// ---------------------------------------------------------------------------
// Experiments workspace (milestone 4). Mirrors `pf-gui::dto` experiment types.
// Runs are references to sessions/ranges; raw evidence is never embedded.
// ---------------------------------------------------------------------------

export type RunGroup = 'baseline' | 'treatment'
export type TreatmentDirection = 'lower' | 'higher' | 'neutral'
export type ExperimentStatus = 'draft' | 'running' | 'complete' | 'aborted' | 'interrupted'
export type ExperimentClassification =
  | 'consistent_difference'
  | 'possible_difference'
  | 'within_noise'
  | 'no_difference_detected'
  | 'insufficient'
  | 'confounded'
export type ExperimentValidity =
  | 'valid'
  | 'valid_with_caveats'
  | 'weak'
  | 'confounded'
  | 'insufficient'
export type ConfounderState = 'controlled' | 'changed' | 'unknown'

export interface RunRecord {
  id: string
  label: string
  group: RunGroup
  order: number
  pairId?: number | null
  sessionPath: string
  fromMs?: number | null
  toMs?: number | null
  included: boolean
  source: 'existing' | 'guided'
  notes: string
  capturedMs?: number | null
}

export interface Confirmation {
  runId: string
  confirmedMs: number
  note: string
}

export interface GuidedState {
  phase: 'prepare' | 'settle' | 'measuring' | 'review' | 'done'
  runIndex: number
  runStartedMs?: number | null
  treatmentConfirmed: boolean
  confirmations: Confirmation[]
}

export interface CachedResult {
  analysisVersion: number
  classification: ExperimentClassification
  validity: ExperimentValidity
  absoluteDelta: number | null
  relativeDelta: number | null
  computedMs: number
}

export interface ExperimentRecord {
  schema: number
  id: string
  name: string
  question: string
  primaryMetric: string
  primaryLabel: string
  primaryUnit: string
  direction: TreatmentDirection
  pairing: 'paired' | 'unpaired'
  baselineLabel: string
  treatmentLabel: string
  settleS: number
  measureS: number
  repetitions: number
  randomized: boolean
  orderSeed?: number | null
  collectors?: string | null
  preset?: string | null
  notes: string
  status: ExperimentStatus
  createdMs: number
  updatedMs: number
  runs: RunRecord[]
  guided?: GuidedState | null
  lastResult?: CachedResult | null
}

export interface ExperimentSummary {
  id: string
  name: string
  question: string
  primaryMetric: string
  primaryLabel: string
  primaryUnit: string
  baselineLabel: string
  treatmentLabel: string
  status: ExperimentStatus
  baselineRuns: number
  treatmentRuns: number
  includedRuns: number
  missingRuns: number
  updatedMs: number
  lastResult?: CachedResult | null
}

export interface CreateExperimentRequest {
  name: string
  question: string
  primaryMetric: string
  primaryLabel: string
  primaryUnit: string
  direction: TreatmentDirection
  pairing: 'paired' | 'unpaired'
  baselineLabel: string
  treatmentLabel: string
  settleS: number
  measureS: number
  repetitions: number
  randomized: boolean
  collectors?: string | null
  preset?: string | null
  notes: string
}

export interface RunValidation {
  status: 'accepted' | 'accepted_with_caveats' | 'invalid'
  primaryPresent: boolean
  findings: string[]
}

export interface ExperimentContext {
  powerScheme: string | null
  powerSource: string | null
  refreshHz: string | null
  displayCount: number | null
  gpuAdapters: string[]
  effectiveCadenceMs: number | null
}

export interface SecondaryValue {
  metric: string
  label: string
  unit: string
  median: number | null
}

export interface RunOutcome {
  id: string
  label: string
  group: RunGroup
  order: number
  pairId: number | null
  included: boolean
  available: boolean
  unavailableReason: string | null
  fromMs: number
  toMs: number
  durationS: number
  primaryValue: number | null
  primaryKnown: number
  coverage: number | null
  discontinuities: number
  timeouts: number
  cadenceMs: number | null
  context: ExperimentContext
  collectors: string[]
  processIncomplete: boolean | null
  processNames: string[]
  secondary: SecondaryValue[]
  validation: RunValidation
}

export interface UnavailableRun {
  id: string
  label: string
  group: RunGroup
  order: number
  sessionPath: string
  reason: string
}

export interface GroupSummary {
  group: RunGroup
  runsTotal: number
  runsIncluded: number
  runsExcluded: number
  runsWithPrimary: number
  runsValid: number
  runsInvalid: number
  median: number | null
  mean: number | null
  spread: number | null
  min: number | null
  max: number | null
  coverageMedian: number | null
}

export interface PairedDifference {
  pairId: number
  baselineRun: string
  treatmentRun: string
  baseline: number
  treatment: number
  delta: number
}

export interface PairedReport {
  pairs: PairedDifference[]
  usablePairs: number
  medianDelta: number | null
  spread: number | null
  consistent: boolean
}

export interface NoiseAssessment {
  source: 'none' | 'run_to_run' | 'insufficient'
  estimate: number | null
  note: string
}

export interface EffectEstimate {
  baselineValue: number | null
  treatmentValue: number | null
  absoluteDelta: number | null
  relativeDelta: number | null
  direction: 'lower' | 'higher' | 'unchanged'
  effectSize: number | null
  confidenceInterval: [number, number] | null
  paired: boolean
}

export interface Confounder {
  key: string
  label: string
  state: ConfounderState
  evidence: string
  runs: string[]
}

export interface SecondaryComparison {
  metric: string
  label: string
  unit: string
  baselineMedian: number | null
  treatmentMedian: number | null
  delta: number | null
  comparable: boolean
  note: string
}

export interface ExperimentAnalysis {
  analysisVersion: number
  primaryMetric: string
  primaryLabel: string
  primaryUnit: string
  direction: TreatmentDirection
  pairing: 'paired' | 'unpaired'
  runs: RunOutcome[]
  baseline: GroupSummary
  treatment: GroupSummary
  effect: EffectEstimate
  paired: PairedReport | null
  noise: NoiseAssessment
  classification: ExperimentClassification
  validity: ExperimentValidity
  validityReasons: string[]
  summaryLines: string[]
  confounders: Confounder[]
  secondary: SecondaryComparison[]
  caveats: string[]
  unavailableRuns: UnavailableRun[]
}

export interface InterestingRegion {
  kind: 'peak' | 'sustained_increase' | 'marker' | 'discontinuity'
  domain: string
  label: string
  startMs: number
  endMs: number
  score: number
  detail: string
}

export interface DomainAvailability {
  domain: string
  available: boolean
  reason?: string | null
}

export interface AnalyzeOverview {
  path: string
  label: string
  note: string
  status: string
  recovered: boolean
  startWallMs: number
  endWallMs: number
  durationS: number
  intervalMs: number
  stats: SessionStats
  collectors: CollectorState[]
  events: TimelineEvent[]
  regions: InterestingRegion[]
  domains: DomainAvailability[]
  coveragePct: number | null
  discontinuities: number | null
  markers: number
  /** Whole-session downsampled series for the initial timeline/minimap. */
  series: Series[]
}

// ---------------------------------------------------------------------------
// Milestone 5: calibration, reports, exports, settings, diagnostics.
// ---------------------------------------------------------------------------

export type EvidenceClass = 'estimated'
export type ReferenceKind =
  | 'external_meter'
  | 'bench_meter'
  | 'oem_reading'
  | 'manual_reference'
  | 'fitted_against_estimate'
export type CalibrationQuality =
  | 'validated'
  | 'usable_with_caveats'
  | 'weak'
  | 'insufficient_evidence'
  | 'out_of_domain'

export interface CalibrationReference {
  input: number
  reference: number
  kind: ReferenceKind
  sourceLabel: string
  note: string
}

export interface CalibrationDatasetRef {
  sessionId: string
  fromMs?: number | null
  toMs?: number | null
  role: 'raw_evidence' | 'reference'
  note: string
}

export interface CalibrationExclusion {
  sessionId: string
  reason: string
}

export interface CalibrationModel {
  slopeWPerPct: number
  interceptW: number
  minBrightness: number
  r2Fit: number
}

export interface CalibrationValidation {
  n: number
  mae: number
  rmse: number
  medianAbsError: number
  maxAbsError: number
  r2: number | null
}

export interface CalibrationResidual {
  input: number
  predicted: number
  reference: number
  absError: number
  relativeError: number | null
  heldOut: boolean
  inDomain: boolean
}

export interface CalibrationApplicability {
  machine: string
  panel: string
  inputMin: number
  inputMax: number
  powerSource?: string | null
}

export interface CalibrationRecord {
  schemaVersion: number
  id: string
  revision: number
  createdAtMs: number
  target: string
  modelKind: string
  evidenceClass: EvidenceClass
  referenceKind: ReferenceKind
  independentReference: boolean
  model: CalibrationModel
  applicability: CalibrationApplicability
  references: CalibrationReference[]
  dataset: CalibrationDatasetRef[]
  exclusions: CalibrationExclusion[]
  validation?: CalibrationValidation | null
  residuals: CalibrationResidual[]
  quality: CalibrationQuality
  qualityReasons: string[]
  notes: string
  active: boolean
}

export interface CalibrationActiveEvent {
  id: string
  previousId?: string | null
  atMs: number
  action: string
}

export interface CreateCalibrationRequest {
  target: string
  referenceKind: ReferenceKind
  references: CalibrationReference[]
  dataset: CalibrationDatasetRef[]
  exclusions: CalibrationExclusion[]
  notes: string
  machine?: string | null
  panel?: string | null
  powerSource?: string | null
  activate: boolean
}

export interface ReportOptions {
  includeTimeline: boolean
  includeProcesses: boolean
  includeSecondary: boolean
  detailedCaveats: boolean
  redact: boolean
}

export interface ReportRequest {
  kind: 'analyze' | 'compare' | 'experiment' | 'calibration'
  path?: string | null
  fromMs?: number | null
  toMs?: number | null
  pathB?: string | null
  fromMsB?: number | null
  toMsB?: number | null
  id?: string | null
  options: ReportOptions
}

export interface ReportManifest {
  reportSchemaVersion: number
  kind: string
  generatedAtMs: number
  applicationVersion: string
  analysisVersion: string
  redaction: string
  sourceIds: string[]
  ranges: [number, number][]
  calibrationIds: string[]
  reproducibility: string
}

export interface BuiltReport {
  title: string
  html: string
  manifest: ReportManifest
  suggestedFileName: string
}

export interface SavedReport {
  path: string
  manifest: ReportManifest
}

export interface AppSettings {
  defaultPreset: string | null
  defaultCollectors: string | null
  redactionDefault: boolean
  theme: 'system' | 'light' | 'dark'
  reportExportDir: string | null
  chartIntervalMs: number
}

export interface Diagnostics {
  guiVersion: string
  toolVersion: string
  sessionsDir: string
  agentPipe: string
  elevated: boolean
  indexEntries: number
  indexSchema: number
  calibrationActiveIds: string[]
  experimentStoreDir: string
  experimentCount: number
  reportDir: string
  settingsFile: string
  logDir: string
  recentErrors: string[]
}
