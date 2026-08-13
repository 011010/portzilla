//! End-to-end tests against the built `portzilla` binary.
//!
//! Each test points `PORTZILLA_DATA_DIR` at a fresh temporary directory so
//! runs never interfere with each other or with a real user's lease store.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn cmd(data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("portzilla").unwrap();
    cmd.env("PORTZILLA_DATA_DIR", data_dir);
    cmd
}

#[test]
fn claim_on_a_free_port_succeeds_and_reports_the_port() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "4000", "--tag", "web-server", "--pid", "111"])
        .assert()
        .success()
        .stdout(predicate::str::contains("4000"));
}

#[test]
fn claim_conflict_with_a_live_pid_reassigns_and_says_so() {
    let dir = tempfile::tempdir().unwrap();

    // Two distinct PIDs that are both guaranteed to be alive during this test:
    // this process, and its parent (the test harness process).
    let own_pid = std::process::id();
    let parent_pid = std::os::unix::process::parent_id();
    assert_ne!(own_pid, parent_pid);

    cmd(dir.path())
        .args([
            "claim",
            "4100",
            "--tag",
            "first",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    // A different, also-alive PID tries to claim the same port and must be
    // reassigned instead of stealing it.
    cmd(dir.path())
        .args([
            "claim",
            "4100",
            "--tag",
            "second",
            "--pid",
            &parent_pid.to_string(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("4101"));
}

#[test]
fn claim_json_output_is_valid_json_with_expected_fields() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["claim", "4200", "--tag", "api", "--pid", "222", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout should be valid JSON");
    assert_eq!(json["port"], 4200);
    assert_eq!(json["pid"], 222);
    assert_eq!(json["tag"], "api");
    assert_eq!(json["reassigned"], false);
}

#[test]
fn idempotent_reclaim_by_the_same_pid_does_not_duplicate_the_lease() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "4300", "--tag", "v1", "--pid", "333"])
        .assert()
        .success();
    cmd(dir.path())
        .args(["claim", "4300", "--tag", "v2", "--pid", "333"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let leases = json.as_array().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0]["tag"], "v2");
}

#[test]
fn ls_json_lists_all_claimed_leases() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "4400", "--tag", "one", "--pid", "444"])
        .assert()
        .success();
    cmd(dir.path())
        .args(["claim", "4401", "--tag", "two", "--pid", "445"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let leases = json.as_array().unwrap();
    assert_eq!(leases.len(), 2);
    let ports: Vec<u64> = leases.iter().map(|l| l["port"].as_u64().unwrap()).collect();
    assert!(ports.contains(&4400));
    assert!(ports.contains(&4401));
}

#[test]
fn ls_human_output_is_a_table_with_expected_columns() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "4500", "--tag", "human-check", "--pid", "555"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PORT"))
        .stdout(predicate::str::contains("PID"))
        .stdout(predicate::str::contains("TAG"))
        .stdout(predicate::str::contains("4500"))
        .stdout(predicate::str::contains("555"))
        .stdout(predicate::str::contains("human-check"));
}

#[test]
fn ls_with_no_leases_succeeds_with_empty_output() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .stdout("[]\n");
}

#[test]
fn claim_without_explicit_pid_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["claim", "4600", "--tag", "default-pid"])
        .assert()
        .success();
}

// ---- who ----

#[test]
fn who_human_shows_the_lease_for_a_claimed_port() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args([
            "claim",
            "4700",
            "--tag",
            "api",
            "--pid",
            "700",
            "--session",
            "sess-1",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["who", "4700"])
        .assert()
        .success()
        .stdout(predicate::str::contains("4700"))
        .stdout(predicate::str::contains("700"))
        .stdout(predicate::str::contains("api"))
        .stdout(predicate::str::contains("sess-1"));
}

#[test]
fn who_json_shows_the_same_flat_shape_as_ls_entries() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["claim", "4701", "--tag", "api", "--pid", "701"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["who", "4701", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout should be valid JSON");
    assert_eq!(json["port"], 4701);
    assert_eq!(json["pid"], 701);
    assert_eq!(json["tag"], "api");
    assert!(json.get("alive").is_some());
    assert!(json.get("age_secs").is_some());
}

#[test]
fn who_not_found_human_exits_nonzero_with_stderr_message_and_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["who", "4799"])
        .assert()
        .failure()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("4799"));
}

#[test]
fn who_not_found_json_exits_nonzero_with_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["who", "4799", "--json"])
        .assert()
        .failure()
        .code(2)
        .stdout("");
}

// ---- release ----

#[test]
fn release_with_a_dead_pid_succeeds_with_no_warning() {
    let dir = tempfile::tempdir().unwrap();
    // A PID this large will not correspond to a real running process.
    cmd(dir.path())
        .args(["claim", "4800", "--tag", "stale", "--pid", "4000000000"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["release", "4800"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // The lease is actually gone.
    cmd(dir.path())
        .args(["who", "4800"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn release_with_an_alive_pid_still_releases_but_warns_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();

    cmd(dir.path())
        .args([
            "claim",
            "4801",
            "--tag",
            "running",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["release", "4801"])
        .assert()
        .success()
        .stderr(predicate::str::contains("still alive"));

    cmd(dir.path())
        .args(["who", "4801"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn release_json_emits_the_released_lease_object() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["claim", "4802", "--tag", "svc", "--pid", "4000000001"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["release", "4802", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["port"], 4802);
    assert_eq!(json["tag"], "svc");
}

#[test]
fn release_not_found_behaves_like_who_not_found() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["release", "4899"])
        .assert()
        .failure()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("4899"));
}

// ---- prune ----

#[test]
fn prune_removes_only_dead_leases_and_reports_them() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();

    cmd(dir.path())
        .args([
            "claim",
            "4900",
            "--tag",
            "alive-one",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();
    cmd(dir.path())
        .args(["claim", "4901", "--tag", "dead-one", "--pid", "4000000002"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["prune"])
        .assert()
        .success()
        .stdout(predicate::str::contains("4901"))
        .stdout(predicate::str::contains("dead-one").and(predicate::str::contains("4900").not()));

    // The alive lease is untouched; the dead one is gone.
    cmd(dir.path()).args(["who", "4900"]).assert().success();
    cmd(dir.path())
        .args(["who", "4901"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn prune_json_reports_an_array_of_pruned_lease_objects() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["claim", "4902", "--tag", "dead-two", "--pid", "4000000003"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["prune", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let pruned = json.as_array().unwrap();
    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0]["port"], 4902);
}

// ---- security regressions found by adversarial review ----

#[test]
fn claim_conflict_onto_a_stale_leased_port_does_not_duplicate_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let parent_pid = std::os::unix::process::parent_id();
    let stale_pid = "4000000010";

    // 1. Claim port 6000 for a PID that is alive for the whole test (this process).
    cmd(dir.path())
        .args([
            "claim",
            "6000",
            "--tag",
            "live-owner",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    // 2. Claim port 6001 for a PID that is definitely dead (stale lease).
    cmd(dir.path())
        .args(["claim", "6001", "--tag", "stale-owner", "--pid", stale_pid])
        .assert()
        .success();

    // 3. A different, also-alive PID conflicts on 6000 and must be reassigned
    //    onto 6001 (the only free-looking port), reusing the stale lease slot.
    cmd(dir.path())
        .args([
            "claim",
            "6000",
            "--tag",
            "new-owner",
            "--pid",
            &parent_pid.to_string(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("6001"));

    // 4. `ls --json` must not contain two entries for port 6001.
    let output = cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let leases = json.as_array().unwrap();
    let port_6001_entries: Vec<&Value> = leases.iter().filter(|l| l["port"] == 6001).collect();
    assert_eq!(
        port_6001_entries.len(),
        1,
        "port 6001 must appear exactly once in ls"
    );

    // 5. `who 6001` must return the NEW owner, not the stale one.
    let who_output = cmd(dir.path())
        .args(["who", "6001", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let who_json: Value = serde_json::from_slice(&who_output).unwrap();
    assert_eq!(who_json["pid"], parent_pid);
    assert_eq!(who_json["tag"], "new-owner");

    // 6. `release 6001` must actually free it: a subsequent `who` must report not-found.
    cmd(dir.path()).args(["release", "6001"]).assert().success();
    cmd(dir.path())
        .args(["who", "6001"])
        .assert()
        .failure()
        .code(2);
}

// ---- control-character injection in human output ----

#[test]
fn ls_human_output_sanitizes_control_characters_in_the_tag() {
    let dir = tempfile::tempdir().unwrap();
    // Embeds a newline plus a payload shaped like a fake table row, attempting
    // to spoof an extra lease entry for a port that was never actually claimed.
    let malicious_tag = "evil\n9999    1        alive  0s         spoofed-row";

    cmd(dir.path())
        .args(["claim", "6100", "--tag", malicious_tag, "--pid", "600"])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    // Exactly one header line and one data row: the injected newline must not
    // have produced a second, spoofed row. The literal digits "9999" from the
    // payload are still allowed to show up as flattened tag text on the one
    // real row — what must not happen is a *separate line* starting with
    // "9999" as if it were its own table row for a port that was never claimed.
    assert_eq!(
        lines.len(),
        2,
        "expected header + exactly one lease row, got:\n{stdout}"
    );
    assert!(
        lines[1].starts_with("6100"),
        "the single data row must be for the real claimed port, got:\n{stdout}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("9999")),
        "no line may start with the spoofed port, got:\n{stdout}"
    );
}

#[test]
fn who_human_output_sanitizes_control_characters_in_the_tag() {
    let dir = tempfile::tempdir().unwrap();
    let malicious_tag = "evil\rcarriage-return";

    cmd(dir.path())
        .args(["claim", "6101", "--tag", malicious_tag, "--pid", "601"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["who", "6101"])
        .assert()
        .success()
        .stdout(predicate::str::contains('\r').not());
}

#[test]
fn prune_with_nothing_to_prune_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "4903",
            "--tag",
            "alive",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["prune", "--json"])
        .assert()
        .success()
        .stdout("[]\n");

    cmd(dir.path()).args(["prune"]).assert().success();
}

// ---- serve ----

#[test]
fn serve_without_mcp_flag_fails_with_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["serve"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--mcp").and(predicate::str::contains("serve requires")));
}

// ---- hook claude-code ----

fn pretooluse_bash_json(command: &str) -> String {
    pretooluse_bash_json_with_session(command, "abc123")
}

fn pretooluse_bash_json_with_session(command: &str, session_id: &str) -> String {
    serde_json::json!({
        "session_id": session_id,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_use_id": "toolu_01"
    })
    .to_string()
}

#[test]
fn hook_claude_code_denies_kill_of_a_foreign_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    // `own_pid` is only here so the lease's PID is a REAL, currently-alive
    // process (required to exercise the live-lease Deny path against the
    // real system PID checker) — it is deliberately NOT what makes this
    // "foreign". The adapter never passes a self_pid at all (PreToolUse
    // fires before the Bash process exists), so ownership through this
    // adapter is decided entirely by session: the claim below carries
    // session "other-session", while the hook payload declares
    // "my-session" — genuinely different sessions, not a same-PID coincidence.
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(pretooluse_bash_json_with_session(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = json["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("permissionDecisionReason must be a string");
    assert!(reason.contains("3000"));
    assert!(reason.contains("dev-server"));
}

#[test]
fn hook_claude_code_allows_kill_of_an_own_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    // Same session on both the claim and the hook payload: this is the
    // "restart my own dev server" case. Since the adapter never supplies a
    // self_pid, session is the ONLY mechanism that can allow this — this
    // test is the direct end-to-end regression guard for that fix.
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "shared-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(pretooluse_bash_json_with_session(
            &format!("kill {own_pid}"),
            "shared-session",
        ))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn hook_claude_code_allows_kill_of_an_unleased_pid() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(pretooluse_bash_json("kill 999999"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn hook_claude_code_allows_non_bash_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let input = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": {"file_path": "/tmp/x", "old_string": "kill 1", "new_string": ""}
    })
    .to_string();

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn hook_claude_code_malformed_json_fails_open_with_a_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();

    // Fail-open, end to end: garbage on stdin must never block or crash —
    // exit 0, nothing on stdout (so Claude Code's normal permission flow
    // applies unmodified), a diagnostic on stderr for troubleshooting.
    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin("{ not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("failing open"));
}

#[test]
fn hook_claude_code_warns_on_process_name_kill() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(pretooluse_bash_json("pkill node"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert!(
        json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("node")
    );
    assert!(json["systemMessage"].as_str().unwrap().contains("node"));
}

// ---- init claude-code ----

#[test]
fn init_claude_code_prints_the_settings_snippet() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["init", "claude-code"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"PreToolUse\""))
        .stdout(predicate::str::contains("\"matcher\": \"Bash\""))
        .stdout(predicate::str::contains("portzilla hook claude-code"));
}

// ---- hook cursor / gemini, init cursor / gemini ----

#[test]
fn hook_cursor_denies_kill_of_a_foreign_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    let input = serde_json::json!({
        "command": format!("kill {own_pid}"),
        "cwd": "/tmp",
        "sandbox": false,
        "conversation_id": "conv-unrelated"
    })
    .to_string();

    let output = cmd(dir.path())
        .args(["hook", "cursor"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["permission"], "deny");
    assert!(json["agent_message"].as_str().unwrap().contains("3000"));
}

#[test]
fn hook_gemini_denies_kill_of_a_foreign_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    let input = serde_json::json!({
        "session_id": "sess-unrelated",
        "hook_event_name": "BeforeTool",
        "tool_name": "run_shell_command",
        "tool_input": { "command": format!("kill {own_pid}") }
    })
    .to_string();

    let output = cmd(dir.path())
        .args(["hook", "gemini"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["decision"], "deny");
    assert!(json["reason"].as_str().unwrap().contains("3000"));
}

#[test]
fn init_cursor_prints_the_hooks_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "cursor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"beforeShellExecution\""))
        .stdout(predicate::str::contains("portzilla hook cursor"));
}

#[test]
fn init_gemini_prints_the_settings_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "gemini"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"BeforeTool\""))
        .stdout(predicate::str::contains("run_shell_command"))
        .stdout(predicate::str::contains("portzilla hook gemini"));
}

// ---- hook codex / kimi, init codex / kimi ----

fn codex_pretooluse_bash_json(command: &str, session_id: &str) -> String {
    serde_json::json!({
        "session_id": session_id,
        "cwd": "/tmp",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_use_id": "call_01",
        "permission_mode": "default"
    })
    .to_string()
}

#[test]
fn hook_codex_denies_kill_of_a_foreign_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["hook", "codex"])
        .write_stdin(codex_pretooluse_bash_json(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = json["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("permissionDecisionReason must be a string");
    assert!(reason.contains("3000"));
    assert!(reason.contains("dev-server"));
}

#[test]
fn hook_codex_allows_kill_of_an_own_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "shared-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "codex"])
        .write_stdin(codex_pretooluse_bash_json(
            &format!("kill {own_pid}"),
            "shared-session",
        ))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn hook_codex_allows_kill_of_an_unleased_pid() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["hook", "codex"])
        .write_stdin(codex_pretooluse_bash_json("kill 999999", "abc123"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn hook_codex_malformed_json_fails_open_with_a_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["hook", "codex"])
        .write_stdin("{ not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("failing open"));
}

#[test]
fn hook_codex_warns_on_process_name_kill() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "codex"])
        .write_stdin(codex_pretooluse_bash_json("pkill node", "abc123"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert!(
        json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("node")
    );
    assert!(json["systemMessage"].as_str().unwrap().contains("node"));
}

#[test]
fn init_codex_prints_the_hooks_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"PreToolUse\""))
        .stdout(predicate::str::contains("\"matcher\": \"Bash\""))
        .stdout(predicate::str::contains("portzilla hook codex"));
}

fn kimi_pretooluse_shell_json(command: &str, session_id: &str) -> String {
    serde_json::json!({
        "session_id": session_id,
        "cwd": "/tmp",
        "hook_event_name": "PreToolUse",
        "tool_name": "Shell",
        "tool_input": { "command": command },
        "tool_call_id": "call_01"
    })
    .to_string()
}

#[test]
fn hook_kimi_denies_kill_of_a_foreign_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin(kimi_pretooluse_shell_json(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .code(2) // Kimi's block exit code
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("3000").and(predicate::str::contains("dev-server")));
}

#[test]
fn hook_kimi_allows_kill_of_an_own_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "shared-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin(kimi_pretooluse_shell_json(
            &format!("kill {own_pid}"),
            "shared-session",
        ))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn hook_kimi_allows_kill_of_an_unleased_pid() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin(kimi_pretooluse_shell_json("kill 999999", "abc123"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn hook_kimi_malformed_json_fails_open_with_a_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin("{ not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("failing open"));
}

#[test]
fn hook_kimi_warns_on_process_name_kill_with_plain_text_stdout() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin(kimi_pretooluse_shell_json("pkill node", "abc123"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout must be UTF-8");
    assert!(
        stdout.contains("node"),
        "warn must reach the model as plain-text stdout, got: {stdout}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_err(),
        "warn output must be plain text, not parseable JSON"
    );
}

#[test]
fn init_kimi_prints_the_config_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "kimi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[[hooks]]"))
        .stdout(predicate::str::contains(r#"event = "PreToolUse""#))
        .stdout(predicate::str::contains(r#"matcher = "Shell""#))
        .stdout(predicate::str::contains("portzilla hook kimi"));
}

// ---- hook opencode / init opencode ----

fn opencode_shim_json(command: &str, session_id: &str) -> String {
    serde_json::json!({
        "session_id": session_id,
        "command": command
    })
    .to_string()
}

#[test]
fn hook_opencode_denies_kill_of_a_foreign_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["hook", "opencode"])
        .write_stdin(opencode_shim_json(&format!("kill {own_pid}"), "my-session"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["action"], "deny");
    let reason = json["reason"].as_str().expect("reason must be a string");
    assert!(reason.contains("3000"));
    assert!(reason.contains("dev-server"));
}

#[test]
fn hook_opencode_allows_kill_of_an_own_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "shared-session",
        ])
        .assert()
        .success();

    let output = cmd(dir.path())
        .args(["hook", "opencode"])
        .write_stdin(opencode_shim_json(
            &format!("kill {own_pid}"),
            "shared-session",
        ))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["action"], "allow", "own-session kill must be allowed");
}

#[test]
fn hook_opencode_allows_kill_of_an_unleased_pid() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "opencode"])
        .write_stdin(opencode_shim_json("kill 999999", "abc123"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["action"], "allow");
}

#[test]
fn hook_opencode_malformed_json_fails_open_with_allow_action() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "opencode"])
        .write_stdin("{ not valid json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["action"], "allow", "malformed input must fail open");
}

#[test]
fn hook_opencode_warns_on_process_name_kill() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["hook", "opencode"])
        .write_stdin(opencode_shim_json("pkill node", "abc123"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["action"], "warn");
    assert!(json["reason"].as_str().unwrap().contains("node"));
}

#[test]
fn init_opencode_prints_the_plugin_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "opencode"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tool.execute.before"))
        .stdout(predicate::str::contains("tool.execute.after"))
        .stdout(predicate::str::contains("shell.env"))
        .stdout(predicate::str::contains("portzilla hook opencode"))
        .stdout(predicate::str::contains("PORTZILLA_SESSION"));
}

// ---- hook windsurf / init windsurf ----

fn windsurf_pre_run_command_json(command: &str, trajectory_id: &str) -> String {
    serde_json::json!({
        "agent_action_name": "pre_run_command",
        "trajectory_id": trajectory_id,
        "execution_id": "exec_01",
        "timestamp": "2026-01-01T00:00:00Z",
        "tool_info": {
            "command_line": command,
            "cwd": "/tmp"
        }
    })
    .to_string()
}

#[test]
fn hook_windsurf_denies_kill_of_a_foreign_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "other-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(windsurf_pre_run_command_json(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .code(2) // Windsurf's block exit code
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("3000").and(predicate::str::contains("dev-server")));
}

#[test]
fn hook_windsurf_allows_kill_of_an_own_session_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "shared-session",
        ])
        .assert()
        .success();

    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(windsurf_pre_run_command_json(
            &format!("kill {own_pid}"),
            "shared-session",
        ))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn hook_windsurf_allows_kill_of_an_unleased_pid() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(windsurf_pre_run_command_json("kill 999999", "abc123"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn hook_windsurf_malformed_json_fails_open_with_exit_0_and_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin("{ not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("failing open"));
}

#[test]
fn hook_windsurf_warns_on_process_name_kill_exit_0_with_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Windsurf has no model-visible warn channel: the warning rides stderr
    // on exit 0 (visible to a human when `show_output` is on), and it never
    // blocks — that is the documented tradeoff for this harness.
    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(windsurf_pre_run_command_json("pkill node", "abc123"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("node"));
}

#[test]
fn hook_windsurf_missing_command_fails_open_with_exit_0_and_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let payload = serde_json::json!({
        "agent_action_name": "pre_run_command",
        "trajectory_id": "abc123",
        "tool_info": { "cwd": "/tmp" }
    })
    .to_string();

    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(payload)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("command_line"));
}

#[test]
fn init_windsurf_prints_the_hooks_json_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "windsurf"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pre_run_command"))
        .stdout(predicate::str::contains("portzilla hook windsurf"))
        .stdout(predicate::str::contains(".windsurf/hooks.json"));
}

// ---- guard ----
//
// These tests spawn the real binary with `guard -- <fake script>` instead of
// a real `kill`, so a bug that lets a Deny slip through can't actually
// terminate the test process: the "kill"/"pkill" named scripts are
// test-controlled no-ops that just touch a marker file, never real signals.

#[cfg(unix)]
fn write_fake_command(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
#[test]
fn guard_deny_blocks_execution_no_side_effect_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    // Named "kill" so guard's basename-based verb matching recognizes it —
    // but it is our own harmless script, not the real system `kill`.
    let script = dir.path().join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    cmd(dir.path())
        .args([
            "guard",
            "--",
            script.to_str().unwrap(),
            &own_pid.to_string(),
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("3000").and(predicate::str::contains("dev-server")));

    assert!(
        !dir.path().join("marker").exists(),
        "a denied command must never have run"
    );
}

#[cfg(unix)]
#[test]
fn guard_allow_executes_and_propagates_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("myapp");
    write_fake_command(
        &script,
        &format!("touch '{}/marker'\nexit 7", dir.path().display()),
    );

    cmd(dir.path())
        .args(["guard", "--", script.to_str().unwrap()])
        .assert()
        .code(7);

    assert!(
        dir.path().join("marker").exists(),
        "an allowed command must have run"
    );
}

#[cfg(unix)]
#[test]
fn guard_warn_executes_with_stderr_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Named "pkill" so guard resolves it to a Warn (unresolvable process
    // name), not a Deny.
    let script = dir.path().join("pkill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    cmd(dir.path())
        .args(["guard", "--", script.to_str().unwrap(), "node"])
        .assert()
        .success()
        .stderr(predicate::str::contains("node"));

    assert!(
        dir.path().join("marker").exists(),
        "a warned command must still execute"
    );
}

#[cfg(unix)]
#[test]
fn guard_analyzes_the_sh_c_payload_and_denies() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    let script = dir.path().join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    // The whole point of the `sh -c` unwrap: the pipe-shaped payload is one
    // pre-quoted argv element, exactly as a real invocation would produce.
    let payload = format!("{} {} | true", script.display(), own_pid);
    cmd(dir.path())
        .args(["guard", "--", "sh", "-c", &payload])
        .assert()
        .failure()
        .code(2);

    assert!(!dir.path().join("marker").exists());
}

// ---- CRITICAL fix regression: generalized shell -c unwrap ----
//
// Each of these previously bypassed detection entirely (the old unwrap only
// matched the exact 3-argv `[shell, "-c", payload]` shape) and would have
// let the fake "kill" script run for real, denying only by accident of test
// design rather than by the guard actually recognizing the command.

#[cfg(unix)]
fn setup_foreign_live_lease_and_kill_script(dir: &std::path::Path) -> (u32, std::path::PathBuf) {
    let own_pid = std::process::id();
    cmd(dir)
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    let script = dir.join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.display()));
    (own_pid, script)
}

#[cfg(unix)]
#[test]
fn guard_denies_via_combined_dash_lc_flag() {
    let dir = tempfile::tempdir().unwrap();
    let (own_pid, script) = setup_foreign_live_lease_and_kill_script(dir.path());
    let payload = format!("{} {}", script.display(), own_pid);

    cmd(dir.path())
        .args(["guard", "--", "sh", "-lc", &payload])
        .assert()
        .failure()
        .code(2);

    assert!(
        !dir.path().join("marker").exists(),
        "sh -lc must not bypass the guard"
    );
}

#[cfg(unix)]
#[test]
fn guard_denies_via_separate_dash_x_dash_c_flags() {
    let dir = tempfile::tempdir().unwrap();
    let (own_pid, script) = setup_foreign_live_lease_and_kill_script(dir.path());
    let payload = format!("{} {}", script.display(), own_pid);

    cmd(dir.path())
        .args(["guard", "--", "sh", "-x", "-c", &payload])
        .assert()
        .failure()
        .code(2);

    assert!(
        !dir.path().join("marker").exists(),
        "sh -x -c must not bypass the guard"
    );
}

#[cfg(unix)]
#[test]
fn guard_denies_via_long_flag_then_dash_c() {
    let dir = tempfile::tempdir().unwrap();
    let (own_pid, script) = setup_foreign_live_lease_and_kill_script(dir.path());
    let payload = format!("{} {}", script.display(), own_pid);

    cmd(dir.path())
        .args(["guard", "--", "bash", "--norc", "-c", &payload])
        .assert()
        .failure()
        .code(2);

    assert!(
        !dir.path().join("marker").exists(),
        "bash --norc -c must not bypass the guard"
    );
}

#[cfg(unix)]
#[test]
fn guard_denies_via_nested_sh_c() {
    let dir = tempfile::tempdir().unwrap();
    let (own_pid, script) = setup_foreign_live_lease_and_kill_script(dir.path());
    let inner = format!("{} {}", script.display(), own_pid);
    let payload = format!("sh -c '{inner}'");

    cmd(dir.path())
        .args(["guard", "--", "sh", "-c", &payload])
        .assert()
        .failure()
        .code(2);

    assert!(
        !dir.path().join("marker").exists(),
        "nested sh -c must not bypass the guard"
    );
}

#[cfg(unix)]
#[test]
fn guard_sh_c_with_non_kill_payload_still_executes() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");

    cmd(dir.path())
        .args([
            "guard",
            "--",
            "sh",
            "-c",
            &format!("touch '{}'", marker.display()),
        ])
        .assert()
        .success();

    assert!(marker.exists(), "a non-kill sh -c payload must still run");
}

#[cfg(unix)]
#[test]
fn guard_own_session_lease_executes() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "my-sess",
        ])
        .assert()
        .success();

    let script = dir.path().join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    cmd(dir.path())
        .args([
            "guard",
            "--session",
            "my-sess",
            "--",
            script.to_str().unwrap(),
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    assert!(dir.path().join("marker").exists());
}

#[cfg(unix)]
#[test]
fn guard_session_from_env_var_executes_own_session_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    cmd(dir.path())
        .args([
            "claim",
            "3000",
            "--tag",
            "dev-server",
            "--pid",
            &own_pid.to_string(),
            "--session",
            "env-sess",
        ])
        .assert()
        .success();

    let script = dir.path().join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    cmd(dir.path())
        .env("PORTZILLA_SESSION", "env-sess")
        .args([
            "guard",
            "--",
            script.to_str().unwrap(),
            &own_pid.to_string(),
        ])
        .assert()
        .success();

    assert!(dir.path().join("marker").exists());
}

// ---- WARNING 1 fix regression: POSIX exec-failure exit codes ----

#[cfg(unix)]
#[test]
fn guard_allowed_command_not_found_exits_127() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["guard", "--", "/nonexistent/portzilla-test-binary-xyz"])
        .assert()
        .code(127);
}

#[cfg(unix)]
#[test]
fn guard_allowed_non_executable_file_exits_126() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-executable");
    std::fs::write(&file, "just a text file, no shebang, no +x bit").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    cmd(dir.path())
        .args(["guard", "--", file.to_str().unwrap()])
        .assert()
        .code(126);
}
