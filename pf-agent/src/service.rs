//! Headless agent service state and named-pipe control verbs.
//!
//! Windows-first, no GUI code. The pipe verb surface stays frozen
//! (`pf_core::ipc::valid_verbs`); this module owns the live state behind
//! `status | snapshot | stop | pause | resume | marker |
//! recent_timeline | events | session_list`. There is no runtime `start`
//! verb: one agent process owns exactly one monitoring run. `pause`/`resume`
//! flip the shared flag the writer loop reads (`pause_flag`).
//! `stop` flips the shared flag the writer loop reads (`stop_flag`), so a
//! pipe `stop` finalizes the running session and ends the monitor. The native
//! `agent` command is the single authoritative background lifecycle.
//!
//! Writer-loop hookup (monitor.rs) is LIVE: `cmd_monitor_with` accepts an
//! optional `live_slot: Option<Arc<Mutex<StateSnapshot>>>` and
//! `pause_slot: Option<Arc<AtomicBool>>`. Per received sample the writer
//! publishes the live snapshot unless paused; while paused the sample is
//! dropped without advancing recording state (no session line, no counters,
//! no publish). The default `cmd_monitor` passes `None` for both, so the
//! legacy monitor path is byte-identical. A daemon wires the slots from a
//! `Service`: `cmd_monitor_with(opts, Some(svc.snapshot_handle()),
//! Some(svc.pause_flag()))`, and `Service::pause()` / `resume()` flip the
//! shared flag. `snapshot_from_sessiondata` is the single session-to-snapshot
//! builder (it supersedes the private copy formerly in `pf-cli/main.rs`).
//!
//! The native `agent` command wires the service, monitor loop and persistent
//! pipe server together. The optional elevated collector remains an isolated
//! spawn-and-poll helper: refusal, timeout or failure degrades that evidence
//! source without stopping the core collectors. The command pipe is restricted
//! to the current user and rejects remote clients.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pf_core::session::SessionData;
use pf_core::telemetry::escape_json;

use crate::ipc_server::{self, StateSnapshot};

/// Pending markers queued by the pipe service and drained by the writer loop
/// into the session as `event` records. Bounded: a slow/absent writer cannot
/// grow the queue without limit.
pub type MarkerQueue = Arc<Mutex<VecDeque<(u64, String)>>>;

/// Recent markers retained in `AgentState` for status/snapshot only. The
/// authoritative copy is the session file; this is a bounded view.
const MARKER_RECENT_CAP: usize = 64;
/// Hard cap on undrained pending markers (oldest dropped, counted).
const MARKER_PENDING_CAP: usize = 1024;

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// In-process agent lifecycle state (tray backend model).
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    pub running: bool,
    pub paused: bool,
    pub label: String,
    pub started_wall_ms: u64,
    /// Bounded recent markers (status/snapshot view). The session file is
    /// authoritative; this never grows past `MARKER_RECENT_CAP`.
    pub markers: Vec<(u64, String)>,
    /// Total markers accepted this lifecycle (for `status`).
    pub markers_total: u64,
    /// Pending markers dropped because the writer never drained them.
    pub markers_dropped: u64,
}

/// Headless service: lifecycle state + live pipe snapshot + pause flag.
/// All methods lock briefly and never block collectors. `Clone` shares the
/// underlying state/snapshot/pause cells so a daemon can hand one clone to
/// the writer loop and keep another for pipe dispatch.
#[derive(Clone)]
pub struct Service {
    state: Arc<Mutex<AgentState>>,
    snapshot: Arc<Mutex<StateSnapshot>>,
    pause: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Markers awaiting the writer loop. Shared with `cmd_monitor_with_control`.
    markers: MarkerQueue,
    /// Absolute path of the session the writer loop currently owns. Published
    /// by the writer loop once its output file exists and cleared when the run
    /// ends. This is the sole authority for "which session is active": a
    /// consumer must never infer it from filesystem ordering.
    session_path: Arc<Mutex<Option<String>>>,
    sessions_dir: String,
}

impl Service {
    pub fn new(sessions_dir: &str) -> Self {
        Service {
            state: Arc::new(Mutex::new(AgentState::default())),
            snapshot: Arc::new(Mutex::new(StateSnapshot::default())),
            pause: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            markers: Arc::new(Mutex::new(VecDeque::new())),
            session_path: Arc::new(Mutex::new(None)),
            sessions_dir: sessions_dir.to_string(),
        }
    }

    pub fn state_handle(&self) -> Arc<Mutex<AgentState>> {
        Arc::clone(&self.state)
    }

    pub fn snapshot_handle(&self) -> Arc<Mutex<StateSnapshot>> {
        Arc::clone(&self.snapshot)
    }

    /// Shared marker queue for the writer loop. Markers are persisted as
    /// session `event` records so a user annotation becomes forensic
    /// evidence; `AgentState::markers` is only a bounded recent view.
    pub fn marker_queue(&self) -> MarkerQueue {
        Arc::clone(&self.markers)
    }

    /// Shared pause cell for the writer loop. `pause()` / `resume()` /
    /// `stop()` flip it in lockstep with `AgentState::paused`.
    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.pause)
    }

    /// Shared authoritative stop cell for the writer loop. `stop()` sets it;
    /// the monitor finalizes (flush + exactly one footer) and exits 0. A new
    /// `start()` clears it so the cell tracks one lifecycle.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Shared cell the writer loop publishes its output path into. See
    /// [`Service::session_path`] for why this is authoritative.
    pub fn session_path_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.session_path)
    }

    /// The active session path, or `None` when no recording is owned.
    pub fn session_path(&self) -> Option<String> {
        self.session_path.lock().unwrap().clone()
    }

    pub fn start(&self, label: &str) {
        let mut s = self.state.lock().unwrap();
        s.running = true;
        s.paused = false;
        s.label = label.to_string();
        s.started_wall_ms = now_wall_ms();
        s.markers.clear();
        s.markers_total = 0;
        s.markers_dropped = 0;
        *self.session_path.lock().unwrap() = None;
        self.pause.store(false, Ordering::Relaxed);
        self.stop.store(false, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        let mut s = self.state.lock().unwrap();
        if s.running {
            s.paused = true;
            self.pause.store(true, Ordering::Relaxed);
        }
    }

    pub fn resume(&self) {
        self.state.lock().unwrap().paused = false;
        self.pause.store(false, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        s.running = false;
        s.paused = false;
        self.pause.store(false, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Queue a marker for persistence by the writer loop and keep a bounded
    /// recent copy for status. Markers are accepted while running, including
    /// while paused: pausing stops telemetry samples, not user annotations.
    pub fn add_marker(&self, text: &str, wall_ms: u64) {
        {
            let mut q = self.markers.lock().unwrap();
            if q.len() >= MARKER_PENDING_CAP {
                q.pop_front();
                let mut s = self.state.lock().unwrap();
                s.markers_dropped += 1;
            }
            q.push_back((wall_ms, text.to_string()));
        }
        let mut s = self.state.lock().unwrap();
        s.markers_total += 1;
        if s.markers.len() >= MARKER_RECENT_CAP {
            s.markers.remove(0);
        }
        s.markers.push((wall_ms, text.to_string()));
    }

    /// Pause flag for the writer loop: while set, snapshot publishes must be
    /// skipped (frozen), session writes continue.
    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }

    /// Replace the live snapshot unless paused. Returns false when the update
    /// was skipped due to pause (frozen); true when published.
    pub fn publish_snapshot(&self, snap: StateSnapshot) -> bool {
        if self.is_paused() {
            return false;
        }
        *self.snapshot.lock().unwrap() = snap;
        true
    }

    /// Verb dispatch returning the response *data* JSON fragment on `Ok`
    /// (caller wraps with `encode_response_ok`), or a human error on `Err`
    /// (caller wraps with `encode_response_err`). Never panics.
    pub fn handle(&self, cmd: &str, arg: Option<&str>) -> Result<String, String> {
        match cmd {
            "status" => {
                no_arg(arg)?;
                Ok(self.status_json())
            }
            "stop" => {
                no_arg(arg)?;
                self.stop();
                Ok("{\"running\":false,\"note\":\"agent stopped\"}".to_string())
            }
            "pause" => {
                no_arg(arg)?;
                self.pause();
                Ok(format!(
                    "{{\"paused\":{},\"note\":\"pause requested\"}}",
                    self.is_paused()
                ))
            }
            "resume" => {
                no_arg(arg)?;
                self.resume();
                Ok("{\"paused\":false,\"note\":\"agent resumed\"}".to_string())
            }
            "marker" => {
                let text = decode_arg_text(arg).ok_or("marker needs a text ARG".to_string())?;
                let wall = now_wall_ms();
                self.add_marker(&text, wall);
                let n = self.state.lock().unwrap().markers_total;
                Ok(format!(
                    "{{\"markers_accepted\":{n},\"note\":\"marker queued for session\",\"text\":\"{}\"}}",
                    escape_json(&text)
                ))
            }
            "snapshot" => {
                no_arg(arg)?;
                Ok(ipc_server::snapshot_json(&self.snapshot.lock().unwrap()))
            }
            "recent_timeline" => {
                no_arg(arg)?;
                let snap = self.snapshot.lock().unwrap();
                Ok(format!(
                    "{{\"label\":\"{}\",\"recent_watts\":[{}],\"events\":{}}}",
                    escape_json(&snap.label),
                    snap.recent
                        .iter()
                        .map(|o| match o {
                            Some(v) => format!("{v:.3}"),
                            None => "null".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    snap.events,
                ))
            }
            "events" => {
                no_arg(arg)?;
                let n = self.snapshot.lock().unwrap().events;
                Ok(format!(
                    "{{\"events\":{n},\"note\":\"live count only; details live in sessions\"}}"
                ))
            }
            "session_list" => {
                no_arg(arg)?;
                Ok(self.session_list_json())
            }
            _ => Err(format!("unknown verb: {cmd}")),
        }
    }

    fn status_json(&self) -> String {
        let s = self.state.lock().unwrap();
        let uptime = if s.running {
            now_wall_ms().saturating_sub(s.started_wall_ms)
        } else {
            0
        };
        let session_path = match self.session_path.lock().unwrap().as_deref() {
            Some(p) => format!("\"{}\"", escape_json(p)),
            None => "null".to_string(),
        };
        format!(
            "{{\"running\":{},\"paused\":{},\"label\":\"{}\",\"uptime_ms\":{uptime},\
             \"started_wall_ms\":{},\"session_path\":{session_path},\
             \"markers_accepted\":{},\"markers_dropped\":{}}}",
            s.running,
            s.paused,
            escape_json(&s.label),
            s.started_wall_ms,
            s.markers_total,
            s.markers_dropped,
        )
    }

    fn session_list_json(&self) -> String {
        match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok().map(|x| x.path()))
                    .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                    .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
                    .collect();
                names.sort();
                let items = names
                    .iter()
                    .map(|n| format!("\"{}\"", escape_json(n)))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{{\"sessions\":[{items}],\"count\":{},\"note\":\"live sessions dir listing\"}}",
                    names.len()
                )
            }
            Err(e) => format!(
                "{{\"sessions\":[],\"count\":0,\"note\":\"cannot list {}: {}\" }}",
                escape_json(&self.sessions_dir),
                escape_json(&e.to_string())
            ),
        }
    }
}

/// Decode a pipe `arg` fragment to plain text: JSON strings (`"lunch"`)
/// unwrap to `lunch`; anything else passes through verbatim. `None` stays
/// `None`.
fn decode_arg_text(arg: Option<&str>) -> Option<String> {
    let raw = arg?;
    let t = raw.trim();
    if t.is_empty() || t == "null" {
        return None;
    }
    if let Ok(v) = pf_core::json::parse(t)
        && let Some(s) = v.as_str()
    {
        return Some(s.to_string());
    }
    Some(t.to_string())
}

/// Reject a non-null ARG for verbs that take none, instead of silently
/// ignoring it. `None` / JSON `null` / empty pass.
fn no_arg(arg: Option<&str>) -> Result<(), String> {
    match decode_arg_text(arg) {
        None => Ok(()),
        Some(a) => Err(format!("verb takes no ARG (got \"{}\")", escape_json(&a))),
    }
}

/// Build a pipe snapshot from a loaded session: current values are the last
/// battery point, recent timeline the last 30 discharge watts. Pure; the
/// writer loop calls this per battery tick, the serve path per request.
///
/// This is the single session-to-`StateSnapshot` builder. `pf-cli/main.rs`
/// kept a private duplicate (`snapshot_from_session`); the CLI owner should
/// call this instead and delete that copy.
pub fn snapshot_from_sessiondata(s: &SessionData) -> StateSnapshot {
    let last = s.battery.last();
    StateSnapshot {
        watts: last.and_then(|b| b.discharge.val()),
        charge_w: last.and_then(|b| b.charge.val()),
        pct: last.and_then(|b| b.pct.val()),
        remaining_wh: last.and_then(|b| b.remaining_wh.val()),
        recent: s
            .battery
            .iter()
            .rev()
            .take(30)
            .rev()
            .map(|b| b.discharge.val())
            .collect(),
        events: s.events.len(),
        label: s.label.clone(),
    }
}

/// Publish `snap` into an optional live slot. `None` is a no-op so the
/// default writer path stays byte-identical. Pause checks belong to the
/// caller via `is_paused_flag()`.
pub fn set_live_snapshot(slot: &Option<Arc<Mutex<StateSnapshot>>>, snap: StateSnapshot) {
    if let Some(m) = slot
        && let Ok(mut s) = m.lock()
    {
        *s = snap;
    }
}

/// Read an optional pause flag. `None` means "never paused".
pub fn is_paused_flag(paused: &Option<Arc<std::sync::atomic::AtomicBool>>) -> bool {
    paused
        .as_ref()
        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> Service {
        Service::new("sessions")
    }

    #[test]
    fn status_reports_active_session_path_authoritatively() {
        let s = svc();
        s.start("auth");
        assert_eq!(s.session_path(), None);
        let v = pf_core::json::parse(&s.handle("status", None).unwrap()).unwrap();
        assert!(
            v.get("session_path")
                .map(|x| matches!(x, pf_core::json::JVal::Null))
                .unwrap_or(false),
            "an agent with no owned file must report null, not a guessed name"
        );
        // The writer loop publishes the exact path once the file exists.
        *s.session_path_handle().lock().unwrap() =
            Some("C:\\sessions\\pf-session-9.jsonl".to_string());
        let v = pf_core::json::parse(&s.handle("status", None).unwrap()).unwrap();
        assert_eq!(
            v.get("session_path").and_then(|x| x.as_str()),
            Some("C:\\sessions\\pf-session-9.jsonl")
        );
        assert!(
            v.get("started_wall_ms")
                .and_then(|x| x.num())
                .unwrap_or(0.0)
                > 0.0
        );
    }

    #[test]
    fn start_clears_a_previous_session_path() {
        let s = svc();
        s.start("a");
        *s.session_path_handle().lock().unwrap() = Some("old.jsonl".to_string());
        s.start("b");
        assert_eq!(
            s.session_path(),
            None,
            "a new lifecycle must not inherit the previous path"
        );
    }

    #[test]
    fn state_transitions_start_stop() {
        let s = svc();
        assert!(!s.state.lock().unwrap().running);
        s.start("demo");
        {
            let st = s.state.lock().unwrap();
            assert!(st.running && !st.paused);
            assert_eq!(st.label, "demo");
            assert!(st.markers.is_empty());
        }
        s.stop();
        {
            let st = s.state.lock().unwrap();
            assert!(!st.running && !st.paused);
        }
    }

    #[test]
    fn pause_resume_freezes_publish() {
        let s = svc();
        s.start("p");
        assert!(!s.is_paused());
        s.pause();
        assert!(s.is_paused());
        let snap = StateSnapshot {
            label: "x".to_string(),
            watts: Some(1.0),
            ..Default::default()
        };
        assert!(!s.publish_snapshot(snap.clone()));
        assert_ne!(s.snapshot.lock().unwrap().label, "x");
        s.resume();
        assert!(!s.is_paused());
        assert!(s.publish_snapshot(snap));
        assert_eq!(s.snapshot.lock().unwrap().label, "x");
    }

    #[test]
    fn pause_without_start_stays_unpaused() {
        let s = svc();
        let flag = s.pause_flag();
        s.pause();
        assert!(!s.is_paused());
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn pause_resume_stop_track_shared_flag() {
        let s = svc();
        let flag = s.pause_flag();
        s.start("p");
        assert!(!flag.load(Ordering::Relaxed));
        s.pause();
        assert!(s.is_paused());
        assert!(flag.load(Ordering::Relaxed));
        s.resume();
        assert!(!s.is_paused());
        assert!(!flag.load(Ordering::Relaxed));
        s.pause();
        assert!(flag.load(Ordering::Relaxed));
        s.stop();
        assert!(!s.is_paused());
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn stop_sets_shared_stop_flag_and_start_clears_it() {
        let s = svc();
        let flag = s.stop_flag();
        assert!(!flag.load(Ordering::Relaxed));
        s.start("p");
        assert!(!flag.load(Ordering::Relaxed));
        s.stop();
        assert!(
            flag.load(Ordering::Relaxed),
            "stop must signal the writer loop"
        );
        assert!(!s.state.lock().unwrap().running);
        // A new lifecycle must not inherit the previous stop.
        s.start("q");
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn handle_stop_sets_shared_stop_flag() {
        let s = svc();
        let flag = s.stop_flag();
        s.start("p");
        let r = s.handle("stop", None).unwrap();
        assert!(r.contains("\"running\":false"), "{r}");
        assert!(
            flag.load(Ordering::Relaxed),
            "pipe stop must reach the writer loop"
        );
    }

    #[test]
    fn handle_rejects_args_on_no_arg_verbs() {
        let s = svc();
        for verb in [
            "status",
            "snapshot",
            "recent_timeline",
            "events",
            "session_list",
            "stop",
            "pause",
            "resume",
        ] {
            assert!(s.handle(verb, Some("\"unexpected\"")).is_err(), "{verb}");
        }
        // Verbs that consume an ARG still accept them; `start` is not a
        // runtime verb (one process owns one run).
        assert!(s.handle("start", Some("\"demo\"")).is_err());
        assert!(s.handle("marker", Some("\"m\"")).is_ok());
    }

    #[test]
    fn marker_list_appends() {
        let s = svc();
        s.start("m");
        s.add_marker("a", 1);
        s.add_marker("b", 2);
        let st = s.state.lock().unwrap();
        assert_eq!(st.markers.len(), 2);
        assert_eq!(st.markers[0], (1, "a".to_string()));
        assert_eq!(st.markers[1], (2, "b".to_string()));
    }

    #[test]
    fn status_and_snapshot_agree_on_label_with_distinct_marker_authorities() {
        let s = svc();
        s.start("run-label");
        s.add_marker("m", 5);
        s.add_marker("n", 6);
        let status = s.handle("status", None).unwrap();
        let v = pf_core::json::parse(&status).unwrap();
        assert_eq!(v.get("label").and_then(|l| l.as_str()), Some("run-label"));
        // `markers_accepted` is the queue authority (2 accepted), not the
        // persisted-session-event count.
        assert_eq!(v.get("markers_accepted").and_then(|n| n.num()), Some(2.0));
        assert!(s.handle("status", None).unwrap().contains("uptime_ms"));
        // The live snapshot's label is the same semantic fact and must agree.
        assert!(s.publish_snapshot(StateSnapshot {
            label: "run-label".to_string(),
            events: 1,
            ..Default::default()
        }));
        let snap = s.handle("snapshot", None).unwrap();
        let sv = pf_core::json::parse(&snap).unwrap();
        assert_eq!(sv.get("label").and_then(|l| l.as_str()), Some("run-label"));
        assert_eq!(sv.get("events").and_then(|n| n.num()), Some(1.0));
    }

    #[test]
    fn handle_dispatch_status_stop_marker() {
        let s = svc();
        let st = s.handle("status", None).unwrap();
        assert!(st.contains("\"running\":false"), "{st}");
        // `start` is not a pipe verb: the process owns one run, started by
        // launching the agent. The internal method is used directly.
        assert!(s.handle("start", Some("\"demo\"")).is_err());
        s.start("demo");
        assert!(s.state.lock().unwrap().running);
        let m = s.handle("marker", Some("\"lunch\"")).unwrap();
        assert!(m.contains("\"markers_accepted\":1"), "{m}");
        let st2 = s.handle("status", None).unwrap();
        assert!(st2.contains("\"markers_accepted\":1"), "{st2}");
        assert!(st2.contains("uptime_ms"), "{st2}");
        s.handle("stop", None).unwrap();
        assert!(!s.state.lock().unwrap().running);
    }

    #[test]
    fn handle_snapshot_and_timeline_read_live_slot() {
        let s = svc();
        s.snapshot.lock().unwrap().watts = Some(6.5);
        s.snapshot.lock().unwrap().label = "live".to_string();
        let snap = s.handle("snapshot", None).unwrap();
        assert!(snap.contains("live"), "{snap}");
        let tl = s.handle("recent_timeline", None).unwrap();
        assert!(tl.contains("recent_watts"), "{tl}");
        let ev = s.handle("events", None).unwrap();
        assert!(ev.contains("\"events\""), "{ev}");
    }

    #[test]
    fn handle_session_list_degrades_to_empty_with_note() {
        let s = Service::new("no-such-dir-xyz-123");
        let j = s.handle("session_list", None).unwrap();
        assert!(j.contains("\"sessions\":[]"), "{j}");
        assert!(j.contains("\"count\":0"), "{j}");
        let v = pf_core::json::parse(&j).unwrap();
        assert_eq!(v.get("count").and_then(|n| n.num()), Some(0.0));
    }

    #[test]
    fn handle_rejects_unknown_verbs() {
        let s = svc();
        for verb in ["bogus", "diagnostics", "experiment_start", "start"] {
            assert!(s.handle(verb, None).is_err(), "{verb}");
        }
        assert!(s.handle("marker", None).is_err());
    }

    #[test]
    fn handle_pause_resume_flip_shared_flag_and_report_state() {
        let s = svc();
        // pause before start is a no-op (no running session to pause).
        let r = s.handle("pause", None).unwrap();
        assert!(r.contains("\"paused\":false"), "{r}");
        s.start("p");
        let r = s.handle("pause", None).unwrap();
        assert!(r.contains("\"paused\":true"), "{r}");
        assert!(s.pause_flag().load(Ordering::Relaxed));
        let r = s.handle("resume", None).unwrap();
        assert!(r.contains("\"paused\":false"), "{r}");
        assert!(!s.pause_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn snapshot_converter_takes_last_point() {
        use pf_core::session::BatteryPoint;
        use pf_core::telemetry::{ClockStamp, Telemetry};
        let stamp = ClockStamp {
            wall_millis: 1,
            mono_millis: 0,
        };
        let mut sd = SessionData {
            label: "t".to_string(),
            ..Default::default()
        };
        for w in [5.0, 7.0] {
            sd.battery.push(BatteryPoint {
                t: 0.0,
                discharge: Telemetry::measured(w, "battery", stamp),
                ..Default::default()
            });
        }
        let snap = snapshot_from_sessiondata(&sd);
        assert_eq!(snap.watts, Some(7.0));
        assert_eq!(snap.recent, vec![Some(5.0), Some(7.0)]);
        assert_eq!(snap.label, "t");
    }

    #[test]
    fn live_slot_helpers_none_is_noop() {
        set_live_snapshot(&None, StateSnapshot::default());
        assert!(!is_paused_flag(&None));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        assert!(is_paused_flag(&Some(flag)));
    }
}
