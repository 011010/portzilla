//! End-to-end regression tests for the confirmed hook-path kill-guard
//! bypasses: `sh -c '<kill>'`, `env <kill>`, and `command <kill>` used to
//! reach the harness adapters as raw command strings and slip past
//! detection, because the `sh -c` unwrap only existed in the universal
//! `portzilla guard` wrapper and wrapper prefixes like `env`/`command` were
//! not stripped at all. Normalization now lives in the harness-agnostic
//! core (`src/guard.rs`), so every adapter gets it for free — these tests
//! prove that end to end against the real binary, one per harness.

use assert_cmd::Command;
use serde_json::Value;

fn cmd(data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("portzilla").unwrap();
    cmd.env("PORTZILLA_DATA_DIR", data_dir);
    cmd
}

/// Claims a live lease on port 3000 owned by `--session other-session`,
/// using the test process's own PID so the lease's PID is a REAL,
/// currently-alive process (required to exercise the live-lease deny path
/// against the real system PID checker).
fn claim_foreign_lease(data_dir: &std::path::Path, pid: u32) {
    cmd(data_dir)
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();
}

/// The confirmed bypass shapes (plus the payload-quoted variants the hook
/// JSON transports intact). Each must be denied once normalization is
/// shared with the core.
fn bypass_commands(pid: u32) -> Vec<String> {
    vec![
        format!("sh -c 'kill {pid}'"),
        format!("env kill {pid}"),
        format!("command kill {pid}"),
    ]
}

fn claude_code_payload(command: &str) -> String {
    serde_json::json!({
        "session_id": "my-session",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_use_id": "toolu_01"
    })
    .to_string()
}

fn cursor_payload(command: &str) -> String {
    serde_json::json!({
        "command": command,
        "cwd": "/tmp",
        "sandbox": false,
        "conversation_id": "conv-unrelated"
    })
    .to_string()
}

fn gemini_payload(command: &str) -> String {
    serde_json::json!({
        "session_id": "sess-unrelated",
        "hook_event_name": "BeforeTool",
        "tool_name": "run_shell_command",
        "tool_input": { "command": command }
    })
    .to_string()
}

fn hook_stdout(data_dir: &std::path::Path, hook: &str, payload: &str) -> Vec<u8> {
    cmd(data_dir)
        .args(["hook", hook])
        .write_stdin(payload)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

#[test]
fn hook_claude_code_denies_every_confirmed_bypass_shape() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    claim_foreign_lease(dir.path(), own_pid);

    for command in bypass_commands(own_pid) {
        let output = hook_stdout(dir.path(), "claude-code", &claude_code_payload(&command));
        let json: Value = serde_json::from_slice(&output)
            .unwrap_or_else(|err| panic!("`{command}`: stdout must be deny JSON, got {err}"));
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecision"], "deny",
            "bypass not closed for claude-code: `{command}`"
        );
        let reason = json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("permissionDecisionReason must be a string");
        assert!(reason.contains("3000"));
    }
}

#[test]
fn hook_cursor_denies_every_confirmed_bypass_shape() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    claim_foreign_lease(dir.path(), own_pid);

    for command in bypass_commands(own_pid) {
        let output = hook_stdout(dir.path(), "cursor", &cursor_payload(&command));
        let json: Value = serde_json::from_slice(&output)
            .unwrap_or_else(|err| panic!("`{command}`: stdout must be JSON, got {err}"));
        assert_eq!(
            json["permission"], "deny",
            "bypass not closed for cursor: `{command}`"
        );
        assert!(json["agent_message"].as_str().unwrap().contains("3000"));
    }
}

#[test]
fn hook_gemini_denies_every_confirmed_bypass_shape() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    claim_foreign_lease(dir.path(), own_pid);

    for command in bypass_commands(own_pid) {
        let output = hook_stdout(dir.path(), "gemini", &gemini_payload(&command));
        let json: Value = serde_json::from_slice(&output)
            .unwrap_or_else(|err| panic!("`{command}`: stdout must be JSON, got {err}"));
        assert_eq!(
            json["decision"], "deny",
            "bypass not closed for gemini: `{command}`"
        );
        assert!(json["reason"].as_str().unwrap().contains("3000"));
    }
}
