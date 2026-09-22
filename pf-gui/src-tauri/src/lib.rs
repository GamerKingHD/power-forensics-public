//! power-forensics GUI bridge.
//!
//! Rust owns forensic truth. This crate is a narrow, non-invasive adapter:
//! it speaks the existing agent pipe and reuses the existing session loader,
//! dashboard freshness, capability inventory and recovery contracts. It adds
//! no energy integration, coverage, discontinuity, recovery or capability
//! classification of its own, and it never invents a value for a reading the
//! backend called unavailable.

mod agent;
mod analyze;
mod calibration;
mod capabilities;
mod cli;
mod compare;
mod dto;
mod elevation;
mod evidence;
mod experiment;
mod export;
mod index;
mod live;
mod logging;
mod report;
mod sessions;
mod settings;
mod single_instance;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use dto::{
    AgentStatus, AnalyzeOverviewDto, AppStatus, BridgeError, CapabilityReport,
    ComparisonAnalysisDto, CreateExperimentRequest, ExperimentAnalysisDto, ExperimentRecordDto,
    ExperimentSummaryDto, LiveEvidence, RangeAnalysisDto, RunValidationDto, SessionListEntry,
    SessionSummary, SessionWindow, StartRequest,
};

/// Small in-memory ring of recent bridge errors for Diagnostics. Bounded so it
/// can never grow without limit.
const MAX_RECENT_ERRORS: usize = 20;

fn recent_errors() -> &'static Mutex<VecDeque<String>> {
    static ERRORS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    ERRORS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn note_error(e: &BridgeError) {
    if let Ok(mut ring) = recent_errors().lock() {
        if ring.len() == MAX_RECENT_ERRORS {
            ring.pop_front();
        }
        ring.push_back(format!("{}: {}", e.code, e.message));
    }
    logging::error(format!(
        "bridge error [{}]: {}{}",
        e.code,
        e.message,
        e.detail
            .as_ref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default()
    ));
}

/// Shared, immutable GUI state resolved once at startup.
#[derive(Clone)]
struct AppState {
    sessions_dir: PathBuf,
    pipe: String,
    agent_binary: Option<PathBuf>,
}

impl AppState {
    fn new() -> Self {
        let sessions_dir = sessions::resolve_sessions_dir();
        // A fresh install or portable extract has no session directory yet;
        // create it so listing an empty library is a normal empty state rather
        // than an I/O error. Failure is non-fatal (the path may be read-only).
        if let Err(e) = std::fs::create_dir_all(&sessions_dir) {
            logging::warn(format!(
                "cannot create session directory {}: {e}",
                sessions_dir.display()
            ));
        }
        let state = AppState {
            sessions_dir,
            pipe: pf_agent::ipc_server::default_pipe_name(),
            agent_binary: agent::locate_agent_binary(),
        };
        logging::info(format!(
            "state: sessions_dir={}, agent_pipe={}, agent_binary={}",
            state.sessions_dir.to_string_lossy(),
            state.pipe,
            state
                .agent_binary
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<not found>".to_string())
        ));
        state
    }
}

/// Reject paths that escape the configured session directory. The frontend is
/// never given arbitrary filesystem access.
pub(crate) fn resolve_session_path(
    sessions_dir: &Path,
    requested: &str,
) -> Result<PathBuf, BridgeError> {
    let candidate = if requested.contains(['/', '\\']) {
        PathBuf::from(requested)
    } else {
        sessions_dir.join(requested)
    };
    let base = sessions_dir.canonicalize().map_err(|e| {
        BridgeError::new("io-error", "session directory is unavailable").with_detail(e.to_string())
    })?;
    let target = candidate.canonicalize().map_err(|e| {
        BridgeError::new("not-found", format!("session not found: {requested}"))
            .with_detail(e.to_string())
    })?;
    if !target.starts_with(&base) {
        return Err(BridgeError::new(
            "permission-denied",
            "session path is outside the session directory",
        ));
    }
    if target.extension().map(|x| x != "jsonl").unwrap_or(true) {
        return Err(BridgeError::new(
            "bad-request",
            "only .jsonl session files can be opened",
        ));
    }
    Ok(target)
}

async fn blocking<T, F>(f: F) -> Result<T, BridgeError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, BridgeError> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            note_error(&e);
            Err(e)
        }
        Err(e) => {
            let err = BridgeError::new("internal", "bridge task failed").with_detail(e.to_string());
            note_error(&err);
            Err(err)
        }
    }
}

#[tauri::command]
fn get_app_status(state: tauri::State<'_, AppState>) -> AppStatus {
    AppStatus {
        gui_version: env!("CARGO_PKG_VERSION").to_string(),
        tool_version: pf_agent::VERSION.to_string(),
        sessions_dir: state.sessions_dir.to_string_lossy().to_string(),
        elevated: elevation::is_elevated(),
        agent_pipe: state.pipe.clone(),
        agent_binary: state
            .agent_binary
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        platform: std::env::consts::OS.to_string(),
    }
}

#[tauri::command]
async fn get_agent_status(state: tauri::State<'_, AppState>) -> Result<AgentStatus, BridgeError> {
    let pipe = state.pipe.clone();
    blocking(move || Ok(agent::agent_status(&pipe))).await
}

#[tauri::command]
async fn get_live_evidence(state: tauri::State<'_, AppState>) -> Result<LiveEvidence, BridgeError> {
    let sessions_dir = state.sessions_dir.clone();
    let pipe = state.pipe.clone();
    blocking(move || {
        let caps = capabilities::current_report(elevation::is_elevated())?;
        Ok(live::build(&sessions_dir, &pipe, &caps))
    })
    .await
}

/// Alias for [`get_live_evidence`] with a name that matches the DTO
/// (`LiveSnapshot`): the view is assembled from independently sampled domains,
/// not one atomic machine snapshot.
#[tauri::command]
async fn get_live_snapshot(state: tauri::State<'_, AppState>) -> Result<LiveEvidence, BridgeError> {
    get_live_evidence(state).await
}

#[tauri::command]
async fn get_capabilities() -> Result<CapabilityReport, BridgeError> {
    blocking(move || capabilities::current_report(elevation::is_elevated())).await
}

#[tauri::command]
async fn list_sessions(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionListEntry>, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || sessions::list_sessions(&dir)).await
}

/// Discard and rebuild the derived session index from the authoritative JSONL.
/// The index is disposable; this exists so the UI can recover from a stale or
/// corrupt cache explicitly.
#[tauri::command]
async fn rebuild_session_index(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionListEntry>, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || sessions::rebuild_index(&dir)).await
}

#[tauri::command]
async fn open_session_summary(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<SessionSummary, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        let caps = capabilities::current_report(elevation::is_elevated())?;
        sessions::open_session_summary(&target, &caps)
    })
    .await
}

#[tauri::command]
async fn get_session_window(
    state: tauri::State<'_, AppState>,
    path: String,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    max_points: Option<usize>,
    pid: Option<u32>,
) -> Result<SessionWindow, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        sessions::session_window_range(
            &target,
            from_ms.unwrap_or(0),
            to_ms.unwrap_or(0),
            max_points.unwrap_or(0),
            pid,
        )
    })
    .await
}

/// Whole-session overview for the Analyze workspace: indexed statistics,
/// all recorded events, candidate interesting regions and domain
/// availability. Cached by file fingerprint; repeated opens do not rescan.
#[tauri::command]
async fn analyze_session_overview(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<AnalyzeOverviewDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        let caps = capabilities::current_report(elevation::is_elevated())?;
        // Bind the cached analysis to the active calibration revision so a
        // cache built under a previous calibration is never served.
        let cal =
            calibration::active_id(&dir, calibration::TARGET_DISPLAY_POWER).unwrap_or_default();
        analyze::overview_keyed(&target, &caps, &cal)
    })
    .await
}

/// Analyze one selected interval. Only the interval plus its before/after
/// context is parsed; the full recording is never materialized for a range.
#[tauri::command]
async fn analyze_session_range(
    state: tauri::State<'_, AppState>,
    path: String,
    from_ms: u64,
    to_ms: u64,
) -> Result<RangeAnalysisDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        let interval_ms = sessions::session_interval_ms(&target)?;
        analyze::range_analysis(&target, interval_ms, from_ms, to_ms)
    })
    .await
}

/// Compare two ranges (or whole sessions). Each side is bounded to its
/// selected interval; a missing bound means "whole session". Rust owns the
/// comparability gate, deltas, reliability and caveats.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn compare_sessions(
    state: tauri::State<'_, AppState>,
    path_a: String,
    from_ms_a: Option<u64>,
    to_ms_a: Option<u64>,
    path_b: String,
    from_ms_b: Option<u64>,
    to_ms_b: Option<u64>,
) -> Result<ComparisonAnalysisDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let a = resolve_session_path(&dir, &path_a)?;
        let b = resolve_session_path(&dir, &path_b)?;
        compare::compare(&a, from_ms_a, to_ms_a, &b, from_ms_b, to_ms_b)
    })
    .await
}

#[tauri::command]
async fn list_experiments(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ExperimentSummaryDto>, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::list(&dir)).await
}

#[tauri::command]
async fn get_experiment(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ExperimentRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::get(&dir, &id)).await
}

/// Persist an experiment definition (inputs only; raw sessions are never
/// copied). The id is assigned by the backend when absent.
#[tauri::command]
async fn save_experiment(
    state: tauri::State<'_, AppState>,
    record: ExperimentRecordDto,
) -> Result<ExperimentRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::save(&dir, record)).await
}

#[tauri::command]
async fn create_experiment(
    state: tauri::State<'_, AppState>,
    request: CreateExperimentRequest,
) -> Result<ExperimentRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::create(&dir, request)).await
}

#[tauri::command]
async fn delete_experiment(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::delete(&dir, &id)).await
}

/// Recompute the run-level analysis from persisted inputs. Missing sessions
/// are surfaced as unavailable runs rather than dropped.
#[tauri::command]
async fn analyze_experiment(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ExperimentAnalysisDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || experiment::analyze(&dir, &id)).await
}

/// Validate one guided run before it is accepted into a group.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn validate_experiment_run(
    state: tauri::State<'_, AppState>,
    session_path: String,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    primary_metric: String,
    primary_label: String,
    primary_unit: String,
) -> Result<RunValidationDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &session_path)?;
        experiment::validate_run(
            &dir,
            &target.to_string_lossy(),
            from_ms,
            to_ms,
            &primary_metric,
            &primary_label,
            &primary_unit,
        )
    })
    .await
}

#[tauri::command]
async fn start_monitoring(
    state: tauri::State<'_, AppState>,
    options: StartRequest,
) -> Result<AgentStatus, BridgeError> {
    let pipe = state.pipe.clone();
    let dir = state.sessions_dir.clone();
    let binary = state.agent_binary.clone().ok_or_else(|| {
        BridgeError::new(
            "agent-binary-missing",
            "power-forensics binary not found; build the workspace first",
        )
    })?;
    blocking(move || {
        if agent::pipe_present(&pipe) {
            return Err(BridgeError::new(
                "agent-already-running",
                "a monitoring agent already owns this user's session",
            ));
        }
        let opts = agent::StartOptions {
            interval_ms: options.interval_ms,
            collectors: options.collectors,
            preset: options.preset,
            note: options.note,
        };
        logging::info(format!(
            "agent launch requested: interval={:?} preset={:?} note={:?}",
            opts.interval_ms, opts.preset, opts.note
        ));
        let mut child = agent::start_agent(&binary, &dir, &opts)?;
        for _ in 0..40 {
            if agent::pipe_present(&pipe) {
                logging::info("agent command pipe is ready");
                return Ok(agent::agent_status(&pipe));
            }
            if let Ok(Some(status)) = child.try_wait() {
                let code = status.code().unwrap_or(-1);
                let msg = match code {
                    1 => "another agent already owns monitoring for this user",
                    2 => "agent could not start its command pipe",
                    _ => "agent exited before its pipe became ready",
                };
                logging::error(format!("agent launch failed: {msg} (exit code {code})"));
                return Err(BridgeError::new("agent-launch-failed", msg)
                    .with_detail(format!("exit code {code}")));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        logging::warn("agent launched but its command pipe did not become ready in time");
        Ok(AgentStatus {
            available: false,
            running: true,
            reason: Some("agent launched, but its command pipe is not ready yet".to_string()),
            ..Default::default()
        })
    })
    .await
}

async fn control(
    state: tauri::State<'_, AppState>,
    verb: &'static str,
) -> Result<AgentStatus, BridgeError> {
    let pipe = state.pipe.clone();
    blocking(move || {
        agent::query_agent(&pipe, verb, None)?;
        Ok(agent::agent_status(&pipe))
    })
    .await
}

#[tauri::command]
async fn pause_monitoring(state: tauri::State<'_, AppState>) -> Result<AgentStatus, BridgeError> {
    control(state, "pause").await
}

#[tauri::command]
async fn resume_monitoring(state: tauri::State<'_, AppState>) -> Result<AgentStatus, BridgeError> {
    control(state, "resume").await
}

#[tauri::command]
async fn stop_monitoring(state: tauri::State<'_, AppState>) -> Result<AgentStatus, BridgeError> {
    let pipe = state.pipe.clone();
    blocking(move || {
        agent::query_agent(&pipe, "stop", None)?;
        // The agent finalizes and exits; report the stopped lifecycle even if
        // the pipe is already gone by the time we re-query.
        Ok(AgentStatus {
            available: false,
            running: false,
            reason: Some("stop requested; session footer is being finalized".to_string()),
            ..Default::default()
        })
    })
    .await
}

#[tauri::command]
async fn add_marker(
    state: tauri::State<'_, AppState>,
    text: String,
) -> Result<AgentStatus, BridgeError> {
    let pipe = state.pipe.clone();
    blocking(move || {
        let arg = format!("\"{}\"", pf_core::telemetry::escape_json(&text));
        agent::query_agent(&pipe, "marker", Some(&arg))?;
        Ok(agent::agent_status(&pipe))
    })
    .await
}

/// Recover a torn session: rebuilds a footer on a copy via the existing CLI
/// contract. The original file is never modified.
#[tauri::command]
async fn recover_session(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<String, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        cli::recover_session(&target)
    })
    .await
}

/// Export a session to CSV next to the session file via the existing CLI.
#[tauri::command]
async fn export_session(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<String, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let target = resolve_session_path(&dir, &path)?;
        cli::export_session(&target)
    })
    .await
}

// ---------------------------------------------------------------------------
// Milestone 5: calibration, reports, exports, settings, diagnostics
// ---------------------------------------------------------------------------

#[tauri::command]
async fn list_calibrations(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<calibration::CalibrationRecordDto>, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || calibration::list(&dir)).await
}

#[tauri::command]
async fn get_calibration(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<calibration::CalibrationRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || calibration::get(&dir, &id)).await
}

#[tauri::command]
async fn create_calibration(
    state: tauri::State<'_, AppState>,
    request: calibration::CreateCalibrationRequest,
) -> Result<calibration::CalibrationRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || calibration::create(&dir, request)).await
}

#[tauri::command]
async fn set_active_calibration(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<calibration::CalibrationRecordDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || calibration::set_active(&dir, &id)).await
}

#[tauri::command]
async fn calibration_history(
    state: tauri::State<'_, AppState>,
    target: String,
) -> Result<Vec<calibration::ActiveEventDto>, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || calibration::active_history(&dir, &target)).await
}

#[tauri::command]
async fn generate_report(
    state: tauri::State<'_, AppState>,
    request: report::ReportRequest,
) -> Result<report::BuiltReport, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || report::build(&dir, &request)).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedReportDto {
    path: String,
    manifest: report::ReportManifestDto,
}

#[tauri::command]
async fn save_report(
    state: tauri::State<'_, AppState>,
    request: report::ReportRequest,
) -> Result<SavedReportDto, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || {
        let (path, manifest) = report::build_and_save(&dir, &request)?;
        Ok(SavedReportDto { path, manifest })
    })
    .await
}

#[tauri::command]
async fn export_session_tidy(
    state: tauri::State<'_, AppState>,
    path: String,
    redact: bool,
    force: bool,
) -> Result<String, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || export::export_tidy_csv(&dir, &path, redact, force)).await
}

#[tauri::command]
async fn export_analysis_json(
    state: tauri::State<'_, AppState>,
    request: report::ReportRequest,
) -> Result<String, BridgeError> {
    let dir = state.sessions_dir.clone();
    blocking(move || export::export_analysis_json(&dir, &request)).await
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> settings::AppSettings {
    settings::load(&state.sessions_dir)
}

#[tauri::command]
fn save_app_settings(
    state: tauri::State<'_, AppState>,
    settings: settings::AppSettings,
) -> Result<settings::AppSettings, BridgeError> {
    settings::save(&state.sessions_dir, &settings)?;
    Ok(settings::load(&state.sessions_dir))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsDto {
    gui_version: String,
    tool_version: String,
    sessions_dir: String,
    agent_pipe: String,
    elevated: bool,
    index_entries: usize,
    index_schema: u32,
    calibration_active_ids: Vec<String>,
    experiment_store_dir: String,
    experiment_count: usize,
    report_dir: String,
    settings_file: String,
    log_dir: String,
    recent_errors: Vec<String>,
}

#[tauri::command]
fn get_diagnostics(state: tauri::State<'_, AppState>) -> DiagnosticsDto {
    let dir = &state.sessions_dir;
    let index = index::SessionIndex::load(dir);
    let settings = settings::load(dir);
    let report_dir = settings::report_dir(dir, &settings)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string());
    DiagnosticsDto {
        gui_version: env!("CARGO_PKG_VERSION").to_string(),
        tool_version: pf_agent::VERSION.to_string(),
        sessions_dir: dir.to_string_lossy().to_string(),
        agent_pipe: state.pipe.clone(),
        elevated: elevation::is_elevated(),
        index_entries: index.as_ref().map(|i| i.entries.len()).unwrap_or(0),
        index_schema: index.map(|i| i.schema).unwrap_or(0),
        calibration_active_ids: calibration::active_id(dir, calibration::TARGET_DISPLAY_POWER)
            .into_iter()
            .collect(),
        experiment_store_dir: experiment::store_dir(dir).to_string_lossy().to_string(),
        experiment_count: experiment::store_count(dir),
        report_dir,
        settings_file: settings::settings_path(dir).to_string_lossy().to_string(),
        log_dir: logging::log_dir().to_string_lossy().to_string(),
        recent_errors: recent_errors()
            .lock()
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default(),
    }
}

/// Open the bounded application log directory in the OS file browser.
#[tauri::command]
fn open_log_dir() -> Result<(), BridgeError> {
    logging::open_log_dir().map_err(|e| {
        BridgeError::new("io-error", "could not open the log directory").with_detail(e)
    })
}

/// Run the desktop application.
pub fn run() {
    // Logging is initialized first, but is best-effort and cannot block startup.
    let log_dir = logging::init();
    logging::info(format!(
        "pf-gui {} starting (tool {}), log dir {}",
        env!("CARGO_PKG_VERSION"),
        pf_agent::VERSION,
        log_dir.to_string_lossy()
    ));

    // Per-user singleton: a second launch must not become a second controller
    // of the same agent. Prefer focusing the existing window over a hard error.
    let _singleton = match single_instance::acquire_gui() {
        Some(guard) => guard,
        None => {
            logging::warn("second GUI instance launched; focusing the existing window");
            single_instance::focus_existing_gui();
            return;
        }
    };

    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            get_app_status,
            get_agent_status,
            get_live_evidence,
            get_live_snapshot,
            get_capabilities,
            list_sessions,
            rebuild_session_index,
            open_session_summary,
            get_session_window,
            analyze_session_overview,
            analyze_session_range,
            compare_sessions,
            list_experiments,
            get_experiment,
            save_experiment,
            create_experiment,
            delete_experiment,
            analyze_experiment,
            validate_experiment_run,
            recover_session,
            export_session,
            start_monitoring,
            pause_monitoring,
            resume_monitoring,
            stop_monitoring,
            add_marker,
            list_calibrations,
            get_calibration,
            create_calibration,
            set_active_calibration,
            calibration_history,
            generate_report,
            save_report,
            export_session_tidy,
            export_analysis_json,
            get_settings,
            save_app_settings,
            get_diagnostics,
            open_log_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running power-forensics");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::EvidenceValue;
    use pf_core::json::parse;
    use pf_core::session;

    const SAMPLE: &str = "\
{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"unit test\"}\n\
{\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":8.4,\"p\":\"measured\"},\
\"charge_w\":{\"v\":null,\"p\":\"unavailable\",\"k\":2,\"q\":4,\"reason\":\"not charging\"},\
\"remaining_mwh\":{\"v\":20000,\"p\":\"measured\"},\"charge_pct\":{\"v\":71,\"p\":\"measured\"}}\n\
{\"type\":\"event\",\"wall_ms\":1100,\"mono_ms\":100,\"kind\":\"marker\",\"detail\":\"start\"}\n\
{\"type\":\"session_footer\",\"wall_ms\":61000,\"summary\":{\"samples\":1,\
\"discharge_median_w\":8.4,\"discharge_wh\":1.2,\"discharge_coverage_pct\":98.7,\"recovered\":false}}\n";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-gui-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unavailable_is_never_a_zero() {
        let s = session::load_session("t", SAMPLE).unwrap();
        let b = crate::evidence::battery_view(&s).unwrap();
        let charge = b.charge.unwrap();
        assert_eq!(charge.value, None);
        assert_eq!(charge.provenance, "unavailable");
        assert_eq!(charge.absence_kind.as_deref(), Some("not-sampled"));
        assert_eq!(charge.reason.as_deref(), Some("not charging"));
        // The available sibling keeps its measured value.
        assert_eq!(b.discharge.unwrap().value, Some(8.4));
    }

    #[test]
    fn unavailable_serializes_with_null_value_and_reason() {
        let ev = EvidenceValue::unavailable("W", "unsupported", "no sensor");
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"value\":null"), "{j}");
        assert!(j.contains("\"reason\":\"no sensor\""), "{j}");
        assert!(j.contains("\"absenceKind\":\"unsupported\""), "{j}");
        assert!(!j.contains("\"value\":0"), "{j}");
    }

    #[test]
    fn headline_from_empty_session_is_all_unavailable() {
        let s = session::SessionData::default();
        for m in crate::evidence::headline(&s) {
            assert_eq!(m.evidence.value, None, "{} must be unavailable", m.key);
            assert_eq!(m.evidence.provenance, "unavailable");
        }
    }

    #[test]
    fn capabilities_keep_hardware_reason_and_gate_on_elevation() {
        let json = r#"{"collectors":[
            {"name":"storage","fields":[
                {"name":"storage.disk.util_pct","unit":"%","source":"PDH","provenance":"measured","interval_ms":1000,"requires_admin":false,"notes":""},
                {"name":"storage.nvme_power","unit":"state","source":"NVMe IOCTL","provenance":"measured","interval_ms":0,"requires_admin":true,"notes":"needs admin"}
            ]},
            {"name":"nvme","fields":[
                {"name":"nvme.health","unit":"%","source":"none","provenance":"unavailable","interval_ms":0,"requires_admin":true,"notes":"absent on this drive"},
                {"name":"nvme.temp","unit":"C","source":"none","provenance":"unavailable","interval_ms":0,"requires_admin":false,"notes":"no sensor"}
            ]}
        ]}"#;
        let no = crate::capabilities::classify(json, false).unwrap();
        assert_eq!(no.degraded, 1);
        assert_eq!(no.unavailable, 1);
        let storage = no.collectors.iter().find(|c| c.name == "storage").unwrap();
        assert_eq!(storage.state, "degraded");
        let nvme_power = storage
            .fields
            .iter()
            .find(|f| f.name == "storage.nvme_power")
            .unwrap();
        assert_eq!(nvme_power.state, "unavailable");
        assert_eq!(nvme_power.reason, "requires administrator privileges");
        assert!(nvme_power.elevation_would_help);
        let nvme = no.collectors.iter().find(|c| c.name == "nvme").unwrap();
        assert_eq!(nvme.state, "unavailable");
        // A permanent hardware absence keeps its own reason, not "failed".
        assert_eq!(nvme.fields[0].reason, "absent on this drive");

        let yes = crate::capabilities::classify(json, true).unwrap();
        let storage = yes.collectors.iter().find(|c| c.name == "storage").unwrap();
        assert_eq!(storage.state, "available");
        let nvme = yes.collectors.iter().find(|c| c.name == "nvme").unwrap();
        assert_eq!(
            nvme.state, "unavailable",
            "elevation cannot create a missing sensor"
        );
    }

    #[test]
    fn list_sessions_reads_header_and_footer() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("pf-session-1000.jsonl"), SAMPLE).unwrap();
        // A footer-less file is a recoverable torn session.
        std::fs::write(
            dir.join("torn.jsonl"),
            "{\"type\":\"session_header\",\"wall_ms\":5,\"interval_ms\":1000,\"note\":\"x\"}\n\
             {\"collector\":\"battery\",\"wall_ms\":6,\"mono_ms\":0,\"discharge_w\":{\"v\":1.0,\"p\":\"measured\"}}\n",
        )
        .unwrap();
        let mut entries = crate::sessions::list_sessions(&dir).unwrap();
        entries.sort_by(|a, b| a.file.cmp(&b.file));
        let ok = entries
            .iter()
            .find(|e| e.file == "pf-session-1000.jsonl")
            .unwrap();
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.samples, 1);
        assert_eq!(ok.duration_s, 60.0);
        assert_eq!(ok.coverage_pct, Some(98.7));
        assert_eq!(ok.markers, Some(1));
        assert_eq!(ok.mode, "recorded");
        let torn = entries.iter().find(|e| e.file == "torn.jsonl").unwrap();
        assert_eq!(torn.status, "incomplete");
        assert_eq!(torn.mode, "incomplete");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_session_is_flagged() {
        let dir = temp_dir("recovered");
        let text = SAMPLE.replace("\"recovered\":false", "\"recovered\":true");
        let text = text.replace(
            "{\"type\":\"session_footer\"",
            "{\"type\":\"session_footer\",\"recovered\":true",
        );
        std::fs::write(dir.join("rec.jsonl"), text).unwrap();
        let entries = crate::sessions::list_sessions(&dir).unwrap();
        assert!(entries[0].recovered);
        assert_eq!(entries[0].mode, "recovered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_path_is_confined_to_session_dir() {
        let dir = temp_dir("confine");
        std::fs::write(dir.join("a.jsonl"), SAMPLE).unwrap();
        let ok = resolve_session_path(&dir, "a.jsonl").unwrap();
        assert!(ok.ends_with("a.jsonl"));
        let escaped = resolve_session_path(&dir, "..\\..\\windows\\system32\\drivers\\etc\\hosts");
        assert!(escaped.is_err());
        let outside = resolve_session_path(&dir, "C:\\Windows\\notepad.exe");
        assert!(outside.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_exposes_footer_evidence_and_processes() {
        let dir = temp_dir("summary");
        std::fs::write(dir.join("s.jsonl"), SAMPLE).unwrap();
        let caps = crate::capabilities::classify("{\"collectors\":[]}", false).unwrap();
        let path = dir.join("s.jsonl");
        let sum = crate::sessions::open_session_summary(&path, &caps).unwrap();
        assert!(sum.has_footer);
        assert_eq!(sum.status, "ok");
        assert_eq!(sum.markers, 1);
        assert_eq!(sum.footer.discharge_wh, Some(1.2));
        assert_eq!(sum.footer.discharge_coverage_pct, Some(98.7));
        assert_eq!(sum.battery.unwrap().pct.unwrap().value, Some(71.0));
        let window = crate::sessions::session_window(&path).unwrap();
        assert!(window.series.iter().any(|s| s.key == "battery_discharge_w"));
        let bat = window
            .series
            .iter()
            .find(|s| s.key == "battery_discharge_w")
            .unwrap();
        assert_eq!(bat.points[0].value, Some(8.4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_session_parses_recent_samples() {
        let dir = temp_dir("tail");
        std::fs::write(dir.join("live.jsonl"), SAMPLE).unwrap();
        let path = dir.join("live.jsonl");
        let (s, head, size) = crate::sessions::read_tail_session(&path).unwrap();
        assert!(size > 0);
        assert!(head.contains("session_header"));
        assert_eq!(s.battery.len(), 1);
        assert_eq!(s.battery[0].discharge.val(), Some(8.4));
        assert_eq!(
            parse(head.lines().next().unwrap())
                .unwrap()
                .get("note")
                .unwrap()
                .as_str(),
            Some("unit test")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn ipc_adapter_speaks_the_agent_pipe() {
        use pf_agent::ipc_server::{self, StateSnapshot};
        let pipe = format!("\\\\.\\pipe\\pf-gui-test-{}", std::process::id());
        let snap = StateSnapshot {
            label: "demo".to_string(),
            watts: Some(6.5),
            ..Default::default()
        };
        let name = pipe.clone();
        let server = std::thread::spawn(move || {
            let _ = ipc_server::serve_once(&name, snap, |_| String::new());
        });
        // The client retries within its own 5s window; the server waits for one
        // request. This exercises framing + request/response codec end to end.
        let data = crate::agent::query_agent(&pipe, "snapshot", None).unwrap();
        assert!(data.contains("demo"), "{data}");
        assert!(data.contains("6.5"), "{data}");
        let _ = server.join();
    }

    #[test]
    fn absent_agent_is_a_state_not_an_error() {
        let pipe = "\\\\.\\pipe\\pf-gui-definitely-not-running";
        let status = crate::agent::agent_status(pipe);
        assert!(!status.available);
        assert!(!status.running);
        assert!(status.reason.is_some());
    }
}
