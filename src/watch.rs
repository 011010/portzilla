use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::PathBuf;

use crate::lease::{Lease, PidChecker, SystemPidChecker, current_unix_timestamp};
use crate::store::Store;
use crate::view::LeaseView;

/// A conservative polling interval that avoids making a foreground watcher
/// contend with normal CLI and MCP store access by default.
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 60;

pub(crate) fn parse_interval_secs(value: &str) -> std::result::Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "interval must be a positive number of seconds".to_string())?;
    if seconds == 0 {
        return Err("interval must be greater than zero seconds".to_string());
    }
    Ok(seconds)
}

/// Runs one lease-pruning cycle using the production process liveness checker.
pub(crate) fn run_cycle(data_dir: Option<PathBuf>) -> Result<Vec<Lease>> {
    run_cycle_with_checker(data_dir, &SystemPidChecker)
}

/// Runs the watcher in the foreground until the process receives Ctrl-C.
/// Store failures are transient after startup, so they are reported and
/// retried on the next interval rather than terminating the watcher. A
/// running blocking task cannot be forcibly stopped; shutdown aborts its
/// handle and lets process exit bound that task's remaining lifetime.
pub(crate) async fn run_loop(
    data_dir: Option<PathBuf>,
    interval_secs: u64,
    json: bool,
) -> Result<()> {
    if interval_secs == 0 {
        bail!("interval must be greater than zero seconds");
    }

    let interval = std::time::Duration::from_secs(interval_secs);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut cycle = spawn_cycle(data_dir.clone());

    loop {
        tokio::select! {
            signal = &mut ctrl_c => {
                signal.context("failed to listen for Ctrl-C")?;
                cycle.abort();
                eprintln!("watch: received Ctrl-C, shutting down");
                return Ok(());
            }
            result = &mut cycle => {
                report_cycle_result(result, json)?;
            }
        }

        tokio::select! {
            signal = &mut ctrl_c => {
                signal.context("failed to listen for Ctrl-C")?;
                eprintln!("watch: received Ctrl-C, shutting down");
                return Ok(());
            }
            _ = tokio::time::sleep(interval) => {
                cycle = spawn_cycle(data_dir.clone());
            }
        }
    }
}

fn spawn_cycle(data_dir: Option<PathBuf>) -> tokio::task::JoinHandle<Result<Vec<Lease>>> {
    tokio::task::spawn_blocking(move || run_cycle(data_dir))
}

fn report_cycle_result(
    result: std::result::Result<Result<Vec<Lease>>, tokio::task::JoinError>,
    json: bool,
) -> Result<()> {
    let pruned = result.context("watch cycle task failed")?;
    match pruned {
        Ok(pruned) => {
            let output = if json {
                render_json_cycle(&pruned)
            } else {
                render_human_cycle(&pruned)
            };
            println!("{output}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        Err(err) => eprintln!("watch: cycle failed: {err:#}; retrying"),
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonCycleEvent {
    event: &'static str,
    pruned: Vec<LeaseView>,
}

pub(crate) fn render_json_cycle(leases: &[Lease]) -> String {
    let event = JsonCycleEvent {
        event: "watch_cycle",
        pruned: leases.iter().map(to_dead_view).collect(),
    };
    serde_json::to_string(&event).expect("watch cycle event always serializes")
}

fn to_dead_view(lease: &Lease) -> LeaseView {
    LeaseView {
        port: lease.port,
        pid: lease.pid,
        tag: lease.tag.clone(),
        created_at: lease.created_at,
        session: lease.session.clone(),
        process_start_time: lease.process_start_time,
        age_secs: current_unix_timestamp().saturating_sub(lease.created_at),
        alive: false,
    }
}

fn render_human_cycle(leases: &[Lease]) -> String {
    if leases.is_empty() {
        return "no leases pruned".to_string();
    }

    leases
        .iter()
        .map(|lease| {
            format!(
                "pruned port {} (pid {}, tag: {})",
                lease.port,
                lease.pid,
                crate::sanitize_for_display(&lease.tag)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(dead_code)] // The checker-injected entry point is exercised by module tests.
fn run_cycle_with_checker(
    data_dir: Option<PathBuf>,
    checker: &dyn PidChecker,
) -> Result<Vec<Lease>> {
    let store = Store::open(data_dir).context("failed to open store for watch cycle")?;
    store
        .prune(checker)
        .context("failed to prune leases during watch cycle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{
        Lease,
        test_support::{AlivePids, AlwaysAlive, AlwaysDead},
    };
    use crate::store::Store;

    #[test]
    fn cycle_removes_dead_leases_and_returns_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let leases = vec![
            Lease::new(27000, 100, "live", None),
            Lease::new(27001, 200, "dead", None),
        ];
        std::fs::write(
            store.state_file_path(),
            serde_json::to_vec(&serde_json::json!({
                "format_version": 2,
                "leases": leases,
            }))
            .unwrap(),
        )
        .unwrap();

        let removed =
            run_cycle_with_checker(Some(dir.path().to_path_buf()), &AlivePids(vec![100])).unwrap();

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].port, 27001);
        assert_eq!(
            store
                .list()
                .unwrap()
                .iter()
                .map(|lease| lease.port)
                .collect::<Vec<_>>(),
            vec![27000]
        );
    }

    #[test]
    fn cycle_leaves_live_leases_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(27002, 100, "live".to_string(), None, &AlwaysAlive)
            .unwrap();
        let before = store.list().unwrap();

        let removed = run_cycle_with_checker(Some(dir.path().to_path_buf()), &AlwaysAlive).unwrap();

        assert!(removed.is_empty());
        assert_eq!(store.list().unwrap(), before);
    }

    #[test]
    fn cycle_returns_corrupt_state_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        std::fs::write(store.state_file_path(), b"{ not valid json").unwrap();

        let result = run_cycle_with_checker(Some(dir.path().to_path_buf()), &AlwaysDead);

        assert!(result.is_err());
    }

    #[test]
    fn interval_parser_rejects_zero_and_invalid_values() {
        assert!(parse_interval_secs("0").is_err());
        assert!(parse_interval_secs("not-a-number").is_err());
        assert_eq!(parse_interval_secs("30").unwrap(), 30);
    }

    #[tokio::test]
    async fn loop_rejects_zero_interval_at_its_boundary() {
        assert!(run_loop(None, 0, false).await.is_err());
    }

    #[test]
    fn json_cycle_event_contains_pruned_lease_views_even_when_empty() {
        let json: serde_json::Value = serde_json::from_str(&render_json_cycle(&[])).unwrap();

        assert_eq!(json["event"], "watch_cycle");
        assert_eq!(json["pruned"], serde_json::json!([]));
    }

    #[test]
    fn json_cycle_event_serializes_pruned_lease_views() {
        let pid = std::process::id();
        let lease = Lease::new(27003, pid, "dead", Some("session-1".to_string()));
        let json: serde_json::Value = serde_json::from_str(&render_json_cycle(&[lease])).unwrap();

        assert_eq!(json["event"], "watch_cycle");
        assert_eq!(json["pruned"][0]["port"], 27003);
        assert_eq!(json["pruned"][0]["pid"], pid);
        assert_eq!(json["pruned"][0]["tag"], "dead");
        assert_eq!(json["pruned"][0]["alive"], false);
    }

    #[test]
    fn human_cycle_event_reports_pruned_port_pid_and_sanitized_tag() {
        let lease = Lease::new(27004, 201, "dead\nserver", None);

        assert_eq!(
            render_human_cycle(&[lease]),
            "pruned port 27004 (pid 201, tag: dead server)"
        );
    }
}
