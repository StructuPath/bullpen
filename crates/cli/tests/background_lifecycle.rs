use std::fs;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

fn bullpen(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bullpen"))
        .args(args)
        .env("BULLPEN_HOME", home)
        .env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GLM_API_KEY")
        .env_remove("KIMI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap()
}

fn sessions(home: &std::path::Path) -> Vec<Value> {
    let output = bullpen(home, &["sessions", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn pre_spawn_failure_never_marks_the_session_running() {
    let dir = tempfile::tempdir().unwrap();
    // `spawn_detached` must create this directory before it can launch. A file
    // at the same path forces a deterministic pre-spawn failure.
    fs::write(dir.path().join("logs"), b"not a directory").unwrap();

    let output = bullpen(
        dir.path(),
        &["run", "--bg", "--json", "this dispatch must fail"],
    );
    assert!(!output.status.success());

    let rows = sessions(dir.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "idle");
    assert!(rows[0]["pid"].is_null());
}

#[test]
fn a_fast_worker_failure_is_terminal_not_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let output = bullpen(
        dir.path(),
        &["run", "--bg", "--json", "fail before a provider request"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dispatched: Value = serde_json::from_slice(&output.stdout).unwrap();
    let session_id = dispatched["session_id"].as_str().unwrap();
    let pid = dispatched["pid"].as_i64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = sessions(dir.path())
            .into_iter()
            .find(|row| row["id"] == session_id)
            .unwrap();
        if row["status"] == "failed" {
            assert!(row["pid"].is_null());
            break;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("background worker did not reach failed state: {row}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}
