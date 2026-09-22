//! Agent control + named-pipe client.
//!
//! The GUI is a controller, not a second monitoring engine. Live monitoring is
//! owned by the existing `power-forensics agent` process; this module speaks
//! its named-pipe verb contract (`pf_core::ipc::valid_verbs`) and, when no
//! agent is attached, launches the existing CLI binary. No collector or
//! scheduling logic is duplicated here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pf_core::ipc;
use pf_core::json::JVal;

use crate::dto::{AgentStatus, BridgeError};

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Cheap readiness probe: is a pipe server currently accepting? Uses a 1 ms
/// wait so callers never block when no agent is attached.
pub fn pipe_present(pipe_name: &str) -> bool {
    #[cfg(windows)]
    {
        let w = wide(pipe_name);
        // SAFETY: `w` is a live nul-terminated UTF-16 buffer.
        unsafe { WaitNamedPipeW(w.as_ptr(), 1) != 0 }
    }
    #[cfg(not(windows))]
    {
        let _ = pipe_name;
        false
    }
}

/// Send one verb to the running agent and return its decoded `data` JSON.
pub fn query_agent(
    pipe_name: &str,
    cmd: &str,
    arg_json: Option<&str>,
) -> Result<String, BridgeError> {
    if !ipc::is_valid_verb(cmd) {
        return Err(BridgeError::new(
            "bad-request",
            format!("unknown agent verb: {cmd}"),
        ));
    }
    let req = ipc::encode_request(cmd, arg_json);
    let resp = pf_agent::ipc_server::query(pipe_name, &req).map_err(|e| {
        if e.contains("no agent listening") || e.contains("pipe unavailable") {
            BridgeError::new("agent-unavailable", "no monitoring agent is attached").with_detail(e)
        } else {
            BridgeError::new("ipc-timeout", "agent IPC request failed").with_detail(e)
        }
    })?;
    ipc::decode_response(&resp).map_err(|e| BridgeError::new("ipc-error", e))
}

/// Read agent lifecycle state. Never fails: an absent agent is a first-class
/// state (`available: false`) rather than an error the UI must guess at.
pub fn agent_status(pipe_name: &str) -> AgentStatus {
    if !pipe_present(pipe_name) {
        return AgentStatus {
            available: false,
            reason: Some("no agent listening on the power-forensics pipe".to_string()),
            ..Default::default()
        };
    }
    match query_agent(pipe_name, "status", None) {
        Ok(data) => match pf_core::json::parse(&data) {
            Ok(v) => parse_status(&v),
            Err(e) => AgentStatus {
                available: false,
                reason: Some(format!("malformed agent status: {e}")),
                ..Default::default()
            },
        },
        Err(e) => AgentStatus {
            available: false,
            reason: Some(e.message),
            ..Default::default()
        },
    }
}

fn parse_status(v: &JVal) -> AgentStatus {
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let n = |k: &str| v.get(k).and_then(|x| x.num()).unwrap_or(0.0).max(0.0) as u64;
    AgentStatus {
        available: true,
        running: b("running"),
        paused: b("paused"),
        label: v
            .get("label")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        uptime_ms: n("uptime_ms"),
        started_wall_ms: n("started_wall_ms"),
        session_path: v
            .get("session_path")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        markers_accepted: n("markers_accepted"),
        markers_dropped: n("markers_dropped"),
        reason: None,
    }
}

/// Locate the `power-forensics` CLI binary (the agent owner). A packaged build
/// only trusts the binary shipped next to the GUI executable; the
/// workspace-relative fallbacks exist for uninstalled development builds and
/// are compiled out of release binaries so a stray executable in the process
/// working directory can never be executed.
pub fn locate_agent_binary() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("power-forensics.exe"));
        candidates.push(dir.join("power-forensics"));
    }
    #[cfg(debug_assertions)]
    if let Ok(cwd) = std::env::current_dir() {
        for rel in [
            "target/debug/power-forensics.exe",
            "target/release/power-forensics.exe",
            "../target/debug/power-forensics.exe",
            "../target/release/power-forensics.exe",
        ] {
            candidates.push(cwd.join(rel));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Options for a new monitoring run. All fields are optional; the agent
/// applies its own defaults, cadences, and preset resolution.
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub interval_ms: Option<u64>,
    pub collectors: Option<String>,
    pub preset: Option<String>,
    pub note: Option<String>,
}

/// Launch the existing agent as a detached process. Returns the child handle
/// so the caller can detect an immediate failure (e.g. single-instance lock).
/// The GUI does not run any collectors itself.
pub fn start_agent(
    binary: &Path,
    sessions_dir: &Path,
    opts: &StartOptions,
) -> Result<std::process::Child, BridgeError> {
    std::fs::create_dir_all(sessions_dir).map_err(|e| {
        BridgeError::new(
            "io-error",
            format!("cannot create {}", sessions_dir.display()),
        )
        .with_detail(e.to_string())
    })?;
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let out = sessions_dir.join(format!("pf-session-{wall}.jsonl"));

    let mut cmd = Command::new(binary);
    cmd.arg("agent");
    cmd.arg("--out").arg(&out);
    if let Some(iv) = opts.interval_ms.filter(|v| *v > 0) {
        cmd.arg("--interval-ms").arg(iv.to_string());
    }
    if let Some(c) = opts.collectors.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--collectors").arg(c);
    }
    if let Some(p) = opts.preset.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--preset").arg(p);
    }
    if let Some(n) = opts.note.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--note").arg(n);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(parent) = sessions_dir.parent()
        && !parent.as_os_str().is_empty()
    {
        cmd.current_dir(parent);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().map_err(|e| {
        BridgeError::new(
            "agent-launch-failed",
            format!("cannot launch agent {}", binary.display()),
        )
        .with_detail(e.to_string())
    })?;
    Ok(child)
}
