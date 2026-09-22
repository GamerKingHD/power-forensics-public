//! Live evidence assembly.
//!
//! While an agent records, the GUI reads the tail of the **agent-published**
//! active session and reconstructs the latest provenance-aware readings with
//! the authoritative `pf_core::session` loader. The active session path comes
//! from the agent's `status`, never from filesystem ordering; when the agent is
//! unavailable, not recording, hasn't published a path, or owns a file outside
//! the configured directory, that is reported explicitly instead of guessed
//! at. No collector or analysis logic runs here.
//!
//! Each reading keeps its own timestamp; [`evidence::stamp_ages`] then ages
//! every reading against this view's `generated_at` so independently sampled
//! domains are never presented as one simultaneous machine snapshot.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pf_core::session::SessionData;

use crate::agent;
use crate::dto::{CapabilityReport, LiveSession, LiveSnapshot};
use crate::evidence;
use crate::sessions;

const LIVE_SERIES_POINTS: usize = 900;
const LIVE_EVENT_LIMIT: usize = 100;

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build a complete live view. Always returns usable state: an absent agent, an
/// unpublished session, an out-of-directory recording or an empty file yields
/// explicit unavailable evidence, never fabricated zeros.
pub fn build(sessions_dir: &Path, pipe_name: &str, caps: &CapabilityReport) -> LiveSnapshot {
    let agent_status = agent::agent_status(pipe_name);
    let generated_at = now_wall_ms();
    let empty = SessionData::default();
    let mut snap = LiveSnapshot {
        agent: agent_status.clone(),
        generated_at,
        session_state: "none".to_string(),
        session_note: None,
        session_outside_dir: false,
        session: None,
        headline: evidence::headline(&empty),
        battery: None,
        cpu: None,
        gpus: Vec::new(),
        display: None,
        system: evidence::system_view(&empty),
        collectors: evidence::collectors_view(&empty, None, 1000, caps, generated_at),
        panes: evidence::domain_series(&empty, LIVE_SERIES_POINTS, None),
        events: Vec::new(),
        markers: agent_status.markers_accepted,
    };

    if !agent_status.available {
        snap.session_note = Some("no monitoring agent is attached".to_string());
        return snap;
    }
    if !agent_status.running {
        snap.session_state = "ended".to_string();
        snap.session_note = Some("agent is attached but not recording".to_string());
        return snap;
    }
    let Some(published) = agent_status.session_path.clone() else {
        snap.session_note =
            Some("agent running — no active session file published yet".to_string());
        return snap;
    };
    let path = match sessions::confine_active_session(sessions_dir, &published) {
        Ok(p) => p,
        Err(_) => {
            snap.session_state = "outside-dir".to_string();
            snap.session_outside_dir = true;
            snap.session_note = Some(
                "Agent running — active session is outside configured GUI session directory"
                    .to_string(),
            );
            return snap;
        }
    };
    let (s, head, size) = match sessions::read_tail_session(&path) {
        Ok(v) => v,
        Err(e) => {
            snap.session_state = "unreadable".to_string();
            snap.session_note = Some(e.message);
            return snap;
        }
    };

    let header = head
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| pf_core::json::parse(l).ok());
    let interval_ms = header
        .as_ref()
        .and_then(|v| v.get("interval_ms").and_then(|m| m.num()))
        .unwrap_or(1000.0)
        .max(1.0) as u64;
    let start_wall_ms = header
        .as_ref()
        .and_then(|v| v.get("wall_ms").and_then(|m| m.num()))
        .unwrap_or(s.wall_base_ms as f64) as u64;
    let label = header
        .as_ref()
        .and_then(|v| v.get("note").and_then(|n| n.as_str()).map(str::to_string))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    let elapsed_ms = if start_wall_ms > 0 {
        generated_at.saturating_sub(start_wall_ms)
    } else {
        0
    };
    let window_samples = s.battery.len()
        + s.cpu.len()
        + s.gpu.len()
        + s.procs.len()
        + s.display.len()
        + s.net.len()
        + s.storage.len();

    snap.session = Some(LiveSession {
        path: path.to_string_lossy().to_string(),
        label,
        interval_ms,
        start_wall_ms,
        elapsed_ms,
        samples: window_samples as u64,
        bytes: size,
        battery_samples: s.battery.len() as u64,
    });
    snap.session_state = if agent_status.paused {
        "paused".to_string()
    } else {
        "recording".to_string()
    };
    snap.headline = evidence::headline(&s);
    snap.battery = evidence::battery_view(&s);
    snap.cpu = evidence::cpu_view(&s);
    snap.gpus = evidence::gpu_views(&s);
    snap.display = evidence::display_view(&s);
    snap.system = evidence::system_view(&s);
    // Absence of a collector is only claimed when the file's collector set is
    // known; the live tail alone cannot prove a collector never appeared.
    snap.collectors = evidence::collectors_view(&s, None, interval_ms, caps, generated_at);
    snap.panes = evidence::domain_series(&s, LIVE_SERIES_POINTS, None);
    snap.events = evidence::events_view(&s, LIVE_EVENT_LIMIT);
    snap.markers = agent_status.markers_accepted;
    evidence::stamp_ages(&mut snap, generated_at);
    snap
}
