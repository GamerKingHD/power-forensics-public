//! Data-transfer objects for the narrow GUI bridge.
//!
//! Every numeric reading crossing this boundary is an [`EvidenceValue`]:
//! value + provenance + quality + absence kind + reason + source + clock.
//! The frontend is never handed a bare `number | null` for forensic data,
//! so it cannot accidentally render an unavailable sensor as zero.
//!
//! The types here mirror backend evidence; they add no forensic meaning.

use serde::{Deserialize, Serialize};

/// Structured bridge error. Known forensic states carry a machine code so the
/// UI can distinguish "agent unavailable" from "unsupported hardware" instead
/// of showing a generic failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl BridgeError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        BridgeError {
            code: code.to_string(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BridgeError {}

/// A provenance-aware numeric reading.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceValue {
    /// `None` means unavailable. It is never coerced to 0.
    pub value: Option<f64>,
    /// "measured" | "derived" | "estimated" | "unavailable"
    pub provenance: String,
    /// "fresh" | "repeated" | "stale" | "error" | "unknown"
    pub quality: String,
    /// "unsupported" | "transient" | "not-sampled" | "stale" (absence only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absence_kind: Option<String>,
    /// Human reason for absence (absence only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub source: String,
    /// Collector that produced this reading (identifies which independently
    /// sampled domain it came from; never a system-wide snapshot claim).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub collector: String,
    pub unit: String,
    pub wall_ms: u64,
    pub mono_ms: u64,
    /// Age of this reading relative to the enclosing view's reference time
    /// (`generated_at` for a live snapshot, session end for a recording).
    /// `None` when there is no reference time. Computed in Rust, never guessed
    /// by the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
}

impl EvidenceValue {
    pub fn unavailable(unit: &str, kind: &str, reason: &str) -> Self {
        EvidenceValue {
            value: None,
            provenance: "unavailable".to_string(),
            quality: if kind == "stale" { "stale" } else { "unknown" }.to_string(),
            absence_kind: Some(kind.to_string()),
            reason: Some(reason.to_string()),
            source: String::new(),
            collector: String::new(),
            unit: unit.to_string(),
            wall_ms: 0,
            mono_ms: 0,
            age_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub gui_version: String,
    pub tool_version: String,
    pub sessions_dir: String,
    pub elevated: bool,
    pub agent_pipe: String,
    pub agent_binary: Option<String>,
    pub platform: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub available: bool,
    pub running: bool,
    pub paused: bool,
    pub label: String,
    pub uptime_ms: u64,
    pub started_wall_ms: u64,
    /// Authoritative path of the session the agent is recording, published by
    /// the agent's writer loop. `None` means no recording is owned; the GUI
    /// must never infer this from filesystem ordering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub markers_accepted: u64,
    pub markers_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Collector health rendered from live evidence + capability inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CollectorState {
    pub name: String,
    /// "available" | "degraded" | "unavailable" | "stale" | "timeout" | "recording"
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sample_wall_ms: Option<u64>,
    pub samples: u64,
    pub failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_hz: Option<f64>,
    pub requires_admin: bool,
    pub elevation_would_help: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetricEvidence {
    pub key: String,
    pub label: String,
    pub unit: String,
    pub evidence: EvidenceValue,
}

/// One point in a time series. `value: None` is a real gap and must be drawn
/// as a gap, never as zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimelinePoint {
    pub t_ms: u64,
    pub value: Option<f64>,
    pub provenance: String,
    pub quality: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEvent {
    pub wall_ms: u64,
    pub mono_ms: u64,
    pub kind: String,
    pub detail: String,
    /// "info" | "marker" | "warning" | "error"
    pub severity: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub key: String,
    pub label: String,
    pub unit: String,
    pub points: Vec<TimelinePoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionWindow {
    pub path: String,
    pub interval_ms: u64,
    /// Requested interval bounds (0 = unbounded on that side).
    pub from_ms: u64,
    pub to_ms: u64,
    pub max_points: usize,
    pub downsampled: bool,
    pub source_points: usize,
    pub series: Vec<Series>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveBattery {
    pub state: String,
    pub pct: Option<EvidenceValue>,
    pub discharge: Option<EvidenceValue>,
    pub charge: Option<EvidenceValue>,
    pub remaining_wh: Option<EvidenceValue>,
    pub runtime_s: Option<EvidenceValue>,
    pub health_pct: Option<EvidenceValue>,
    pub full_charge_wh: Option<EvidenceValue>,
    pub design_wh: Option<EvidenceValue>,
    pub ac: Option<EvidenceValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveCpu {
    pub utility: Option<EvidenceValue>,
    pub package_power: Option<EvidenceValue>,
    pub package_derived: Option<EvidenceValue>,
    pub freq_mhz: Option<EvidenceValue>,
    pub c3_pct: Option<EvidenceValue>,
    pub core_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveGpuAdapter {
    pub name: String,
    pub discrete: bool,
    pub utilization: Option<EvidenceValue>,
    pub memory_mb: Option<EvidenceValue>,
    pub power: Option<EvidenceValue>,
    pub awake: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    pub name: String,
    pub primary: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub refresh_hz: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveDisplay {
    pub count: usize,
    pub brightness: Option<EvidenceValue>,
    pub displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSystem {
    pub ac: Option<EvidenceValue>,
    pub scheme: Option<String>,
    pub foreground_process: Option<String>,
    pub process_count: usize,
    pub net_rx: Option<EvidenceValue>,
    pub net_tx: Option<EvidenceValue>,
    pub storage_activity: Option<EvidenceValue>,
    pub storage_read: Option<EvidenceValue>,
    pub storage_write: Option<EvidenceValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    pub path: String,
    pub label: String,
    pub interval_ms: u64,
    pub start_wall_ms: u64,
    pub elapsed_ms: u64,
    pub samples: u64,
    pub bytes: u64,
    pub battery_samples: u64,
}

/// Live view of the running session.
///
/// This is deliberately **not** called an atomic snapshot: each domain is
/// sampled independently and carries its own timestamp and age. `generated_at`
/// is when this view was assembled, `session_state` is the explicit recording
/// lifecycle, and `session_note` carries a truthful reason when the active
/// session is unknown or outside the configured directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    pub agent: AgentStatus,
    pub generated_at: u64,
    /// "recording" | "paused" | "none" | "outside-dir" | "ended" | "unreadable"
    pub session_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_note: Option<String>,
    /// True when the agent owns a session the GUI is not allowed to read
    /// (outside the configured session directory). No file is read in that
    /// case; the value is reported, never guessed at.
    pub session_outside_dir: bool,
    pub session: Option<LiveSession>,
    pub headline: Vec<MetricEvidence>,
    pub battery: Option<LiveBattery>,
    pub cpu: Option<LiveCpu>,
    pub gpus: Vec<LiveGpuAdapter>,
    pub display: Option<LiveDisplay>,
    pub system: LiveSystem,
    pub collectors: Vec<CollectorState>,
    /// Retained name for the chartable domain series in this view.
    pub panes: Vec<Series>,
    pub events: Vec<TimelineEvent>,
    pub markers: u64,
}

/// Backwards-compatible alias: the milestone-1 name for the same live view.
pub type LiveEvidence = LiveSnapshot;

/// Per-collector record count retained by the disposable index. Exists so a
/// session browser or analysis header can state whole-session totals without
/// rescanning the JSONL.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CollectorSampleCount {
    pub name: String,
    pub samples: u64,
}

/// Cheap, disposable whole-session summary computed during indexing. Counts
/// and clock bounds only: it stores no sample values, so it can never become a
/// second source of forensic truth.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub total_samples: u64,
    /// Distinct record timestamps (effective ticks), when derivable.
    pub ticks: u64,
    pub collector_samples: Vec<CollectorSampleCount>,
    pub first_sample_wall_ms: Option<u64>,
    pub last_sample_wall_ms: Option<u64>,
    /// Effective observed rate across the whole session (ticks / span).
    pub observed_hz: Option<f64>,
    pub events: u64,
    pub markers: u64,
    pub timeouts: u64,
    pub recoveries: u64,
    pub errors: u64,
    /// From the footer summary when present, else unknown.
    pub discontinuities: Option<f64>,
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionListEntry {
    pub file: String,
    pub path: String,
    pub label: String,
    pub note: String,
    pub start_wall_ms: u64,
    pub end_wall_ms: u64,
    pub duration_s: f64,
    /// "ok" | "incomplete" | "no-header"
    pub status: String,
    pub recovered: bool,
    /// "recorded" | "recovered" | "incomplete"
    pub mode: String,
    pub samples: u64,
    pub interval_ms: u64,
    pub discharge_median_w: Option<f64>,
    pub discharge_wh: Option<f64>,
    pub charge_wh: Option<f64>,
    pub cpu_median_pct: Option<f64>,
    pub coverage_pct: Option<f64>,
    pub discontinuities: Option<f64>,
    /// `None` when the file was too large to scan for markers (unknown, not 0).
    pub markers: Option<u64>,
    /// Collectors observed in the recording (from the derived index). Empty
    /// when unknown; never fabricated from capabilities.
    pub collectors: Vec<String>,
    /// Whole-session summary from the disposable index. `None` when the file
    /// was too large to scan (unknown, never guessed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<SessionStats>,
    pub bytes: u64,
    pub mtime_s: Option<u64>,
}

/// Typed view of the session footer summary; absent keys stay `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FooterSummary {
    pub samples: Option<f64>,
    pub discharge_wh: Option<f64>,
    pub charge_wh: Option<f64>,
    pub discharge_median_w: Option<f64>,
    pub discharge_coverage_pct: Option<f64>,
    pub discharge_unknown_s: Option<f64>,
    pub discharge_unobserved_s: Option<f64>,
    pub discharge_discontinuities: Option<f64>,
    pub cpu_utility_median_pct: Option<f64>,
    pub cpu_pkg_derived_median_w: Option<f64>,
    pub collector_timeouts: Option<f64>,
    pub session_bytes: Option<f64>,
    pub recovered: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRow {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: Option<EvidenceValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub path: String,
    pub label: String,
    pub note: String,
    pub status: String,
    pub recovered: bool,
    pub start_wall_ms: u64,
    pub end_wall_ms: u64,
    pub duration_s: f64,
    pub interval_ms: u64,
    pub samples: u64,
    pub markers: u64,
    /// Domain headline evidence, keyed for the UI.
    pub battery: Option<LiveBattery>,
    pub cpu: Option<LiveCpu>,
    pub gpus: Vec<LiveGpuAdapter>,
    pub display: Option<LiveDisplay>,
    pub system: LiveSystem,
    pub processes: Vec<ProcessRow>,
    pub collectors: Vec<CollectorState>,
    pub events: Vec<TimelineEvent>,
    pub footer: FooterSummary,
    pub has_footer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityField {
    pub name: String,
    pub unit: String,
    pub source: String,
    pub provenance: String,
    pub interval_ms: u64,
    pub requires_admin: bool,
    pub notes: String,
    /// "available" | "unavailable"
    pub state: String,
    pub reason: String,
    pub elevation_would_help: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CollectorCapability {
    pub name: String,
    /// "available" | "degraded" | "unavailable"
    pub state: String,
    pub available: usize,
    pub unavailable: usize,
    pub fields: Vec<CapabilityField>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityReport {
    pub elevated: bool,
    pub collectors: Vec<CollectorCapability>,
    pub available: usize,
    pub degraded: usize,
    pub unavailable: usize,
}

// ---------------------------------------------------------------------------
// Analyze workspace (milestone 2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RangeEnergyDto {
    pub discharge_wh: Option<f64>,
    pub charge_wh: Option<f64>,
    pub covered_s: f64,
    pub unknown_s: f64,
    pub unobserved_s: f64,
    pub discontinuities: usize,
    pub crosses_discontinuity: bool,
    pub discharge_present: bool,
    pub charge_present: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RangeQualityDto {
    pub span_s: f64,
    pub covered_s: f64,
    pub unknown_s: f64,
    pub unobserved_s: f64,
    pub coverage: Option<f64>,
    pub discontinuities: usize,
    pub timeouts: usize,
    pub recoveries: usize,
    pub errors: usize,
    pub events: usize,
    pub markers: usize,
    pub samples: usize,
    pub collector_counts: Vec<CollectorSampleCount>,
    pub stale_collectors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetricStatDto {
    pub key: String,
    pub label: String,
    pub unit: String,
    pub n: usize,
    pub known: usize,
    pub median: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub p10: Option<f64>,
    pub p90: Option<f64>,
    pub provenance: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DomainStatDto {
    pub domain: String,
    pub available: bool,
    pub metrics: Vec<MetricStatDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEntryDto {
    pub pid: u32,
    pub name: String,
    pub ppid: u32,
    pub cpu_median_pct: Option<f64>,
    pub cpu_max_pct: Option<f64>,
    pub presence: usize,
    pub ticks: usize,
    pub first_seen_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    pub start_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessAnalysisDto {
    pub ticks: usize,
    pub entries: Vec<ProcessEntryDto>,
    pub total_threads: Option<u64>,
    pub inaccessible: Option<usize>,
    pub truncated: Option<bool>,
    pub incomplete: Option<bool>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangeFactDto {
    pub domain: String,
    pub key: String,
    pub label: String,
    pub unit: String,
    pub reference: Option<f64>,
    pub during: Option<f64>,
    pub after: Option<f64>,
    pub delta: Option<f64>,
    /// "increased" | "decreased" | "appeared" | "disappeared"
    pub direction: String,
    /// "high" | "medium" | "low"
    pub confidence: String,
    /// Observational wording; never a causal claim.
    pub basis: String,
    pub n_before: usize,
    pub n_during: usize,
    pub n_after: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CategoryChangeDto {
    pub domain: String,
    pub label: String,
    pub before: Option<String>,
    pub during: Option<String>,
    pub after: Option<String>,
    pub basis: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CorrelationDto {
    pub x: String,
    pub y: String,
    pub label: String,
    pub n: usize,
    pub effective_n: usize,
    pub r: Option<f64>,
    pub coverage: f64,
    /// "insufficient" | "negligible" | "weak" | "moderate" | "strong"
    pub strength: String,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RangeAnalysisDto {
    pub path: String,
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub quality: RangeQualityDto,
    pub energy: RangeEnergyDto,
    pub domains: Vec<DomainStatDto>,
    pub processes: ProcessAnalysisDto,
    pub changes: Vec<ChangeFactDto>,
    pub categorical: Vec<CategoryChangeDto>,
    pub correlations: Vec<CorrelationDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InterestingRegionDto {
    /// "peak" | "sustained_increase" | "marker" | "discontinuity"
    pub kind: String,
    pub domain: String,
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub score: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DomainAvailabilityDto {
    pub domain: String,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeOverviewDto {
    pub path: String,
    pub label: String,
    pub note: String,
    pub status: String,
    pub recovered: bool,
    pub start_wall_ms: u64,
    pub end_wall_ms: u64,
    pub duration_s: f64,
    pub interval_ms: u64,
    pub stats: SessionStats,
    /// Session-wide collector health, including collectors absent from the
    /// recording (so the UI can adapt its tracks honestly).
    pub collectors: Vec<CollectorState>,
    pub events: Vec<TimelineEvent>,
    pub regions: Vec<InterestingRegionDto>,
    pub domains: Vec<DomainAvailabilityDto>,
    pub coverage_pct: Option<f64>,
    pub discontinuities: Option<f64>,
    pub markers: u64,
    /// Whole-session downsampled series, enough to render the initial
    /// timeline and the overview minimap without a second full parse.
    pub series: Vec<Series>,
}

// ---------------------------------------------------------------------------
// Compare workspace (milestone 3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonContextDto {
    pub power_scheme: Option<String>,
    pub power_source: Option<String>,
    pub refresh_hz: Option<String>,
    pub display_count: Option<usize>,
    pub gpu_adapters: Vec<String>,
    pub effective_cadence_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonSideDto {
    pub path: String,
    pub label: String,
    pub note: String,
    /// "ok" | "incomplete"
    pub status: String,
    pub recovered: bool,
    pub whole_session: bool,
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub interval_ms: u64,
    pub coverage: Option<f64>,
    pub energy: RangeEnergyDto,
    pub quality: RangeQualityDto,
    pub context: ComparisonContextDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComparabilityFindingDto {
    pub metric: Option<String>,
    /// "compatible" | "compatible_with_caveats" | "weak" | "not_comparable"
    pub level: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComparabilityDto {
    pub overall: String,
    pub findings: Vec<ComparabilityFindingDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReliabilityDto {
    /// "distinguishable" | "weak_evidence" | "within_noise" | "insufficient"
    pub level: String,
    pub note: String,
    pub noise_floor: Option<f64>,
    pub effect_size: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DistributionSummaryDto {
    pub n: usize,
    pub known: usize,
    pub min: Option<f64>,
    pub p10: Option<f64>,
    pub median: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
    pub spread: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DistributionComparisonDto {
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub a: DistributionSummaryDto,
    pub b: DistributionSummaryDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetricComparisonDto {
    pub metric: String,
    pub label: String,
    pub domain: String,
    pub unit: String,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub absolute_delta: Option<f64>,
    pub relative_delta: Option<f64>,
    /// "increased" | "decreased" | "unchanged" | "unavailable" | "missing_a" | "missing_b"
    pub direction: String,
    pub sample_count_a: usize,
    pub sample_count_b: usize,
    pub coverage_a: Option<f64>,
    pub coverage_b: Option<f64>,
    pub provenance_a: String,
    pub provenance_b: String,
    /// "compatible" | "compatible_with_caveats" | "weak" | "not_comparable"
    pub comparability: String,
    pub comparability_reason: String,
    pub evidence: String,
    pub reliability: ReliabilityDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RankedDifferenceDto {
    pub metric: String,
    pub label: String,
    pub domain: String,
    pub unit: String,
    pub delta: f64,
    pub relative_delta: Option<f64>,
    pub direction: String,
    pub relevance: f64,
    pub comparability: String,
    pub reliability: String,
    pub basis: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DomainMetricDto {
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub a_median: Option<f64>,
    pub b_median: Option<f64>,
    pub delta: Option<f64>,
    pub known_a: usize,
    pub known_b: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DomainComparisonDto {
    pub domain: String,
    pub available_a: bool,
    pub available_b: bool,
    pub unavailable_reason: Option<String>,
    pub metrics: Vec<DomainMetricDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessDifferenceDto {
    pub identity: String,
    pub display_name: String,
    pub a_cpu_median_pct: Option<f64>,
    pub b_cpu_median_pct: Option<f64>,
    pub delta: Option<f64>,
    /// "both" | "a_only" | "b_only"
    pub presence: String,
    pub a_presence: usize,
    pub b_presence: usize,
    pub a_ticks: usize,
    pub b_ticks: usize,
    pub a_ambiguous: bool,
    pub b_ambiguous: bool,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessComparisonDto {
    pub a_ticks: usize,
    pub b_ticks: usize,
    pub a_incomplete: Option<bool>,
    pub b_incomplete: Option<bool>,
    pub a_total_threads: Option<u64>,
    pub b_total_threads: Option<u64>,
    pub incomplete: bool,
    pub rows: Vec<ProcessDifferenceDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CategoricalDifferenceDto {
    pub key: String,
    pub label: String,
    pub a: Option<String>,
    pub b: Option<String>,
    /// "same" | "changed" | "unavailable"
    pub state: String,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CorrelationComparisonDto {
    pub x: String,
    pub y: String,
    pub label: String,
    pub r_a: Option<f64>,
    pub r_b: Option<f64>,
    pub n_a: usize,
    pub n_b: usize,
    pub effective_n_a: usize,
    pub effective_n_b: usize,
    pub comparable: bool,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CaveatDto {
    pub scope: String,
    /// "info" | "warning" | "critical"
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnergyComparisonDto {
    pub total_wh_a: Option<f64>,
    pub total_wh_b: Option<f64>,
    pub avg_power_w_a: Option<f64>,
    pub avg_power_w_b: Option<f64>,
    pub normalized_wh_a: Option<f64>,
    pub normalized_wh_b: Option<f64>,
    pub duration_ratio: Option<f64>,
    pub durations_similar: bool,
    /// "total_energy" | "average_power"
    pub preferred_basis: String,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QualityComparisonDto {
    pub coverage_a: Option<f64>,
    pub coverage_b: Option<f64>,
    pub discontinuities_a: usize,
    pub discontinuities_b: usize,
    pub timeouts_a: usize,
    pub timeouts_b: usize,
    pub stale_collectors_a: Vec<String>,
    pub stale_collectors_b: Vec<String>,
    /// "A" | "B" | "equal" | "unknown"
    pub weakest_side: String,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonAnalysisDto {
    pub a: ComparisonSideDto,
    pub b: ComparisonSideDto,
    pub comparability: ComparabilityDto,
    pub quality: QualityComparisonDto,
    pub energy: EnergyComparisonDto,
    pub headline_metrics: Vec<MetricComparisonDto>,
    pub ranked_changes: Vec<RankedDifferenceDto>,
    pub domains: Vec<DomainComparisonDto>,
    pub processes: ProcessComparisonDto,
    pub categorical_differences: Vec<CategoricalDifferenceDto>,
    pub distributions: Vec<DistributionComparisonDto>,
    pub correlations: Vec<CorrelationComparisonDto>,
    pub caveats: Vec<CaveatDto>,
}

// ---------------------------------------------------------------------------
// Experiments workspace (milestone 4)
// ---------------------------------------------------------------------------

/// One experimental run. This is a **reference** to a session/range; raw
/// evidence is never copied into the experiment file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunRecordDto {
    pub id: String,
    pub label: String,
    /// "baseline" | "treatment"
    pub group: String,
    pub order: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair_id: Option<usize>,
    pub session_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<u64>,
    pub included: bool,
    /// "existing" | "guided"
    pub source: String,
    #[serde(default)]
    pub notes: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationDto {
    pub run_id: String,
    pub confirmed_ms: u64,
    #[serde(default)]
    pub note: String,
}

/// Guided-run state persisted so an interrupted experiment can be reviewed and
/// resumed safely after an application restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuidedStateDto {
    /// "prepare" | "settle" | "measuring" | "review" | "done"
    pub phase: String,
    pub run_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_started_ms: Option<u64>,
    pub treatment_confirmed: bool,
    #[serde(default)]
    pub confirmations: Vec<ConfirmationDto>,
}

/// Recomputable result metadata. Never the authoritative analysis: the UI
/// recalculates from inputs whenever it opens the experiment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedResultDto {
    pub analysis_version: u32,
    pub classification: String,
    pub validity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absolute_delta: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_delta: Option<f64>,
    pub computed_ms: u64,
}

/// Persisted experiment definition. Versioned (`schema`); stores inputs and
/// optional cached result metadata, never raw sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentRecordDto {
    pub schema: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub question: String,
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    /// "lower" | "higher" | "neutral"
    pub direction: String,
    /// "paired" | "unpaired"
    pub pairing: String,
    pub baseline_label: String,
    pub treatment_label: String,
    pub settle_s: f64,
    pub measure_s: f64,
    pub repetitions: usize,
    pub randomized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collectors: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub notes: String,
    /// "draft" | "running" | "complete" | "aborted" | "interrupted"
    pub status: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub runs: Vec<RunRecordDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guided: Option<GuidedStateDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<CachedResultDto>,
}

/// Library row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentSummaryDto {
    pub id: String,
    pub name: String,
    pub question: String,
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    pub baseline_label: String,
    pub treatment_label: String,
    pub status: String,
    pub baseline_runs: usize,
    pub treatment_runs: usize,
    pub included_runs: usize,
    pub missing_runs: usize,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<CachedResultDto>,
}

/// Request body for creating a new experiment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateExperimentRequest {
    pub name: String,
    #[serde(default)]
    pub question: String,
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    #[serde(default = "default_direction")]
    pub direction: String,
    #[serde(default = "default_pairing")]
    pub pairing: String,
    #[serde(default)]
    pub baseline_label: String,
    #[serde(default)]
    pub treatment_label: String,
    #[serde(default)]
    pub settle_s: f64,
    #[serde(default)]
    pub measure_s: f64,
    #[serde(default)]
    pub repetitions: usize,
    #[serde(default)]
    pub randomized: bool,
    #[serde(default)]
    pub collectors: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub notes: String,
}

fn default_direction() -> String {
    "lower".to_string()
}

fn default_pairing() -> String {
    "unpaired".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunValidationDto {
    /// "accepted" | "accepted_with_caveats" | "invalid"
    pub status: String,
    pub primary_present: bool,
    pub findings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentContextDto {
    pub power_scheme: Option<String>,
    pub power_source: Option<String>,
    pub refresh_hz: Option<String>,
    pub display_count: Option<usize>,
    pub gpu_adapters: Vec<String>,
    pub effective_cadence_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcomeDto {
    pub id: String,
    pub label: String,
    /// "baseline" | "treatment"
    pub group: String,
    pub order: usize,
    pub pair_id: Option<usize>,
    pub included: bool,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub primary_value: Option<f64>,
    pub primary_known: usize,
    pub coverage: Option<f64>,
    pub discontinuities: usize,
    pub timeouts: usize,
    pub cadence_ms: Option<f64>,
    pub context: ExperimentContextDto,
    pub collectors: Vec<String>,
    pub process_incomplete: Option<bool>,
    pub process_names: Vec<String>,
    pub secondary: Vec<SecondaryValueDto>,
    pub validation: RunValidationDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecondaryValueDto {
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub median: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GroupSummaryDto {
    pub group: String,
    pub runs_total: usize,
    pub runs_included: usize,
    pub runs_excluded: usize,
    pub runs_with_primary: usize,
    pub runs_valid: usize,
    pub runs_invalid: usize,
    pub median: Option<f64>,
    pub mean: Option<f64>,
    pub spread: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub coverage_median: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairedDifferenceDto {
    pub pair_id: usize,
    pub baseline_run: String,
    pub treatment_run: String,
    pub baseline: f64,
    pub treatment: f64,
    pub delta: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairedReportDto {
    pub pairs: Vec<PairedDifferenceDto>,
    pub usable_pairs: usize,
    pub median_delta: Option<f64>,
    pub spread: Option<f64>,
    pub consistent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoiseAssessmentDto {
    /// "none" | "run_to_run" | "insufficient"
    pub source: String,
    pub estimate: Option<f64>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EffectEstimateDto {
    pub baseline_value: Option<f64>,
    pub treatment_value: Option<f64>,
    pub absolute_delta: Option<f64>,
    pub relative_delta: Option<f64>,
    pub direction: String,
    pub effect_size: Option<f64>,
    pub confidence_interval: Option<(f64, f64)>,
    pub paired: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfounderDto {
    pub key: String,
    pub label: String,
    pub state: String,
    pub evidence: String,
    pub runs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecondaryComparisonDto {
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub baseline_median: Option<f64>,
    pub treatment_median: Option<f64>,
    pub delta: Option<f64>,
    pub comparable: bool,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentAnalysisDto {
    pub analysis_version: u32,
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    pub direction: String,
    pub pairing: String,
    pub runs: Vec<RunOutcomeDto>,
    pub baseline: GroupSummaryDto,
    pub treatment: GroupSummaryDto,
    pub effect: EffectEstimateDto,
    pub paired: Option<PairedReportDto>,
    pub noise: NoiseAssessmentDto,
    pub classification: String,
    pub validity: String,
    pub validity_reasons: Vec<String>,
    pub summary_lines: Vec<String>,
    pub confounders: Vec<ConfounderDto>,
    pub secondary: Vec<SecondaryComparisonDto>,
    pub caveats: Vec<String>,
    /// Runs whose session could not be read; kept visible, never dropped.
    pub unavailable_runs: Vec<UnavailableRunDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnavailableRunDto {
    pub id: String,
    pub label: String,
    pub group: String,
    pub order: usize,
    pub session_path: String,
    pub reason: String,
}

/// User-selected options for a new monitoring run. Unset fields fall back to
/// the agent's own defaults and preset resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StartRequest {
    #[serde(default)]
    pub interval_ms: Option<u64>,
    #[serde(default)]
    pub collectors: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}
