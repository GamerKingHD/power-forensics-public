//! CLI-level regression for calibration identity: the `report` command must
//! only turn brightness into watts when the persisted calibration belongs to
//! this machine/panel. Exercises the real binary dispatch, not just the
//! calib.rs helper.

use std::path::{Path, PathBuf};
use std::process::Command;

const SESSION: &str = concat!(
    "{\"type\":\"session_header\",\"wall_ms\":1000000,\"tool\":\"power-forensics\",",
    "\"version\":\"0.1.0\",\"interval_ms\":1000,\"note\":\"identity\"}\n",
    "{\"collector\":\"battery\",\"wall_ms\":1000000,\"mono_ms\":0,",
    "\"discharge_w\":{\"v\":5.7,\"p\":\"measured\"},",
    "\"remaining_mwh\":{\"v\":30000,\"p\":\"measured\"},",
    "\"full_charge_mwh\":{\"v\":37090,\"p\":\"measured\"},",
    "\"charge_pct\":{\"v\":55,\"p\":\"measured\"}}\n",
    "{\"collector\":\"display\",\"wall_ms\":1000000,\"mono_ms\":0,",
    "\"displays\":[{\"name\":\"PANEL\",\"brightness_pct\":{\"v\":60,\"p\":\"measured\"}}]}\n",
    "{\"collector\":\"battery\",\"wall_ms\":1001000,\"mono_ms\":1000,",
    "\"discharge_w\":{\"v\":5.7,\"p\":\"measured\"},",
    "\"remaining_mwh\":{\"v\":29990,\"p\":\"measured\"},",
    "\"full_charge_mwh\":{\"v\":37090,\"p\":\"measured\"},",
    "\"charge_pct\":{\"v\":55,\"p\":\"measured\"}}\n",
    "{\"collector\":\"display\",\"wall_ms\":1001000,\"mono_ms\":1000,",
    "\"displays\":[{\"name\":\"PANEL\",\"brightness_pct\":{\"v\":60,\"p\":\"measured\"}}]}\n",
    "{\"type\":\"session_footer\",\"wall_ms\":1002000,\"lines\":5,",
    "\"summary\":{\"discharge_wh\":0.01,\"samples\":2}}\n",
);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("pf-calib-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(p.join("sessions")).unwrap();
    p
}

fn write_calibration(dir: &Path, machine: &str, panel: &str) {
    let body = format!(
        "{{\"machine\":\"{machine}\",\"panel\":\"{panel}\",\"min_brightness\":0,\
         \"slope_w_per_pct\":0.05,\"intercept_w\":5,\"r2\":0.9,\"created_ms\":0,\
         \"levels\":[{{\"brightness\":0,\"median_w\":5}},{{\"brightness\":100,\"median_w\":10}}]}}"
    );
    std::fs::write(dir.join("sessions").join("display-calibration.json"), body).unwrap();
}

/// Run `report SESSION` with the binary's cwd set to `dir` (so the fixed
/// relative calibration path resolves there). Returns stdout.
fn run_report(dir: &Path) -> String {
    let session = dir.join("session.jsonl");
    std::fs::write(&session, SESSION).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_power-forensics"))
        .arg("report")
        .arg(&session)
        .current_dir(dir)
        .output()
        .expect("failed to spawn power-forensics");
    assert!(
        out.status.success(),
        "report exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn current_machine() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}

#[test]
fn matching_identity_makes_calibration_usable() {
    let dir = unique_dir("match");
    // Empty stored panel is grandfathered (legacy). Matching machine is what
    // matters and is deterministic.
    write_calibration(&dir, &current_machine(), "");
    let text = run_report(&dir);
    assert!(
        text.contains("derived from calibration"),
        "expected display power from matching calibration, got:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrong_machine_rejects_calibration() {
    let dir = unique_dir("machine");
    write_calibration(&dir, "SENTINEL-NOT-THIS-MACHINE", "");
    let text = run_report(&dir);
    assert!(
        !text.contains("derived from calibration"),
        "mismatched machine must not apply another machine's calibration:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrong_panel_rejects_calibration() {
    let dir = unique_dir("panel");
    write_calibration(&dir, &current_machine(), "SENTINEL-NOT-A-PANEL");
    let text = run_report(&dir);
    assert!(
        !text.contains("derived from calibration"),
        "mismatched panel must not apply another panel's calibration:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
