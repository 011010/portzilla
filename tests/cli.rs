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
        .args(["claim", "4100", "--tag", "first", "--pid", &own_pid.to_string()])
        .assert()
        .success();

    // A different, also-alive PID tries to claim the same port and must be
    // reassigned instead of stealing it.
    cmd(dir.path())
        .args(["claim", "4100", "--tag", "second", "--pid", &parent_pid.to_string()])
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

    cmd(dir.path()).args(["ls", "--json"]).assert().success().stdout("[]\n");
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
        .args(["claim", "4700", "--tag", "api", "--pid", "700", "--session", "sess-1"])
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
    cmd(dir.path()).args(["who", "4800"]).assert().failure().code(2);
}

#[test]
fn release_with_an_alive_pid_still_releases_but_warns_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();

    cmd(dir.path())
        .args(["claim", "4801", "--tag", "running", "--pid", &own_pid.to_string()])
        .assert()
        .success();

    cmd(dir.path())
        .args(["release", "4801"])
        .assert()
        .success()
        .stderr(predicate::str::contains("still alive"));

    cmd(dir.path()).args(["who", "4801"]).assert().failure().code(2);
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
        .args(["claim", "4900", "--tag", "alive-one", "--pid", &own_pid.to_string()])
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
    cmd(dir.path()).args(["who", "4901"]).assert().failure().code(2);
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
        .args(["claim", "6000", "--tag", "live-owner", "--pid", &own_pid.to_string()])
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
        .args(["claim", "6000", "--tag", "new-owner", "--pid", &parent_pid.to_string()])
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
    assert_eq!(port_6001_entries.len(), 1, "port 6001 must appear exactly once in ls");

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
    cmd(dir.path()).args(["who", "6001"]).assert().failure().code(2);
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
    assert_eq!(lines.len(), 2, "expected header + exactly one lease row, got:\n{stdout}");
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
        .args(["claim", "4903", "--tag", "alive", "--pid", &own_pid.to_string()])
        .assert()
        .success();

    cmd(dir.path()).args(["prune", "--json"]).assert().success().stdout("[]\n");

    cmd(dir.path()).args(["prune"]).assert().success();
}
