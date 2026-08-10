//! Integration tests for the input-validation and failure-mode hardening
//! added alongside the shared kill-guard normalization. These tests run
//! against the real built binary so they exercise the CLI/MCP wire formats
//! end to end, not just the in-process functions.
//!
//! Each test points `PORTZILLA_DATA_DIR` at a fresh temporary directory so
//! runs never interfere with each other or a real user's lease store.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::process::Command as StdCommand;

fn cmd(data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("portzilla").unwrap();
    cmd.env("PORTZILLA_DATA_DIR", data_dir);
    cmd
}

#[cfg(unix)]
fn write_fake_killer(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let marker = dir.join("marker");
    let script = dir.join("portzilla-test-fake-kill");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

// ---- claim input validation ----

#[test]
fn claim_with_port_zero_is_rejected_with_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "0", "--tag", "x", "--pid", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value").and(predicate::str::contains("0")));
}

#[test]
fn claim_with_a_tag_at_exactly_the_limit_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let tag = "a".repeat(1024);

    cmd(dir.path())
        .args(["claim", "7100", "--tag", &tag, "--pid", "1"])
        .assert()
        .success();
}

#[test]
fn claim_with_an_oversized_tag_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let tag = "a".repeat(1025);

    cmd(dir.path())
        .args(["claim", "7101", "--tag", &tag, "--pid", "1"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("tag"));
}

#[test]
fn claim_with_a_session_at_exactly_the_limit_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let session = "s".repeat(512);

    cmd(dir.path())
        .args([
            "claim",
            "7102",
            "--tag",
            "x",
            "--pid",
            "1",
            "--session",
            &session,
        ])
        .assert()
        .success();
}

#[test]
fn claim_with_an_oversized_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let session = "s".repeat(513);

    cmd(dir.path())
        .args([
            "claim",
            "7103",
            "--tag",
            "x",
            "--pid",
            "1",
            "--session",
            &session,
        ])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("session"));
}

// ---- PORTZILLA_FAIL_CLOSED wiring ----

#[test]
fn fail_closed_default_is_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let input = serde_json::json!({
        "session_id": "sess",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "kill 1" },
    })
    .to_string();

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .stdout("");
}

#[test]
fn fail_closed_under_corrupt_store_emits_deny_for_claude_code() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let input = serde_json::json!({
        "session_id": "sess",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "kill 1" },
    })
    .to_string();

    let output = cmd(dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["hook", "claude-code"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output)
        .unwrap_or_else(|err| panic!("claude-code fail-closed must emit deny JSON, got {err}"));
    assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = json["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("permissionDecisionReason must be a string");
    assert!(
        reason.contains("PORTZILLA_FAIL_CLOSED"),
        "reason must mention fail-closed mode, got: {reason}"
    );
}

#[test]
fn fail_closed_under_corrupt_store_emits_deny_for_cursor() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let input = serde_json::json!({
        "command": "kill 1",
        "cwd": "/tmp",
        "sandbox": false,
        "conversation_id": "conv",
    })
    .to_string();

    let output = cmd(dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["hook", "cursor"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output)
        .unwrap_or_else(|err| panic!("cursor fail-closed must emit deny JSON, got {err}"));
    assert_eq!(json["permission"], "deny");
}

#[test]
fn fail_closed_under_corrupt_store_emits_deny_for_gemini() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let input = serde_json::json!({
        "session_id": "sess",
        "hook_event_name": "BeforeTool",
        "tool_name": "run_shell_command",
        "tool_input": { "command": "kill 1" },
    })
    .to_string();

    let output = cmd(dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["hook", "gemini"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output)
        .unwrap_or_else(|err| panic!("gemini fail-closed must emit deny JSON, got {err}"));
    assert_eq!(json["decision"], "deny");
}

#[test]
fn fail_closed_under_unparseable_payload_emits_deny_for_claude_code() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["hook", "claude-code"])
        .write_stdin(b"{ not valid json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("deny JSON expected");
    assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn fail_closed_under_oversized_stdin_emits_deny_for_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut payload = String::from("{\"command\":\"kill 1\"");
    payload.push_str(&" ".repeat(1 << 21));
    payload.push('}');

    let output = cmd(dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["hook", "cursor"])
        .write_stdin(payload.as_bytes())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("deny JSON expected");
    assert_eq!(json["permission"], "deny");
}

#[cfg(unix)]
#[test]
fn fail_closed_guard_with_corrupt_store_exits_2_and_does_not_run_the_command() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let script = write_fake_killer(dir.path());
    let marker = dir.path().join("marker");

    let exe = assert_cmd::cargo::cargo_bin("portzilla");
    let output = StdCommand::new(exe)
        .env("PORTZILLA_DATA_DIR", dir.path())
        .env("PORTZILLA_FAIL_CLOSED", "1")
        .args(["guard", "--", script.to_str().unwrap(), "1"])
        .output()
        .expect("failed to spawn portzilla guard");

    assert_eq!(
        output.status.code(),
        Some(2),
        "exit must be 2 under fail-closed"
    );
    assert!(
        !marker.exists(),
        "the command must not have run under fail-closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PORTZILLA_FAIL_CLOSED"),
        "stderr must mention fail-closed mode, got: {stderr}"
    );
}

// ---- stdin cap (fail-open default) ----

#[test]
fn oversized_stdin_fails_open_with_a_stderr_note_under_the_default_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut payload = String::from("{\"command\":\"kill 1\"");
    payload.push_str(&" ".repeat(1 << 21));
    payload.push('}');

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(payload.as_bytes())
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains("limit"));
}
