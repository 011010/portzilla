//! End-to-end tests against the built `portzilla` binary.
//!
//! Each test points `PORTZILLA_DATA_DIR` at a fresh temporary directory so
//! runs never interfere with each other or with a real user's lease store.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
#[cfg(unix)]
use std::process::{Child, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(unix)]
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
#[cfg(unix)]
use std::time::{Duration, Instant};

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
fn history_json_on_empty_store_returns_stable_envelope() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["history", "--json"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(r#"\{"events":\[\],"has_more":false,"next_before":null\}"#)
                .unwrap(),
        );
}

#[test]
fn history_json_returns_events_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["claim", "4001", "--tag", "history", "--pid", "111"])
        .assert()
        .success();
    let output = cmd(dir.path())
        .args(["history", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["events"][0]["event"], "lease_claimed");
    assert_eq!(json["events"][0]["sequence"], 1);
}

#[test]
fn history_clear_replaces_prior_events_with_clear_marker() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["claim", "4002", "--tag", "history", "--pid", "111"])
        .assert()
        .success();
    cmd(dir.path())
        .args(["history", "clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared 1 history events"));
    let output = cmd(dir.path())
        .args(["history", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["events"][0]["event"], "history_cleared");
    assert_eq!(json["events"][0]["data"]["removed_count"], 1);
}

#[test]
fn history_clear_rejects_query_options() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["history", "clear", "--limit", "2"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot be used").or(predicate::str::contains("conflicts")),
        );
}

/// Dedicated, non-ephemeral range for CLI tests.
///
/// `bind(("127.0.0.1", 0))` hands the port back to the shared pool as soon as
/// the listener drops, so a parallel test can take the very port this file is
/// about to assert on. The range below is outside every platform's ephemeral
/// range and disjoint from the unit-test range in `src/store.rs`, so the two
/// test binaries stay out of each other's way even when a runner executes them
/// concurrently.
const TEST_PORT_BASE: u16 = 30_500;
const TEST_PORT_SPAN: u32 = 1_400;
/// Ports advance in pairs so a single-port handout never collides with the
/// successor another handout reserved for an adjacent-pair test.
const TEST_PORT_STRIDE: u32 = 2;

static NEXT_TEST_PORT: AtomicU32 = AtomicU32::new(0);

static TEST_PORT_LOCK: Mutex<()> = Mutex::new(());

/// Serializes the tests that need a port to stay free for their whole body.
///
/// A unique handout is not enough by itself: `portzilla run` reassigns to the
/// next free port on conflict and binds it, so a walk from one test's port can
/// land on another test's reserved neighbour. Holding this for the body of
/// every port-sensitive test makes that impossible. Poisoning is ignored so one
/// failing test cannot cascade into unrelated failures.
fn lock_test_ports() -> MutexGuard<'static, ()> {
    TEST_PORT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Yields ports in the test range that no other handout in this process can
/// return, so uniqueness does not depend on the OS allocator.
fn next_test_port() -> u16 {
    let offset = NEXT_TEST_PORT.fetch_add(TEST_PORT_STRIDE, Ordering::Relaxed);
    assert!(
        offset < TEST_PORT_SPAN,
        "CLI test port pool exhausted: more than {} handouts in one run",
        TEST_PORT_SPAN / TEST_PORT_STRIDE
    );
    TEST_PORT_BASE + offset as u16
}

fn reserve_adjacent_ports() -> (TcpListener, TcpListener, u16) {
    for _ in 0..TEST_PORT_SPAN {
        let base = next_test_port();
        let Some(successor_port) = base.checked_add(1) else {
            continue;
        };
        let (Ok(requested), Ok(successor)) = (
            TcpListener::bind(("127.0.0.1", base)),
            TcpListener::bind(("127.0.0.1", successor_port)),
        ) else {
            continue;
        };
        return (requested, successor, base);
    }
    panic!("could not reserve adjacent test ports");
}

/// Mirrors the store's `is_os_port_free` probe: wildcard + loopback in
/// both families, with unbindable IPv6 treated as unprobeable rather than
/// occupied. `reserve_adjacent_ports` only checks 127.0.0.1, which is not
/// enough when a test asserts where the conflict walk lands: on macOS CI
/// runners the walk also skips ports occupied on `0.0.0.0`/`::`/`::1`, so
/// a merely-IPv4-free "next" port gets passed over.
fn is_os_free_like_portzilla(port: u16) -> bool {
    ["0.0.0.0", "127.0.0.1"]
        .into_iter()
        .all(|address| TcpListener::bind((address, port)).is_ok())
        && ["::", "::1"]
            .into_iter()
            .all(|address| match TcpListener::bind((address, port)) {
                Ok(_) => true,
                Err(error) => matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ),
            })
}

fn free_test_port() -> u16 {
    for _ in 0..TEST_PORT_SPAN {
        let port = next_test_port();
        if is_os_free_like_portzilla(port) {
            return port;
        }
    }
    panic!("could not find a free test port");
}

fn claim_live_lease(data_dir: &std::path::Path, pid: u32, session: Option<&str>) -> u16 {
    let port = free_test_port();
    let port_arg = port.to_string();
    let pid_arg = pid.to_string();
    let mut command = cmd(data_dir);
    command.args(["claim", &port_arg, "--tag", "dev-server", "--pid", &pid_arg]);
    if let Some(session) = session {
        command.args(["--session", session]);
    }
    let output = command
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    json["port"].as_u64().unwrap() as u16
}

#[cfg(unix)]
fn terminate_and_reap(child: &mut Child) -> std::process::ExitStatus {
    const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

    let poll_until = |child: &mut Child, deadline: Instant| loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    if let Some(status) = poll_until(child, Instant::now() + EXIT_TIMEOUT) {
        return status;
    }

    let sigkill_sent = match child.kill() {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("failed to terminate watch process: {error}"),
    };

    if let Some(status) = poll_until(child, Instant::now() + EXIT_TIMEOUT) {
        return status;
    }

    // A successful Child::kill sends SIGKILL on Unix. It cannot be caught or
    // ignored, so this final reap cannot leave the child running.
    if sigkill_sent {
        return child.wait().unwrap();
    }

    // NotFound means the process exited between the last poll and kill; wait
    // still reaps its already-terminated child entry.
    child.wait().unwrap()
}

#[cfg(unix)]
fn run_watch_once(data_dir: &std::path::Path, json: bool) -> Output {
    let binary = std::env::var_os("CARGO_BIN_EXE_portzilla").unwrap();
    let mut command = std::process::Command::new(binary);
    command
        .env("PORTZILLA_DATA_DIR", data_dir)
        .args(["watch", "--interval", "60"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if json {
        command.arg("--json");
    }

    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (output_tx, output_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).unwrap();
        ready_tx.send(()).unwrap();
        let mut remaining = String::new();
        reader.read_to_string(&mut remaining).unwrap();
        output_tx.send(first_line + &remaining).unwrap();
    });
    if ready_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        terminate_and_reap(&mut child);
        panic!("watch process did not emit its first cycle before timeout");
    }
    let signal_result = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    if !signal_result
        .as_ref()
        .is_ok_and(std::process::ExitStatus::success)
        && child.try_wait().unwrap().is_none()
    {
        let detail = match signal_result {
            Ok(status) => format!("kill -INT exited with {status}"),
            Err(error) => format!("could not run kill -INT: {error}"),
        };
        terminate_and_reap(&mut child);
        panic!("failed to send SIGINT to watch process: {detail}");
    }
    let status = terminate_and_reap(&mut child);
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    let stdout = output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    Output {
        status,
        stdout: stdout.into_bytes(),
        stderr,
    }
}

#[test]
#[cfg(unix)]
fn watch_human_mode_reports_pruned_lease_details() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    claim_live_lease(dir.path(), u32::MAX, None);

    let output = run_watch_once(dir.path(), false);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("pruned port"));
    assert!(stdout.contains("pid"));
    assert!(stdout.contains("dev-server"));
}

#[test]
#[cfg(unix)]
fn watch_json_mode_keeps_stdout_machine_readable() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    claim_live_lease(dir.path(), u32::MAX, None);

    let output = run_watch_once(dir.path(), true);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let event: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(event["event"], "watch_cycle");
    assert_eq!(event["pruned"][0]["tag"], "dev-server");
}

#[test]
#[cfg(unix)]
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
        .stdout(predicate::str::contains("4101"))
        .stdout(predicate::str::contains("lease conflict"));
}

#[test]
#[cfg(unix)]
fn claim_json_reports_lease_conflict_reason_when_reassigned() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let parent_pid = std::os::unix::process::parent_id();

    cmd(dir.path())
        .args([
            "claim",
            "4150",
            "--tag",
            "first",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();
    let output = cmd(dir.path())
        .args([
            "claim",
            "4150",
            "--tag",
            "second",
            "--pid",
            &parent_pid.to_string(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["reassigned"], true);
    assert_eq!(json["reassignment_reason"], "lease_conflict");
}

#[test]
fn claim_json_reports_os_occupied_reason_for_an_unregistered_bound_port() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let (_listener, successor, base) = reserve_adjacent_ports();

    let output = cmd(dir.path())
        .args([
            "claim",
            &base.to_string(),
            "--tag",
            "second",
            "--pid",
            "222",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        json["port"].as_u64().unwrap() > u64::from(base),
        "occupied port must be reassigned forward"
    );
    assert_eq!(json["reassigned"], true);
    assert_eq!(json["reassignment_reason"], "os_occupied");
    drop(successor);
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
fn claim_and_ls_json_persist_process_start_time_for_the_real_pid() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();

    let claim_output = cmd(dir.path())
        .args([
            "claim",
            "4250",
            "--tag",
            "api",
            "--pid",
            &own_pid.to_string(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let claim_json: Value = serde_json::from_slice(&claim_output).unwrap();
    let start_time = claim_json["process_start_time"]
        .as_u64()
        .expect("claim JSON should include process_start_time");

    let ls_output = cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let leases: Value = serde_json::from_slice(&ls_output).unwrap();
    assert_eq!(leases[0]["pid"], own_pid);
    assert_eq!(leases[0]["process_start_time"], start_time);
}

#[test]
fn idempotent_reclaim_by_the_same_pid_does_not_duplicate_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();

    cmd(dir.path())
        .args([
            "claim",
            "4300",
            "--tag",
            "v1",
            "--pid",
            &own_pid.to_string(),
        ])
        .assert()
        .success();
    cmd(dir.path())
        .args([
            "claim",
            "4300",
            "--tag",
            "v2",
            "--pid",
            &own_pid.to_string(),
        ])
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
#[cfg(unix)]
fn claim_conflict_onto_a_stale_leased_port_does_not_duplicate_the_lease() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let parent_pid = std::os::unix::process::parent_id();
    let stale_pid = "4000000010";

    // The ports this test ends up using come from the CLI, not from an
    // assumption about the OS: on a busy machine a requested port can be taken
    // between the probe and the claim, so asserting an exact "next port" makes
    // the test depend on machine load rather than on the contract being tested.
    let claim_json = |port: u16, tag: &str, pid: &str| -> Value {
        let output = cmd(dir.path())
            .args([
                "claim",
                &port.to_string(),
                "--tag",
                tag,
                "--pid",
                pid,
                "--json",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };

    // 1. A live owner holds some port.
    let live = claim_json(free_test_port(), "live-owner", &own_pid.to_string());
    let live_port = live["port"].as_u64().unwrap() as u16;

    // 2. A stale (dead) lease holds a different port.
    let stale = claim_json(free_test_port(), "stale-owner", stale_pid);
    let stale_port = stale["port"].as_u64().unwrap() as u16;
    assert_ne!(
        live_port, stale_port,
        "the two claims must not land on the same port"
    );

    // 3. Another live owner conflicts on the first port and must be reassigned.
    let reassigned = claim_json(live_port, "new-owner", &parent_pid.to_string());
    assert_eq!(
        reassigned["reassigned"], true,
        "a conflict with a live lease must reassign: {reassigned}"
    );
    assert_eq!(reassigned["reassignment_reason"], "lease_conflict");
    assert_ne!(
        reassigned["port"].as_u64().unwrap() as u16,
        live_port,
        "the reassigned claim must not keep the contested port"
    );

    // 4. Whatever port it landed on, no port may hold two leases and each
    //    claim must appear exactly once — the duplicate the transfer path used
    //    to create when it reused a stale slot.
    let output = cmd(dir.path())
        .args(["ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let leases = json.as_array().unwrap();
    let mut ports: Vec<u64> = leases
        .iter()
        .map(|lease| lease["port"].as_u64().unwrap())
        .collect();
    let total = ports.len();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(
        ports.len(),
        total,
        "no port may hold two leases at once: {json}"
    );
    for tag in ["live-owner", "new-owner"] {
        assert_eq!(
            leases.iter().filter(|lease| lease["tag"] == tag).count(),
            1,
            "exactly one {tag} lease expected: {json}"
        );
    }
    // The stale lease survives only when the reassignment landed elsewhere. It
    // is legitimately replaced when the walk reuses its slot, which is the
    // behaviour this test guards — the point is that it is never duplicated.
    assert!(
        leases
            .iter()
            .filter(|lease| lease["tag"] == "stale-owner")
            .count()
            <= 1,
        "at most one stale-owner lease expected: {json}"
    );
}

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
    let _ports = lock_test_ports();
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
    let port = claim_live_lease(dir.path(), own_pid, Some("other-session"));

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
    assert!(reason.contains(&port.to_string()));
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
fn hook_claude_code_allows_unguarded_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let input = serde_json::json!({
        "session_id": "my-session",
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "tool_input": {"file_path": "/tmp/x"}
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
fn hook_claude_code_claims_a_free_file_for_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let input = serde_json::json!({
        "session_id": "my-session",
        "hook_event_name": "PreToolUse",
        "cwd": project.path(),
        "tool_name": "Write",
        "tool_input": {"file_path": "notes.md", "content": "hello\n"}
    })
    .to_string();

    cmd(dir.path())
        .args(["hook", "claude-code"])
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());

    // The end-to-end wiring opened the intent store next to the lease store
    // and recorded the claim with a location.
    let intents = std::fs::read_to_string(dir.path().join("intents.json")).unwrap();
    let json: Value = serde_json::from_str(&intents).unwrap();
    assert_eq!(json["intents"][0]["session"], "my-session");
    assert!(
        json["intents"][0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("notes.md")
    );
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
    // PreToolUse has no additionalContext in the verified contract; the user is
    // warned through systemMessage, and the model is not warned at all.
    assert!(
        json["hookSpecificOutput"]
            .get("additionalContext")
            .is_none(),
        "PreToolUse must not carry additionalContext: {json}"
    );
    assert!(json["systemMessage"].as_str().unwrap().contains("node"));
}

// ---- init claude-code ----

#[test]
fn init_claude_code_prints_the_settings_snippet() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["init", "claude-code"])
        .output()
        .expect("init claude-code must run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let snippet: Value =
        serde_json::from_str(&stdout).expect("init must print one JSON settings document");

    let pre = &snippet["hooks"]["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash|Write|Edit");
    assert_eq!(pre["hooks"][0]["type"], "command");
    assert_eq!(pre["hooks"][0]["command"], "portzilla hook claude-code");

    let post = &snippet["hooks"]["PostToolUse"][0];
    assert_eq!(post["matcher"], "Write|Edit");
    assert_eq!(post["hooks"][0]["type"], "command");
    assert_eq!(post["hooks"][0]["command"], "portzilla hook claude-code");

    // The prose must cover the migration and the silent-clean rule.
    let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    assert!(
        stderr.contains("Bash only"),
        "the prose must say an old Bash-only registration needs updating: {stderr}"
    );
    assert!(
        stderr.contains("permission prompt"),
        "the prose must say a clean edit does not skip the permission prompt: {stderr}"
    );
}

// ---- hook cursor / gemini, init cursor / gemini ----

#[test]
fn hook_cursor_denies_kill_of_a_foreign_live_lease() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, None);

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
    assert!(
        json["agent_message"]
            .as_str()
            .unwrap()
            .contains(&port.to_string())
    );
}

#[test]
fn hook_gemini_denies_kill_of_a_foreign_live_lease() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, None);

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
    assert!(json["reason"].as_str().unwrap().contains(&port.to_string()));
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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, Some("other-session"));

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
    assert!(reason.contains(&port.to_string()));
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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, Some("other-session"));

    cmd(dir.path())
        .args(["hook", "kimi"])
        .write_stdin(kimi_pretooluse_shell_json(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .code(2) // Kimi's block exit code
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains(port.to_string()).and(predicate::str::contains("dev-server")),
        );
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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, Some("other-session"));

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
    assert!(reason.contains(&port.to_string()));
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
fn hook_opencode_denies_an_edit_of_a_file_held_by_another_session() {
    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let file_input = |session: &str| {
        serde_json::json!({
            "session_id": session,
            "file": "src/auth.rs",
            "cwd": project.path().to_string_lossy(),
        })
        .to_string()
    };

    let ask = |session: &str| {
        let output = cmd(dir.path())
            .args(["hook", "opencode"])
            .write_stdin(file_input(session))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<Value>(&output).expect("stdout must be valid JSON")
    };

    // The first session claims the file by editing it.
    assert_eq!(ask("session-a")["action"], "allow");

    // A second session is denied, and the reason names the file and the holder.
    let denied = ask("session-b");
    assert_eq!(denied["action"], "deny");
    let reason = denied["reason"].as_str().expect("reason must be a string");
    assert!(
        reason.contains("src/auth.rs"),
        "unexpected reason: {reason}"
    );
    assert!(reason.contains("session-a"), "unexpected reason: {reason}");

    // The holding session may keep editing.
    assert_eq!(ask("session-a")["action"], "allow");

    // The intent store lives under PORTZILLA_DATA_DIR, like the lease store,
    // and the denied session never got a record.
    let intents =
        std::fs::read_to_string(dir.path().join("intents.json")).expect("intents.json must exist");
    assert!(intents.contains("session-a"));
    assert!(!intents.contains("session-b"));
}

#[test]
fn init_opencode_prints_the_plugin_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "opencode"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"id: "portzilla""#))
        .stdout(predicate::str::contains("async setup(ctx)"))
        .stdout(predicate::str::contains("execute.before"))
        .stdout(predicate::str::contains("portzilla hook opencode"))
        .stdout(predicate::str::contains("Bun.spawn"));
}

// The stdout/stderr contract (issue #28): stdout is exactly what a user saves
// or pastes verbatim; instructions and caveats go to stderr. A redirect such
// as `portzilla init opencode > .opencode/plugin/portzilla.js` must produce a
// file that loads, which the prose mixed into stdout used to break.
#[test]
fn init_opencode_stdout_is_only_the_plugin() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "opencode"])
        .assert()
        .success()
        // The snippet, whole and alone on stdout.
        .stdout(predicate::str::starts_with(
            "// portzilla guard for OpenCode",
        ))
        .stdout(predicate::str::ends_with("};\n"))
        .stdout(predicate::str::contains("export default {"))
        // Every prose marker must be gone from stdout.
        .stdout(predicate::str::contains("How it works:").not())
        .stdout(predicate::str::contains("Save the file below as").not())
        .stdout(predicate::str::contains("Fail-open:").not())
        // And must have moved to stderr instead of disappearing.
        .stderr(predicate::str::contains("Save the file below as"))
        .stderr(predicate::str::contains("How it works:"));
}

#[test]
fn init_claude_code_stdout_is_only_the_settings_snippet() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "claude-code"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("{\n"))
        .stdout(predicate::str::contains("How it works:").not())
        .stdout(predicate::str::contains("Add the following").not())
        .stderr(predicate::str::contains("Add the following"));
}

#[test]
fn init_skill_stdout_is_only_the_skill_file() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["init", "skill"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("---"))
        // `init skill` always printed only the file body; this pins that the
        // contract holds for the subcommand the docs redirect directly.
        .stdout(predicate::str::contains("Add the following").not());
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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, Some("other-session"));

    cmd(dir.path())
        .args(["hook", "windsurf"])
        .write_stdin(windsurf_pre_run_command_json(
            &format!("kill {own_pid}"),
            "my-session",
        ))
        .assert()
        .code(2) // Windsurf's block exit code
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains(port.to_string()).and(predicate::str::contains("dev-server")),
        );
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
        .stderr(predicate::str::contains(".windsurf/hooks.json"));
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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let port = claim_live_lease(dir.path(), own_pid, None);

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
        .stderr(
            predicate::str::contains(port.to_string()).and(predicate::str::contains("dev-server")),
        );

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
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    claim_live_lease(dir.path(), own_pid, None);

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
    claim_live_lease(dir, own_pid, None);

    let script = dir.join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.display()));
    (own_pid, script)
}

#[cfg(unix)]
#[test]
fn guard_denies_via_combined_dash_lc_flag() {
    let _ports = lock_test_ports();
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
    let _ports = lock_test_ports();
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
    let _ports = lock_test_ports();
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
    let _ports = lock_test_ports();
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

// ---- run ----
//
// The child fixtures below are plain binaries (`printenv`, `sleep`, `false`)
// executed directly — no shell is assumed by the implementation under test.

#[test]
fn run_rejects_absent_command_after_dash() {
    let dir = tempfile::tempdir().unwrap();

    cmd(dir.path())
        .args(["run", "4600", "--tag", "svc", "--"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[cfg(unix)]
#[test]
fn run_free_port_sets_portzilla_port_for_child() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let port = free_test_port();

    cmd(dir.path())
        .args([
            "run",
            &port.to_string(),
            "--tag",
            "svc",
            "--",
            "printenv",
            "PORTZILLA_PORT",
        ])
        .assert()
        .success()
        .stdout(format!("{port}\n"))
        .stderr(predicate::str::contains(port.to_string()));
}

#[cfg(unix)]
#[test]
fn run_conflicting_port_reassigns_and_sets_actual_port() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let own_pid = std::process::id();
    let requested = claim_live_lease(dir.path(), own_pid, None);

    let output = cmd(dir.path())
        .args([
            "run",
            &requested.to_string(),
            "--tag",
            "svc",
            "--",
            "printenv",
            "PORTZILLA_PORT",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("instead"))
        .get_output()
        .stdout
        .clone();

    let actual: u16 = String::from_utf8(output)
        .unwrap()
        .trim()
        .parse()
        .expect("child must print the assigned port");
    assert_ne!(
        actual, requested,
        "a conflicting requested port must be reassigned"
    );

    // The pre-existing lease on the requested port is left untouched.
    let who_output = cmd(dir.path())
        .args(["who", &requested.to_string(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let who_json: Value = serde_json::from_slice(&who_output).unwrap();
    assert_eq!(who_json["pid"], own_pid);
}

#[cfg(unix)]
#[test]
fn run_propagates_child_nonzero_exit() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let port = free_test_port();

    // `false` exits 1 without needing a shell.
    cmd(dir.path())
        .args(["run", &port.to_string(), "--tag", "svc", "--", "false"])
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains(port.to_string()));
}

#[cfg(unix)]
#[test]
fn run_passes_session_to_child_only_when_supplied() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();

    let with_session = free_test_port();
    cmd(dir.path())
        .env("PORTZILLA_SESSION", "parent-leak")
        .args([
            "run",
            &with_session.to_string(),
            "--tag",
            "svc",
            "--session",
            "run-sess",
            "--",
            "printenv",
            "PORTZILLA_SESSION",
        ])
        .assert()
        .success()
        .stdout("run-sess\n");

    // Without --session the variable must be absent even when the parent
    // environment carries a dirty PORTZILLA_SESSION: `printenv` on a missing
    // variable exits 1, and that child status propagates out of `run`.
    let without_session = free_test_port();
    cmd(dir.path())
        .env("PORTZILLA_SESSION", "parent-leak")
        .args([
            "run",
            &without_session.to_string(),
            "--tag",
            "svc",
            "--",
            "printenv",
            "PORTZILLA_SESSION",
        ])
        .assert()
        .code(1);
}

#[cfg(unix)]
#[test]
fn run_empty_session_is_treated_as_no_session() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let port = free_test_port();
    let binary = std::env::var_os("CARGO_BIN_EXE_portzilla").unwrap();

    let mut wrapper = std::process::Command::new(binary)
        .env("PORTZILLA_DATA_DIR", dir.path())
        .args([
            "run",
            &port.to_string(),
            "--tag",
            "run-svc",
            "--session",
            "",
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wrapper_pid = wrapper.id();

    let lease = poll_run_child_lease(dir.path(), "run-svc", wrapper_pid, Duration::from_secs(30))
        .expect("the run lease must be visible while the child runs");
    assert!(
        lease["session"].is_null(),
        "an empty --session must normalize to no session, got {}",
        lease["session"]
    );
    let child_pid = lease["pid"].as_u64().unwrap() as u32;

    std::process::Command::new("kill")
        .args(["-KILL", &child_pid.to_string()])
        .status()
        .unwrap();
    let status = terminate_and_reap(&mut wrapper);
    assert!(!status.success());

    // An empty --session must also remove the child variable even when the
    // parent carries a dirty PORTZILLA_SESSION, exactly like omitting it.
    let env_port = free_test_port();
    cmd(dir.path())
        .env("PORTZILLA_SESSION", "parent-leak")
        .args([
            "run",
            &env_port.to_string(),
            "--tag",
            "svc",
            "--session",
            "",
            "--",
            "printenv",
            "PORTZILLA_SESSION",
        ])
        .assert()
        .code(1);
}

/// Polls `ls` until the lease tagged `tag` names the transferred child rather
/// than the still-setting-up wrapper: between the wrapper's claim and its
/// transfer the lease legitimately names the wrapper PID, so returning the
/// first sighting would race with the transfer.
///
/// Looked up by tag, not by the requested port: `run` is allowed to reassign
/// the port when another process or lease takes it between the OS probe and
/// the claim, and polling the requested port would then wait out the whole
/// timeout for a lease that was correctly created somewhere else.
#[cfg(unix)]
fn poll_run_child_lease(
    data_dir: &std::path::Path,
    tag: &str,
    wrapper_pid: u32,
    timeout: Duration,
) -> Option<Value> {
    let binary = std::env::var_os("CARGO_BIN_EXE_portzilla").unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        let output = std::process::Command::new(&binary)
            .env("PORTZILLA_DATA_DIR", data_dir)
            .args(["ls", "--json"])
            .output()
            .ok()?;
        if output.status.success()
            && let Ok(Value::Array(leases)) = serde_json::from_slice::<Value>(&output.stdout)
            && let Some(lease) = leases.iter().find(|lease| {
                lease["tag"] == tag && lease["pid"].as_u64() != Some(u64::from(wrapper_pid))
            })
        {
            return Some(lease.clone());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn run_guard_denies_kill_of_run_child_from_foreign_session() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let port = free_test_port();
    let binary = std::env::var_os("CARGO_BIN_EXE_portzilla").unwrap();

    let mut wrapper = std::process::Command::new(binary)
        .env("PORTZILLA_DATA_DIR", dir.path())
        .args([
            "run",
            &port.to_string(),
            "--tag",
            "run-svc",
            "--session",
            "run-sess",
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wrapper_pid = wrapper.id();

    let lease = poll_run_child_lease(dir.path(), "run-svc", wrapper_pid, Duration::from_secs(30))
        .expect("the run lease must be visible while the child runs");
    assert_eq!(lease["tag"], "run-svc");
    assert_eq!(lease["session"], "run-sess");
    assert_eq!(lease["alive"], true);
    // The wrapper reassigns when the requested port was taken between the probe
    // and the claim, so the lease's port is the authority from here on.
    let actual_port = lease["port"].as_u64().unwrap() as u16;
    let child_pid = lease["pid"].as_u64().unwrap() as u32;
    assert_ne!(
        child_pid, wrapper_pid,
        "the transferred lease must name the child, not the wrapper"
    );

    // `who` on the actual port reports the same live child PID.
    let who_output = cmd(dir.path())
        .args(["who", &actual_port.to_string(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let who_json: Value = serde_json::from_slice(&who_output).unwrap();
    assert_eq!(who_json["pid"], child_pid);
    assert_eq!(who_json["alive"], true);

    // Fake "kill" so a guard decision can be observed without signals: a
    // deny must never run it, an allow must.
    let script = dir.path().join("kill");
    write_fake_command(&script, &format!("touch '{}/marker'", dir.path().display()));

    // Foreign session (no session at all): must deny with exit 2.
    cmd(dir.path())
        .args([
            "guard",
            "--",
            script.to_str().unwrap(),
            &child_pid.to_string(),
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(
            predicate::str::contains(port.to_string()).and(predicate::str::contains("run-svc")),
        );
    assert!(
        !dir.path().join("marker").exists(),
        "a denied guard kill of a run child must never execute"
    );

    // Matching session: the owner's own kill must be allowed through.
    cmd(dir.path())
        .args([
            "guard",
            "--session",
            "run-sess",
            "--",
            script.to_str().unwrap(),
            &child_pid.to_string(),
        ])
        .assert()
        .success();
    assert!(
        dir.path().join("marker").exists(),
        "an own-session guard kill of a run child must execute"
    );

    // Condition-based teardown only: signal the child, then reap the
    // wrapper (it was waiting on the child) with the polling helper.
    std::process::Command::new("kill")
        .args(["-KILL", &child_pid.to_string()])
        .status()
        .unwrap();
    let status = terminate_and_reap(&mut wrapper);
    assert!(!status.success());
}

#[cfg(unix)]
#[test]
fn run_child_lease_is_live_then_dead_then_pruned() {
    let _ports = lock_test_ports();
    let dir = tempfile::tempdir().unwrap();
    let port = free_test_port();
    let binary = std::env::var_os("CARGO_BIN_EXE_portzilla").unwrap();

    let mut wrapper = std::process::Command::new(binary)
        .env("PORTZILLA_DATA_DIR", dir.path())
        .args([
            "run",
            &port.to_string(),
            "--tag",
            "run-svc",
            "--session",
            "run-sess",
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wrapper_pid = wrapper.id();

    let lease = poll_run_child_lease(dir.path(), "run-svc", wrapper_pid, Duration::from_secs(30))
        .expect("the run lease must be visible while the child runs");
    assert_eq!(lease["tag"], "run-svc");
    assert_eq!(lease["session"], "run-sess");
    assert_eq!(lease["alive"], true);
    // The wrapper reassigns when the requested port was taken between the probe
    // and the claim, so the lease's port is the authority from here on.
    let actual_port = lease["port"].as_u64().unwrap() as u16;
    let child_pid = lease["pid"].as_u64().unwrap() as u32;

    // Ending the child ends the wrapper (it was waiting on the child); the
    // signaled child propagates as a nonzero wrapper exit.
    std::process::Command::new("kill")
        .args(["-KILL", &child_pid.to_string()])
        .status()
        .unwrap();
    let status = terminate_and_reap(&mut wrapper);
    assert!(!status.success());

    let output = cmd(dir.path())
        .args(["who", &actual_port.to_string(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let dead: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(dead["pid"], child_pid);
    assert_eq!(dead["alive"], false);

    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("leases.json")).unwrap()).unwrap();
    assert_eq!(state["events"][2]["event"], "process_exited");
    assert_eq!(state["events"][2]["data"]["outcome"]["kind"], "signal");

    cmd(dir.path())
        .args(["prune"])
        .assert()
        .success()
        .stdout(predicate::str::contains(port.to_string()));
    cmd(dir.path())
        .args(["who", &port.to_string()])
        .assert()
        .failure()
        .code(2);
}

// ---- init skill ----

#[test]
fn init_skill_forwards_supported_agent_sessions_to_run() {
    let dir = tempfile::tempdir().unwrap();

    let output = cmd(dir.path())
        .args(["init", "skill"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let skill = String::from_utf8(output).unwrap();

    assert!(
        skill.contains("--session \"$PORTZILLA_SESSION\""),
        "the skill must forward OpenCode's injected session to run"
    );
    assert!(
        skill.contains("--session \"$CLAUDE_CODE_SESSION_ID\""),
        "the skill must forward Claude Code's session to run"
    );
    assert!(
        skill.contains("Never pass `--session \"\"`"),
        "the skill must forbid empty --session expansions"
    );
    assert!(
        skill.contains("SESSION_ARGS"),
        "the skill must show an empty-safe conditional session expansion"
    );
}

#[test]
fn init_skill_prints_the_skill_file_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let expected = include_str!("../skills/portzilla/SKILL.md");

    let output = cmd(dir.path())
        .args(["init", "skill"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        output,
        expected.as_bytes(),
        "portzilla init skill must print skills/portzilla/SKILL.md byte-for-byte, so \
         `portzilla init skill > .opencode/skills/portzilla/SKILL.md` reproduces the file"
    );
}

// ---- intent ----

/// A claimed path plus the data directory, for the `intent` command tests.
fn intent_paths() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("auth.rs");
    (project, file)
}

#[test]
fn intent_claim_then_who_then_release_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .args(["intent", "claim", path, "--session", "session-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-a"))
        .stdout(predicate::str::contains("auth.rs"));

    cmd(dir.path())
        .args(["intent", "who", path])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-a"))
        .stdout(predicate::str::contains("auth.rs"));

    let output = cmd(dir.path())
        .args(["intent", "who", path, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["session"], "session-a");
    assert_eq!(json["live"], true);
    assert!(
        json["key"].as_str().unwrap().contains("auth.rs"),
        "the key must name the file: {json}"
    );
    assert_eq!(
        json["file"].as_str().unwrap(),
        _project
            .path()
            .canonicalize()
            .unwrap()
            .join("auth.rs")
            .to_str()
            .unwrap(),
        "the JSON file must be the located working copy"
    );
    assert!(json["created_at"].is_u64());
    assert!(json["expires_at"].is_u64());
    assert!(json["age_secs"].is_u64());

    cmd(dir.path())
        .args(["intent", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("KEY SESSION AGE EXPIRES"))
        .stdout(predicate::str::contains("session-a"));

    cmd(dir.path())
        .args(["intent", "release", path, "--session", "session-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("released"));

    // The released claim is gone: `who` reports it as absent (exit 2).
    cmd(dir.path())
        .args(["intent", "who", path])
        .assert()
        .failure()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("no live file intent"));
}

#[test]
fn intent_claim_by_another_session_exits_1_and_names_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .args(["intent", "claim", path, "--session", "session-a"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["intent", "claim", path, "--session", "session-b"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("session-a"))
        .stderr(predicate::str::contains("expires in"));
}

#[test]
fn intent_release_by_a_non_owner_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .args(["intent", "claim", path, "--session", "session-a"])
        .assert()
        .success();

    cmd(dir.path())
        .args(["intent", "release", path, "--session", "session-b"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("session-a"))
        .stderr(predicate::str::contains("expires in"));

    // The owner's claim survived the rejected release.
    cmd(dir.path())
        .args(["intent", "who", path])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-a"));
}

#[test]
fn intent_release_of_an_absent_claim_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .args(["intent", "release", path, "--session", "session-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already absent"));

    let output = cmd(dir.path())
        .args([
            "intent",
            "release",
            path,
            "--session",
            "session-a",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("stdout must be valid JSON");
    assert_eq!(json["released"], false);
    assert!(json["key"].as_str().unwrap().contains("auth.rs"));
}

#[test]
fn intent_claim_uses_portzilla_session_when_the_flag_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .env("PORTZILLA_SESSION", "env-session")
        .args(["intent", "claim", path])
        .assert()
        .success()
        .stdout(predicate::str::contains("env-session"));

    let output = cmd(dir.path())
        .args(["intent", "who", path, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["session"], "env-session");
}

#[test]
fn intent_claim_without_any_session_errors_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    cmd(dir.path())
        .env_remove("PORTZILLA_SESSION")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .args(["intent", "claim", path])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "intent claim needs a session: pass --session or set PORTZILLA_SESSION",
        ));

    cmd(dir.path())
        .env_remove("PORTZILLA_SESSION")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .args(["intent", "release", path])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("intent release needs a session"));
}

#[test]
fn intent_ls_json_lists_only_live_claims() {
    let dir = tempfile::tempdir().unwrap();
    // Seed one live and one expired record directly: an expired claim is
    // invisible to `ls` but not reaped by it, which is the store's contract.
    std::fs::write(
        dir.path().join("intents.json"),
        r#"{"format_version":2,"intents":[
            {"key":"repo:live.rs","session":"session-live","created_at":1000,"expires_at":4000000000},
            {"key":"repo:expired.rs","session":"session-expired","created_at":1000,"expires_at":1001}
        ]}"#,
    )
    .unwrap();

    let output = cmd(dir.path())
        .args(["intent", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let intents = json.as_array().unwrap();
    assert_eq!(
        intents.len(),
        1,
        "only the live claim may be listed: {json}"
    );
    assert_eq!(intents[0]["key"], "repo:live.rs");
    assert_eq!(intents[0]["live"], true);
}

#[test]
fn intent_claim_honours_ttl_secs() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    let output = cmd(dir.path())
        .args([
            "intent",
            "claim",
            path,
            "--session",
            "session-a",
            "--ttl-secs",
            "120",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let created_at = json["created_at"].as_u64().unwrap();
    let expires_at = json["expires_at"].as_u64().unwrap();
    assert_eq!(expires_at - created_at, 120);
    assert_eq!(json["live"], true);
}

#[test]
fn intent_claim_rejects_out_of_range_ttl_secs() {
    let dir = tempfile::tempdir().unwrap();
    let (_project, file) = intent_paths();
    let path = file.to_str().unwrap();

    for ttl in ["0", "86401"] {
        cmd(dir.path())
            .args([
                "intent",
                "claim",
                path,
                "--session",
                "session-a",
                "--ttl-secs",
                ttl,
            ])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("--ttl-secs must be between"));
    }
}

// ---- environment slots ----

fn write_slot_config(worktree: &std::path::Path, contents: &str) -> std::path::PathBuf {
    let path = worktree.join("portzilla.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

fn slot_json_output(
    data_dir: &std::path::Path,
    worktree: &std::path::Path,
    args: &[&str],
) -> Value {
    let output = cmd(data_dir)
        .current_dir(worktree)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[cfg(unix)]
#[test]
fn slot_round_trip_covers_up_ls_show_env_run_and_down() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    let config = format!(
        r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = {port}

[slot.cache]
CARGO_TARGET_DIR = ".slot/{{name}}/target"

[slot.env]
GREETING = "hello world"
"#
    );
    write_slot_config(worktree.path(), &config);
    let target = worktree.path().join(".slot/auth/target");
    let env_file = worktree.path().join(".env.portzilla");

    let up = slot_json_output(
        data.path(),
        worktree.path(),
        &["slot", "up", "auth", "--json"],
    );
    assert_eq!(up["name"], "auth");
    assert_eq!(up["ports"]["web"], port);
    assert_eq!(up["db"], Value::Null);
    assert_eq!(up["replaced"], false);
    assert!(target.is_dir());
    assert!(env_file.is_file());
    let up_keys: std::collections::HashSet<&str> =
        up.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        up_keys,
        [
            "name",
            "worktree",
            "repo_key",
            "ports",
            "db",
            "cache_dirs",
            "env_file",
            "created_at",
            "age_secs",
            "replaced",
        ]
        .into_iter()
        .collect()
    );

    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["name"], "auth");
    assert_eq!(listed[0]["ports"]["web"], port);

    let shown = slot_json_output(
        data.path(),
        worktree.path(),
        &["slot", "show", "auth", "--json"],
    );
    // `age_secs` is rendered from the wall clock, so it can tick between the
    // two invocations; every other field must match exactly.
    let without_age = |mut value: Value| {
        value.as_object_mut().unwrap().remove("age_secs");
        value
    };
    assert_eq!(without_age(shown), without_age(listed[0].clone()));

    // `env` is line-oriented KEY=value and survives `eval`.
    let env_output = cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "env", "auth"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env_text = String::from_utf8(env_output).unwrap();
    assert!(env_text.contains(&format!("PORTZILLA_PORT_WEB={port}\n")));
    assert!(env_text.contains("GREETING=\"hello world\"\n"));
    let evaluated = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{env_text}\nprintf '%s' \"$PORTZILLA_PORT_WEB|$GREETING\""
        ))
        .output()
        .unwrap();
    assert!(evaluated.status.success());
    assert_eq!(
        String::from_utf8(evaluated.stdout).unwrap(),
        format!("{port}|hello world")
    );

    cmd(data.path())
        .current_dir(worktree.path())
        .args([
            "slot",
            "run",
            "auth",
            "--",
            "printenv",
            "PORTZILLA_PORT_WEB",
        ])
        .assert()
        .success()
        .stdout(format!("{port}\n"));

    // `run` is transparent about the child's exit code.
    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "run", "auth", "--", "sh", "-c", "exit 7"])
        .assert()
        .code(7);

    let down = slot_json_output(
        data.path(),
        worktree.path(),
        &["slot", "down", "auth", "--json"],
    );
    assert_eq!(down["released_ports"], serde_json::json!([port]));
    assert_eq!(down["forced"], false);
    assert!(down["purged_dirs"].as_array().unwrap().len() == 1);
    assert!(!target.exists());
    assert!(!env_file.exists());

    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed, serde_json::json!([]));
    // The durable lease went with the record.
    cmd(data.path())
        .args(["who", &port.to_string(), "--json"])
        .assert()
        .code(2);
}

#[test]
fn slot_up_twice_fails_and_replace_replaces() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(
        worktree.path(),
        &format!("[slot]\npurge_cache_on_down = true\n\n[slot.ports]\nweb = {port}\n"),
    );

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already up"))
        .stderr(predicate::str::contains("--replace"));

    let replaced = slot_json_output(
        data.path(),
        worktree.path(),
        &["slot", "up", "auth", "--replace", "--json"],
    );
    assert_eq!(replaced["replaced"], true);
    assert_eq!(replaced["ports"]["web"], port);
    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "down", "auth"])
        .assert()
        .success();
}

#[test]
fn slot_missing_commands_exit_2() {
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    write_slot_config(worktree.path(), "[slot.ports]\nweb = 3600\n");

    for args in [
        vec!["slot", "show", "ghost"],
        vec!["slot", "down", "ghost"],
        vec!["slot", "env", "ghost"],
        vec!["slot", "run", "ghost", "--", "true"],
    ] {
        cmd(data.path())
            .current_dir(worktree.path())
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("no slot named ghost"));
    }
}

#[test]
fn slot_prune_removes_vanished_worktrees_and_releases_their_ports() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(
        worktree.path(),
        &format!("[slot]\npurge_cache_on_down = true\n\n[slot.ports]\nweb = {port}\n"),
    );

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    drop(worktree);

    let elsewhere = tempfile::tempdir().unwrap();
    let pruned = slot_json_output(data.path(), elsewhere.path(), &["slot", "prune", "--json"]);
    assert_eq!(pruned.as_array().unwrap().len(), 1);
    assert_eq!(pruned[0]["name"], "auth");
    assert_eq!(pruned[0]["ports"]["web"], port);

    let listed = slot_json_output(data.path(), elsewhere.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed, serde_json::json!([]));
    cmd(data.path())
        .args(["who", &port.to_string(), "--json"])
        .assert()
        .code(2);
    cmd(data.path())
        .current_dir(elsewhere.path())
        .args(["slot", "prune"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no slots to prune"));
}

#[cfg(unix)]
#[test]
fn slot_down_keeps_the_record_when_deprovision_fails_unless_forced() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(
        worktree.path(),
        &format!(
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = {port}

[slot.cache]
CARGO_TARGET_DIR = ".slot/{{name}}/target"

[slot.db]
name_template = "app_{{slot}}"
provision = ["sh", "-c", "true"]
deprovision = ["sh", "-c", "echo deprovision-broke >&2; exit 5"]
"#
        ),
    );
    let target = worktree.path().join(".slot/auth/target");

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "down", "auth"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("deprovision-broke"))
        .stderr(predicate::str::contains("--force"));

    // Retryable: everything is still in place.
    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(target.is_dir());
    cmd(data.path())
        .args(["who", &port.to_string(), "--json"])
        .assert()
        .success();

    let forced = slot_json_output(
        data.path(),
        worktree.path(),
        &["slot", "down", "auth", "--force", "--json"],
    );
    assert_eq!(forced["forced"], true);
    assert!(
        !forced["warnings"].as_array().unwrap().is_empty(),
        "the forced skip must be reported"
    );
    assert!(!target.exists());
    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed, serde_json::json!([]));
    cmd(data.path())
        .args(["who", &port.to_string(), "--json"])
        .assert()
        .code(2);
}

#[cfg(unix)]
#[test]
fn slot_up_rolls_back_when_provision_fails() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(
        worktree.path(),
        &format!(
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = {port}

[slot.cache]
CARGO_TARGET_DIR = ".slot/{{name}}/target"

[slot.db]
name_template = "app_{{slot}}"
provision = ["sh", "-c", "echo provision-broke >&2; exit 3"]
deprovision = ["sh", "-c", "true"]
"#
        ),
    );

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("provision-broke"));

    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed, serde_json::json!([]));
    assert!(!worktree.path().join(".slot/auth/target").exists());
    assert!(!worktree.path().join(".env.portzilla").exists());
    cmd(data.path())
        .args(["who", &port.to_string(), "--json"])
        .assert()
        .code(2);
}

#[test]
fn slot_keeps_its_ports_reserved_from_a_plain_claim() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(worktree.path(), &format!("[slot.ports]\nweb = {port}\n"));

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    let own_pid = std::process::id().to_string();
    let claimed = slot_json_output(
        data.path(),
        worktree.path(),
        &[
            "claim",
            &port.to_string(),
            "--tag",
            "intruder",
            "--pid",
            &own_pid,
            "--json",
        ],
    );
    assert_eq!(claimed["reassigned"], true);
    assert_ne!(claimed["port"], port);

    // The reservation is untouched and still names the slot.
    let who = slot_json_output(
        data.path(),
        worktree.path(),
        &["who", &port.to_string(), "--json"],
    );
    assert_eq!(who["tag"], "slot:auth:web");

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "down", "auth"])
        .assert()
        .success();
}

#[cfg(unix)]
#[test]
fn slot_down_never_releases_a_port_another_owner_took() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(worktree.path(), &format!("[slot.ports]\nweb = {port}\n"));

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    // Drop the slot's lease, then take the port for a different tag.
    cmd(data.path())
        .args(["release", &port.to_string()])
        .assert()
        .success();
    let own_pid = std::process::id().to_string();
    cmd(data.path())
        .args([
            "claim",
            &port.to_string(),
            "--tag",
            "foreign-owner",
            "--pid",
            &own_pid,
        ])
        .assert()
        .success();

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "down", "auth"])
        .assert()
        .success()
        .stderr(predicate::str::contains("not by slot auth"));

    let who = slot_json_output(
        data.path(),
        worktree.path(),
        &["who", &port.to_string(), "--json"],
    );
    assert_eq!(who["tag"], "foreign-owner");
}

#[cfg(unix)]
#[test]
fn slot_env_config_reloads_only_slot_env_values() {
    let _ports = lock_test_ports();
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let port = free_test_port();
    write_slot_config(
        worktree.path(),
        &format!("[slot.ports]\nweb = {port}\n\n[slot.env]\nGREETING = \"first\"\n"),
    );
    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .success();

    let override_config = worktree.path().join("override.toml");
    std::fs::write(
        &override_config,
        format!(
            "[slot.ports]\nweb = {port}\n\n[slot.env]\nGREETING = \"second\"\nEXTRA = \"yes\"\n"
        ),
    )
    .unwrap();

    let reloaded = cmd(data.path())
        .current_dir(worktree.path())
        .args([
            "slot",
            "env",
            "auth",
            "--config",
            override_config.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let reloaded = String::from_utf8(reloaded).unwrap();
    assert!(reloaded.contains("GREETING=second\n"));
    assert!(reloaded.contains("EXTRA=yes\n"));
    assert!(reloaded.contains(&format!("PORTZILLA_PORT_WEB={port}\n")));

    // Without --config the stored environment is unchanged.
    let stored = cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "env", "auth"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(stored)
            .unwrap()
            .contains("GREETING=first\n")
    );

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "down", "auth"])
        .assert()
        .success();
}

#[test]
fn slot_up_names_the_missing_config_and_creates_nothing() {
    let data = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();

    cmd(data.path())
        .current_dir(worktree.path())
        .args(["slot", "up", "auth"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("portzilla.toml"));

    let listed = slot_json_output(data.path(), worktree.path(), &["slot", "ls", "--json"]);
    assert_eq!(listed, serde_json::json!([]));
    assert!(!worktree.path().join(".env.portzilla").exists());
}
