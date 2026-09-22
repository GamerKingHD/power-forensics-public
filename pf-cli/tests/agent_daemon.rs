//! CLI-level daemon integration: `power-forensics agent` must construct one
//! `Service`, serve the named pipe, and own the monitor lifecycle. Exercises
//! the real binary and the real named-pipe transport end to end (no faked
//! daemon, no in-process shortcut).
//!
//! Single test by design: the daemon and the `service`/`ipc --pipe` clients
//! all use the per-user default pipe name, so two concurrent tests in this
//! binary would cross-talk.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The agent and the `service`/`ipc --pipe` clients all use the per-user
/// default pipe name, so tests in this binary must not run concurrently.
static AGENT_LOCK: Mutex<()> = Mutex::new(());

fn agent_lock() -> std::sync::MutexGuard<'static, ()> {
    AGENT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait_exit(child: &mut Child, deadline: Instant) -> ExitStatus {
    loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => return s,
            None if Instant::now() >= deadline => panic!("process did not exit in time"),
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_power-forensics")
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("pf-agent-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn power-forensics")
}

fn wait_for_pipe(dir: &Path, deadline: Instant) {
    loop {
        let o = run(dir, &["service", "status", "--pipe"]);
        if o.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "agent pipe never came up: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// A real agent: the pipe comes up, pause/resume reach the live writer via
/// both `service` and `ipc send`, and the monitor's own stop-file path lets
/// the daemon finalize the session cleanly.
#[cfg(windows)]
#[test]
fn agent_daemon_serves_pause_resume_and_finalizes() {
    let _guard = agent_lock();
    let dir = unique_dir("daemon");
    let out = dir.join("agent.jsonl");
    let out_s = out.to_string_lossy().to_string();
    let child = Command::new(bin())
        .args([
            "agent",
            "--interval-ms",
            "200",
            "--collectors",
            "os",
            "--note",
            "agent-it",
            "--out",
            &out_s,
        ])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(30);

    wait_for_pipe(&dir, deadline);
    let st = run(&dir, &["service", "status", "--pipe"]);
    let st_out = String::from_utf8_lossy(&st.stdout);
    assert!(
        st_out.contains("running=true"),
        "daemon not running: {st_out}"
    );
    assert!(
        st_out.contains("paused=false"),
        "daemon unexpectedly paused: {st_out}"
    );

    // pause flips the shared writer flag; a later status observes it; resume clears it.
    let p = run(&dir, &["service", "pause", "--pipe"]);
    assert!(
        p.status.success(),
        "pause failed: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let p_out = String::from_utf8_lossy(&p.stdout);
    assert!(p_out.contains("\"paused\":true"), "{p_out}");
    let st = run(&dir, &["service", "status", "--pipe"]);
    let st_out = String::from_utf8_lossy(&st.stdout);
    assert!(st_out.contains("paused=true"), "{st_out}");
    let r = run(&dir, &["service", "resume", "--pipe"]);
    assert!(
        r.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let r_out = String::from_utf8_lossy(&r.stdout);
    assert!(r_out.contains("\"paused\":false"), "{r_out}");

    // The same live daemon is reachable through the generic `ipc send`.
    let s = run(&dir, &["ipc", "send", "pause", "--pipe"]);
    assert!(
        s.status.success(),
        "ipc send pause failed: {}",
        String::from_utf8_lossy(&s.stderr)
    );
    let s_out = String::from_utf8_lossy(&s.stdout);
    assert!(s_out.contains("\"paused\":true"), "{s_out}");
    let s = run(&dir, &["ipc", "send", "resume", "--pipe"]);
    assert!(
        s.status.success(),
        "ipc send resume failed: {}",
        String::from_utf8_lossy(&s.stderr)
    );

    // Shared-state contract: status label and the live snapshot label are the
    // same semantic fact and must agree (and match the session header note).
    let status_label = {
        let st = run(&dir, &["service", "status", "--pipe"]);
        let out = String::from_utf8_lossy(&st.stdout).to_string();
        out.lines()
            .rfind(|l| l.trim_start().starts_with('{'))
            .and_then(|l| pf_core::json::parse(l).ok())
            .and_then(|v| v.get("label").and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_else(|| panic!("no status label in:\n{out}"))
    };
    assert_eq!(status_label, "agent-it", "status label authority");
    // The live snapshot is published on the first collected sample; poll until
    // it carries the run label.
    let mut snap_label = None;
    let snap_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < snap_deadline {
        let o = run(&dir, &["ipc", "send", "snapshot", "--pipe"]);
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Ok(v) = pf_core::json::parse(text.trim()) {
                snap_label = v
                    .get("data")
                    .and_then(|d| d.get("label"))
                    .and_then(|l| l.as_str())
                    .map(String::from);
            }
        }
        if snap_label.as_deref() == Some("agent-it") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        snap_label.as_deref(),
        Some("agent-it"),
        "live snapshot label must match status/session label"
    );

    // Clean stop via the monitor's stop-file sentinel; daemon must finalize.
    std::fs::write(dir.join("stop.requested"), b"stop").unwrap();
    let timeout = Instant::now() + Duration::from_secs(30);
    let exit = loop {
        match guard.0.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() >= timeout => {
                panic!("agent did not stop after stop.requested")
            }
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    };
    assert!(exit.success(), "agent exited {exit:?}");
    let text = std::fs::read_to_string(&out).expect("session file missing");
    assert!(
        text.contains("session_footer"),
        "daemon did not finalize the session"
    );
    assert!(
        text.contains("\"note\":\"agent-it\""),
        "session header label must match status/snapshot label:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Markers sent over the real pipe must become session evidence: persisted
/// exactly once as `event` records, including markers queued while paused and
/// immediately before stop, and including non-ASCII text.
#[cfg(windows)]
#[test]
fn agent_pipe_marker_becomes_session_evidence() {
    let _guard = agent_lock();
    let dir = unique_dir("marker");
    let out = dir.join("marker.jsonl");
    let out_s = out.to_string_lossy().to_string();
    let child = Command::new(bin())
        .args([
            "agent",
            "--interval-ms",
            "200",
            "--collectors",
            "os",
            "--note",
            "marker-it",
            "--out",
            &out_s,
        ])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    wait_for_pipe(&dir, deadline);

    let mark = |text: &str| {
        let o = run(&dir, &["service", "marker", text, "--pipe"]);
        assert!(
            o.status.success(),
            "marker {text:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    };
    mark("checkpoint-alpha");
    // Non-ASCII survives the pipe + JSON escaping.
    mark("caf\u{00e9}-\u{0394}");

    // A marker queued while paused is still persisted (pausing stops
    // telemetry samples, not user annotations).
    let p = run(&dir, &["service", "pause", "--pipe"]);
    assert!(p.status.success());
    mark("while-paused");
    let r = run(&dir, &["service", "resume", "--pipe"]);
    assert!(r.status.success());
    mark("just-before-stop");

    let s = run(&dir, &["service", "stop", "--pipe"]);
    assert!(s.status.success(), "pipe stop failed");
    let exit = wait_exit(&mut guard.0, Instant::now() + Duration::from_secs(30));
    assert!(exit.success(), "agent exited {exit:?}");

    let text = std::fs::read_to_string(&out).expect("session file missing");
    for want in [
        "checkpoint-alpha",
        "caf\u{00e9}-\u{0394}",
        "while-paused",
        "just-before-stop",
    ] {
        assert_eq!(
            text.matches(want).count(),
            1,
            "marker {want:?} must appear exactly once in:\n{text}"
        );
    }
    let marker_events = text.matches("\"kind\":\"marker\"").count();
    assert_eq!(marker_events, 4, "expected 4 marker events:\n{text}");
    // The marker is an event record, ordered before the footer.
    let mi = text.find("\"kind\":\"marker\"").unwrap();
    let fi = text.find("\"type\":\"session_footer\"").unwrap();
    assert!(mi < fi, "marker must precede the footer");
    let _ = std::fs::remove_dir_all(&dir);
}

/// At most one agent owns monitoring per user: a second start must fail
/// cleanly and create no session, and ownership must be released when the
/// first process exits so a later start succeeds.
#[cfg(windows)]
#[test]
fn second_agent_is_rejected_then_ownership_is_released() {
    let _guard = agent_lock();
    let dir = unique_dir("singleton");
    let spawn_agent = |out: &Path| {
        let out_s = out.to_string_lossy().to_string();
        Command::new(bin())
            .args([
                "agent",
                "--interval-ms",
                "200",
                "--collectors",
                "os",
                "--note",
                "singleton",
                "--out",
                &out_s,
            ])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agent")
    };

    let first_out = dir.join("first.jsonl");
    let mut first = KillOnDrop(spawn_agent(&first_out));
    let deadline = Instant::now() + Duration::from_secs(30);
    wait_for_pipe(&dir, deadline);

    // Second start: rejected, nonzero, no session file.
    let second_out = dir.join("second.jsonl");
    let second = Command::new(bin())
        .args([
            "agent",
            "--interval-ms",
            "200",
            "--collectors",
            "os",
            "--note",
            "second",
            "--out",
            second_out.to_string_lossy().as_ref(),
        ])
        .current_dir(&dir)
        .output()
        .expect("spawn second agent");
    assert!(
        !second.status.success(),
        "second agent must fail while the first owns monitoring"
    );
    assert!(
        !second_out.exists(),
        "second agent must not create a session"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another agent"),
        "unexpected second-agent stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // Release ownership via the first agent's clean stop.
    std::fs::write(dir.join("stop.requested"), b"stop").unwrap();
    let exit = wait_exit(&mut first.0, Instant::now() + Duration::from_secs(30));
    assert!(exit.success(), "first agent exit {exit:?}");

    // A new agent can now acquire ownership and serve.
    let third_out = dir.join("third.jsonl");
    let mut third = KillOnDrop(spawn_agent(&third_out));
    wait_for_pipe(&dir, Instant::now() + Duration::from_secs(30));
    std::fs::write(dir.join("stop.requested"), b"stop").unwrap();
    let exit = wait_exit(&mut third.0, Instant::now() + Duration::from_secs(30));
    assert!(exit.success(), "third agent exit {exit:?}");
    assert!(third_out.exists(), "third agent should own a session");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pipe `stop` is authoritative end-to-end: over the real named pipe it must/// finalize the session (exactly one footer) and make the agent process exit 0.
#[cfg(windows)]
#[test]
fn agent_pipe_stop_finalizes_once_and_exits_zero() {
    let _guard = agent_lock();
    let dir = unique_dir("pipestop");
    let out = dir.join("stop.jsonl");
    let out_s = out.to_string_lossy().to_string();
    let child = Command::new(bin())
        .args([
            "agent",
            "--interval-ms",
            "200",
            "--collectors",
            "os",
            "--note",
            "agent-stop-it",
            "--out",
            &out_s,
        ])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(30);

    wait_for_pipe(&dir, deadline);
    let st = run(&dir, &["service", "status", "--pipe"]);
    assert!(
        String::from_utf8_lossy(&st.stdout).contains("running=true"),
        "agent not running before stop"
    );

    // Authoritative stop over the real pipe.
    let s = run(&dir, &["service", "stop", "--pipe"]);
    assert!(
        s.status.success(),
        "pipe stop failed: {}",
        String::from_utf8_lossy(&s.stderr)
    );
    let s_out = String::from_utf8_lossy(&s.stdout);
    assert!(s_out.contains("\"running\":false"), "{s_out}");

    let exit = wait_exit(&mut guard.0, Instant::now() + Duration::from_secs(30));
    assert!(exit.success(), "agent exited {exit:?} after pipe stop");
    let text = std::fs::read_to_string(&out).expect("session file missing");
    assert_eq!(
        text.matches("\"type\":\"session_footer\"").count(),
        1,
        "pipe stop must finalize exactly one footer: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
