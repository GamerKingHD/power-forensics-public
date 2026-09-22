//! Narrow CLI orchestration for archive/export/recovery workflows.
//!
//! The GUI never reimplements the backend file contracts. These helpers spawn
//! the existing `power-forensics` binary for the few operations that are
//! implemented as CLI verbs (`recover`, `export`). Paths are confined to the
//! session directory by the caller; no arbitrary shell access is exposed.

use std::path::Path;
use std::process::Command;

use crate::agent;
use crate::dto::BridgeError;

pub struct CommandOutcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub fn run_cli(args: &[&str]) -> Result<CommandOutcome, BridgeError> {
    let binary = agent::locate_agent_binary().ok_or_else(|| {
        BridgeError::new(
            "agent-binary-missing",
            "power-forensics binary not found; build the workspace first",
        )
    })?;
    let output = Command::new(&binary).args(args).output().map_err(|e| {
        BridgeError::new("io-error", format!("cannot run {}", binary.display()))
            .with_detail(e.to_string())
    })?;
    Ok(CommandOutcome {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Rebuild a footer on a copy of a torn session. The original is never touched
/// (the backend writes `<file>.recovered.jsonl`).
pub fn recover_session(path: &Path) -> Result<String, BridgeError> {
    let outcome = run_cli(&["recover", &path.to_string_lossy()])?;
    if !outcome.success {
        return Err(BridgeError::new(
            if outcome.stderr.contains("already has a footer") {
                "session-not-recoverable"
            } else {
                "recovery-failed"
            },
            if outcome.stderr.is_empty() {
                "recovery failed".to_string()
            } else {
                outcome.stderr
            },
        ));
    }
    Ok(outcome.stdout)
}

/// Export a session to long-format CSV next to the session file.
pub fn export_session(path: &Path) -> Result<String, BridgeError> {
    let out = path.with_extension("csv");
    let outcome = run_cli(&[
        "export",
        &path.to_string_lossy(),
        "--out",
        &out.to_string_lossy(),
        "--force",
    ])?;
    if !outcome.success {
        return Err(BridgeError::new(
            "export-failed",
            if outcome.stderr.is_empty() {
                "export failed".to_string()
            } else {
                outcome.stderr
            },
        ));
    }
    Ok(out.to_string_lossy().to_string())
}
