//! Locked JSON persistence for leases, and the port-claiming core logic.

use crate::lease::{Lease, PidChecker};
use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

/// Restricts `path` (a directory) to owner-only access (`0700`) on Unix.
/// No-op on non-Unix platforms, which don't share the same permission model.
#[cfg(unix)]
fn harden_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set owner-only permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn harden_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Restricts `path` (a file) to owner-only access (`0600`) on Unix.
/// No-op on non-Unix platforms, which don't share the same permission model.
#[cfg(unix)]
fn harden_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set owner-only permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn harden_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

const DATA_DIR_ENV_VAR: &str = "PORTZILLA_DATA_DIR";
const XDG_DATA_HOME_ENV_VAR: &str = "XDG_DATA_HOME";
const HOME_ENV_VAR: &str = "HOME";

/// Result of a `claim` operation: the lease that was actually created/updated,
/// and whether it landed on a different port than the one requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimOutcome {
    pub lease: Lease,
    pub reassigned: bool,
}

/// Result of a `release` operation: the lease that was removed, and whether
/// its owning PID was still alive at the time of release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseOutcome {
    pub lease: Lease,
    pub was_alive: bool,
}

/// A locked JSON-backed store of leases rooted at a resolved data directory.
///
/// `Clone` is cheap and intentional: the store carries no in-memory state of
/// its own (`data_dir` is the only field) — every read-modify-write goes
/// through the on-disk lock, so cloning just shares the same path, the same
/// way opening a second `Store` pointed at the same directory would. This is
/// what lets the MCP server hand a `Store` to its `Clone`-bound handler type
/// without wrapping it in an `Arc` for no reason.
#[derive(Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    /// Resolves the data directory to use, in priority order:
    /// 1. An explicit override (used by callers/tests that already know the path).
    /// 2. The `PORTZILLA_DATA_DIR` environment variable (used directly as the data dir).
    /// 3. `$XDG_DATA_HOME/portzilla`.
    /// 4. `$HOME/.local/share/portzilla`.
    pub fn resolve_data_dir(
        override_dir: Option<PathBuf>,
        env_portzilla_data_dir: Option<String>,
        env_xdg_data_home: Option<String>,
        env_home: Option<String>,
    ) -> Result<PathBuf> {
        if let Some(dir) = override_dir {
            return Ok(dir);
        }
        if let Some(dir) = env_portzilla_data_dir {
            return Ok(PathBuf::from(dir));
        }
        if let Some(xdg) = env_xdg_data_home {
            return Ok(PathBuf::from(xdg).join("portzilla"));
        }
        if let Some(home) = env_home {
            return Ok(PathBuf::from(home).join(".local/share/portzilla"));
        }
        bail!(
            "could not determine a data directory: set {} or {} or {}",
            DATA_DIR_ENV_VAR,
            XDG_DATA_HOME_ENV_VAR,
            HOME_ENV_VAR
        )
    }

    /// Opens a store, resolving the data directory from the real process
    /// environment unless `override_dir` is given.
    pub fn open(override_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = Self::resolve_data_dir(
            override_dir,
            std::env::var(DATA_DIR_ENV_VAR).ok(),
            std::env::var(XDG_DATA_HOME_ENV_VAR).ok(),
            std::env::var(HOME_ENV_VAR).ok(),
        )?;
        let store = Self { data_dir };
        store.ensure_data_dir()?;
        Ok(store)
    }

    /// Path to the JSON state file inside the data directory.
    pub fn state_file_path(&self) -> PathBuf {
        self.data_dir.join("leases.json")
    }

    fn lock_file_path(&self) -> PathBuf {
        self.data_dir.join("leases.json.lock")
    }

    /// Returns all currently stored leases.
    pub fn list(&self) -> Result<Vec<Lease>> {
        let _guard = self.lock_exclusive()?;
        self.read_leases()
    }

    /// Claims `requested_port` for `pid`, following the conflict-resolution
    /// rules documented on [`claim_in_place`].
    pub fn claim(
        &self,
        requested_port: u16,
        pid: u32,
        tag: String,
        session: Option<String>,
        checker: &dyn PidChecker,
    ) -> Result<ClaimOutcome> {
        let _guard = self.lock_exclusive()?;
        let mut leases = self.read_leases()?;
        let outcome = claim_in_place(&mut leases, requested_port, pid, tag, session, checker)?;
        self.write_leases(&leases)?;
        Ok(outcome)
    }

    /// Returns the lease on `port`, if any.
    pub fn get(&self, port: u16) -> Result<Option<Lease>> {
        let _guard = self.lock_exclusive()?;
        let leases = self.read_leases()?;
        Ok(leases.into_iter().find(|lease| lease.port == port))
    }

    /// Removes the lease on `port`, if any, and reports whether its owning
    /// PID was still alive at the time of removal. Returns `None` if there
    /// was no lease on `port`.
    pub fn release(&self, port: u16, checker: &dyn PidChecker) -> Result<Option<ReleaseOutcome>> {
        let _guard = self.lock_exclusive()?;
        let mut leases = self.read_leases()?;
        let outcome = release_in_place(&mut leases, port, checker);
        if outcome.is_some() {
            self.write_leases(&leases)?;
        }
        Ok(outcome)
    }

    /// Removes every lease whose owning PID is dead, and returns the
    /// removed leases. Returns an empty vector if nothing was pruned.
    pub fn prune(&self, checker: &dyn PidChecker) -> Result<Vec<Lease>> {
        let _guard = self.lock_exclusive()?;
        let mut leases = self.read_leases()?;
        let pruned = prune_in_place(&mut leases, checker);
        if !pruned.is_empty() {
            self.write_leases(&leases)?;
        }
        Ok(pruned)
    }

    /// Ensures the data directory exists and is restricted to owner-only
    /// access (`0700` on Unix). Leases can carry PIDs and tags that reveal
    /// what a user is running, so the store directory is not world-readable.
    fn ensure_data_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("failed to create data directory at {}", self.data_dir.display()))?;
        harden_dir_permissions(&self.data_dir)
    }

    /// Acquires the exclusive lock used to guard read-modify-write access to
    /// the state file. The lock is released when the returned file handle drops.
    fn lock_exclusive(&self) -> Result<fs::File> {
        self.ensure_data_dir()?;
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.lock_file_path())
            .with_context(|| format!("failed to open lock file at {}", self.lock_file_path().display()))?;
        harden_file_permissions(&self.lock_file_path())?;
        // Called explicitly via the `fs4` trait (rather than relying on the
        // newer `std::fs::File::lock` inherent method) so locking stays on a
        // dependency with documented cross-platform semantics.
        fs4::FileExt::lock(&lock_file)
            .context("failed to acquire exclusive lock on the lease store")?;
        Ok(lock_file)
    }

    fn read_leases(&self) -> Result<Vec<Lease>> {
        let path = self.state_file_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read state file at {}", path.display()))
            }
        };
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("state file at {} contains invalid JSON and was not modified", path.display()))
    }

    fn write_leases(&self, leases: &[Lease]) -> Result<()> {
        let path = self.state_file_path();
        let tmp_path = self.data_dir.join(format!(".leases.json.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(leases).context("failed to serialize leases to JSON")?;
        fs::write(&tmp_path, json)
            .with_context(|| format!("failed to write temporary state file at {}", tmp_path.display()))?;
        // Harden before the rename: rename preserves the file's existing mode,
        // so this also leaves the final `leases.json` owner-only (0600).
        harden_file_permissions(&tmp_path)?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to atomically replace state file at {}", path.display()))?;
        Ok(())
    }

    /// Test-only variant of [`Store::claim`] that runs `hook` while the
    /// exclusive lock is held, so tests can observe mutual exclusion directly.
    #[cfg(test)]
    pub(crate) fn claim_with_hook(
        &self,
        requested_port: u16,
        pid: u32,
        tag: String,
        session: Option<String>,
        checker: &dyn PidChecker,
        hook: impl FnOnce(),
    ) -> Result<ClaimOutcome> {
        let _guard = self.lock_exclusive()?;
        hook();
        let mut leases = self.read_leases()?;
        let outcome = claim_in_place(&mut leases, requested_port, pid, tag, session, checker)?;
        self.write_leases(&leases)?;
        Ok(outcome)
    }
}

/// Applies the claim logic in-memory against an existing lease list:
///
/// - No lease on `requested_port`, or its lease's PID is dead: create/replace
///   the lease at `requested_port`. Not reassigned.
/// - A live lease on `requested_port` owned by `pid` itself: update it in place
///   (idempotent re-claim). Not reassigned.
/// - A live lease on `requested_port` owned by a different PID: find the next
///   available port at or after `requested_port + 1` and lease that instead.
///   Reassigned.
pub(crate) fn claim_in_place(
    leases: &mut Vec<Lease>,
    requested_port: u16,
    pid: u32,
    tag: String,
    session: Option<String>,
    checker: &dyn PidChecker,
) -> Result<ClaimOutcome> {
    let existing_index = leases.iter().position(|l| l.port == requested_port);

    match existing_index {
        None => {
            let lease = Lease::new(requested_port, pid, tag, session);
            upsert_lease(leases, lease.clone());
            Ok(ClaimOutcome {
                lease,
                reassigned: false,
            })
        }
        Some(index) if leases[index].pid == pid => {
            let lease = Lease::new(requested_port, pid, tag, session);
            upsert_lease(leases, lease.clone());
            Ok(ClaimOutcome {
                lease,
                reassigned: false,
            })
        }
        Some(index) if leases[index].is_alive(checker) => {
            let next_port = find_next_available_port(
                requested_port
                    .checked_add(1)
                    .context("no available port: requested port is already at the maximum")?,
                leases,
                checker,
            )
            .context("no available port found while resolving a port conflict")?;
            let lease = Lease::new(next_port, pid, tag, session);
            // `next_port` may carry a stale (dead) lease that made it look
            // "available" to find_next_available_port: upsert, don't push
            // blindly, or the dead entry and the new one both survive on the
            // same port.
            upsert_lease(leases, lease.clone());
            Ok(ClaimOutcome {
                lease,
                reassigned: true,
            })
        }
        Some(_) => {
            // Dead lease on the requested port: prune it and take over the port.
            let lease = Lease::new(requested_port, pid, tag, session);
            upsert_lease(leases, lease.clone());
            Ok(ClaimOutcome {
                lease,
                reassigned: false,
            })
        }
    }
}

/// Inserts or replaces the lease for `lease.port` in `leases`, so a given
/// port is never represented by more than one entry.
pub(crate) fn upsert_lease(leases: &mut Vec<Lease>, lease: Lease) {
    if let Some(index) = leases.iter().position(|l| l.port == lease.port) {
        leases[index] = lease;
    } else {
        leases.push(lease);
    }
}

/// Finds the first port at or after `start` that has no live lease in
/// `leases` and is not currently bound by the OS.
pub(crate) fn find_next_available_port(
    start: u16,
    leases: &[Lease],
    checker: &dyn PidChecker,
) -> Option<u16> {
    let mut candidate = start;
    loop {
        let has_live_lease = leases
            .iter()
            .any(|l| l.port == candidate && l.is_alive(checker));
        if !has_live_lease && is_os_port_free(candidate) {
            return Some(candidate);
        }
        candidate = candidate.checked_add(1)?;
    }
}

/// Probes whether a port is currently free by attempting to bind it.
pub(crate) fn is_os_port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Removes the lease on `port` from `leases`, if any, and reports whether
/// its owning PID was still alive at the time of removal.
pub(crate) fn release_in_place(
    leases: &mut Vec<Lease>,
    port: u16,
    checker: &dyn PidChecker,
) -> Option<ReleaseOutcome> {
    let index = leases.iter().position(|lease| lease.port == port)?;
    let lease = leases.remove(index);
    let was_alive = lease.is_alive(checker);
    Some(ReleaseOutcome { lease, was_alive })
}

/// Removes every lease in `leases` whose owning PID is dead, and returns the
/// removed leases in their original relative order.
pub(crate) fn prune_in_place(leases: &mut Vec<Lease>, checker: &dyn PidChecker) -> Vec<Lease> {
    let mut dead = Vec::new();
    leases.retain(|lease| {
        if lease.is_alive(checker) {
            true
        } else {
            dead.push(lease.clone());
            false
        }
    });
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::test_support::{AlivePids, AlwaysAlive, AlwaysDead};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    // ---- resolve_data_dir ----

    #[test]
    fn resolve_data_dir_prefers_explicit_override() {
        let resolved = Store::resolve_data_dir(
            Some(PathBuf::from("/override/dir")),
            Some("/env/dir".to_string()),
            Some("/xdg/dir".to_string()),
            Some("/home/dir".to_string()),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/override/dir"));
    }

    #[test]
    fn resolve_data_dir_uses_env_var_when_no_override() {
        let resolved = Store::resolve_data_dir(
            None,
            Some("/env/dir".to_string()),
            Some("/xdg/dir".to_string()),
            Some("/home/dir".to_string()),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/env/dir"));
    }

    #[test]
    fn resolve_data_dir_uses_xdg_data_home_and_appends_portzilla() {
        let resolved =
            Store::resolve_data_dir(None, None, Some("/xdg/dir".to_string()), Some("/home/dir".to_string()))
                .unwrap();
        assert_eq!(resolved, PathBuf::from("/xdg/dir/portzilla"));
    }

    #[test]
    fn resolve_data_dir_falls_back_to_home_local_share() {
        let resolved = Store::resolve_data_dir(None, None, None, Some("/home/dir".to_string())).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/dir/.local/share/portzilla"));
    }

    #[test]
    fn resolve_data_dir_errors_when_nothing_is_available() {
        let result = Store::resolve_data_dir(None, None, None, None);
        assert!(result.is_err());
    }

    // ---- persistence ----

    #[test]
    fn missing_state_file_reads_as_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let leases = store.list().unwrap();
        assert!(leases.is_empty());
    }

    #[test]
    fn corrupt_state_file_returns_a_clear_error_instead_of_crashing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let result = store.list();
        assert!(result.is_err());
    }

    #[test]
    fn claim_persists_and_is_visible_from_a_new_store_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store_a
            .claim(4000, 111, "svc".to_string(), None, &AlwaysAlive)
            .unwrap();

        let store_b = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let leases = store_b.list().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].port, 4000);
        assert_eq!(leases[0].pid, 111);
    }

    #[test]
    fn concurrent_claims_are_serialized_by_the_file_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Some(dir.path().to_path_buf())).unwrap());
        let in_critical_section = Arc::new(AtomicBool::new(false));
        let overlap_detected = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for i in 0..4u32 {
            let store = Arc::clone(&store);
            let in_critical_section = Arc::clone(&in_critical_section);
            let overlap_detected = Arc::clone(&overlap_detected);
            handles.push(thread::spawn(move || {
                store
                    .claim_with_hook(5000 + i as u16, 200 + i, "svc".to_string(), None, &AlwaysAlive, || {
                        if in_critical_section.swap(true, Ordering::SeqCst) {
                            overlap_detected.store(true, Ordering::SeqCst);
                        }
                        thread::sleep(Duration::from_millis(20));
                        in_critical_section.store(false, Ordering::SeqCst);
                    })
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert!(!overlap_detected.load(Ordering::SeqCst));
    }

    // ---- is_os_port_free ----

    #[test]
    fn is_os_port_free_reports_bound_port_as_taken() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(!is_os_port_free(port));
        drop(listener);
        assert!(is_os_port_free(port));
    }

    // ---- find_next_available_port ----

    // These tests use fixed, widely-spaced high ports (rather than OS-assigned
    // ephemeral ports) so they stay deterministic under parallel test execution:
    // ephemeral ports are drawn from a shared pool that other concurrently
    // running tests (and unrelated sockets) also draw from, which previously
    // caused flaky collisions right after a probe listener was dropped.

    #[test]
    fn find_next_available_port_returns_start_when_nothing_blocks_it() {
        let start: u16 = 24000;
        let leases = vec![];
        let found = find_next_available_port(start, &leases, &AlwaysAlive).unwrap();
        assert_eq!(found, start);
    }

    #[test]
    fn find_next_available_port_skips_ports_with_a_live_lease() {
        let start: u16 = 24100;
        let leases = vec![Lease::new(start, 999, "blocker", None)];
        let found = find_next_available_port(start, &leases, &AlwaysAlive).unwrap();
        assert_eq!(found, start + 1);
    }

    #[test]
    fn find_next_available_port_reuses_ports_with_a_dead_lease() {
        let start: u16 = 24200;
        let leases = vec![Lease::new(start, 999, "stale", None)];
        let found = find_next_available_port(start, &leases, &AlwaysDead).unwrap();
        assert_eq!(found, start);
    }

    #[test]
    fn find_next_available_port_skips_ports_actually_bound_by_the_os() {
        let start: u16 = 24300;
        let listener = TcpListener::bind(("127.0.0.1", start)).unwrap();
        // No lease recorded for `start`, but the OS has it bound.
        let leases = vec![];
        let found = find_next_available_port(start, &leases, &AlwaysAlive).unwrap();
        assert_eq!(found, start + 1);
        drop(listener);
    }

    #[test]
    fn find_next_available_port_returns_none_on_overflow() {
        let leases = vec![Lease::new(u16::MAX, 999, "blocker", None)];
        let found = find_next_available_port(u16::MAX, &leases, &AlwaysAlive);
        assert_eq!(found, None);
    }

    // ---- claim_in_place ----

    #[test]
    fn claim_free_port_creates_a_lease() {
        let mut leases = vec![];
        let outcome =
            claim_in_place(&mut leases, 3000, 100, "server".to_string(), None, &AlwaysAlive).unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(outcome.lease.port, 3000);
        assert_eq!(outcome.lease.pid, 100);
        assert_eq!(leases.len(), 1);
    }

    #[test]
    fn claim_replaces_a_dead_lease_on_the_same_port() {
        let mut leases = vec![Lease::new(3000, 999, "stale-owner", None)];
        let outcome =
            claim_in_place(&mut leases, 3000, 100, "server".to_string(), None, &AlwaysDead).unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(outcome.lease.port, 3000);
        assert_eq!(outcome.lease.pid, 100);
        assert_eq!(leases.len(), 1, "dead lease should be replaced, not duplicated");
    }

    #[test]
    fn claim_conflicting_with_a_live_lease_reassigns_to_next_port() {
        // Dedicated port range so this test cannot collide with other tests
        // that also probe real OS ports concurrently.
        let base: u16 = 21000;
        let mut leases = vec![Lease::new(base, 999, "other-owner", None)];
        let checker = AlivePids(vec![999]);
        let outcome =
            claim_in_place(&mut leases, base, 100, "server".to_string(), None, &checker).unwrap();

        assert!(outcome.reassigned);
        assert_eq!(outcome.lease.port, base + 1);
        assert_eq!(outcome.lease.pid, 100);
        // Original lease on the requested port is left untouched for its live owner.
        assert!(leases.iter().any(|l| l.port == base && l.pid == 999));
        assert!(leases.iter().any(|l| l.port == base + 1 && l.pid == 100));
    }

    #[test]
    fn claim_same_pid_reclaiming_its_own_port_is_idempotent() {
        let mut leases = vec![Lease::new(3000, 100, "server".to_string(), None)];
        let outcome = claim_in_place(
            &mut leases,
            3000,
            100,
            "server-renamed".to_string(),
            Some("session-x".to_string()),
            &AlwaysAlive,
        )
        .unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(leases.len(), 1, "re-claiming should update, not duplicate");
        assert_eq!(leases[0].tag, "server-renamed");
        assert_eq!(leases[0].session.as_deref(), Some("session-x"));
    }

    #[test]
    fn claim_reassignment_skips_both_live_leased_and_os_bound_ports() {
        // Dedicated port range, isolated from other tests that also probe real
        // OS ports concurrently.
        let base: u16 = 22000;
        // base:     live lease owned by someone else (conflict, triggers scan).
        // base + 1: no lease, but OS has it bound.
        // base + 2: free -> should be picked.
        let listener = TcpListener::bind(("127.0.0.1", base + 1)).unwrap();

        let mut leases = vec![Lease::new(base, 999, "other-owner", None)];
        let checker = AlivePids(vec![999]);
        let outcome =
            claim_in_place(&mut leases, base, 100, "server".to_string(), None, &checker).unwrap();

        drop(listener);

        assert!(outcome.reassigned);
        assert_eq!(outcome.lease.port, base + 2);
    }

    #[test]
    fn claim_reassignment_reuses_a_port_whose_lease_is_dead() {
        // Dedicated port range, isolated from other tests that also probe real
        // OS ports concurrently.
        let base: u16 = 23000;
        // base:     live lease owned by someone else (conflict).
        // base + 1: dead lease -> should be reused.
        let mut leases = vec![
            Lease::new(base, 999, "other-owner", None),
            Lease::new(base + 1, 998, "stale", None),
        ];
        let checker = AlivePids(vec![999]);
        let outcome =
            claim_in_place(&mut leases, base, 100, "server".to_string(), None, &checker).unwrap();

        assert!(outcome.reassigned);
        assert_eq!(outcome.lease.port, base + 1);
        assert_eq!(outcome.lease.pid, 100);

        // Regression guard: reassigning onto a port that had a stale (dead)
        // lease must upsert, not leave the old dead entry AND a new one
        // sitting on the same port. Exactly one entry per port, and the
        // survivor on base+1 must be the new claim, not the stale one.
        assert_eq!(leases.len(), 2, "must not duplicate the lease on the reused port");
        let ports: std::collections::HashSet<u16> = leases.iter().map(|l| l.port).collect();
        assert_eq!(ports.len(), leases.len(), "no two leases may share a port");
        let survivor = leases.iter().find(|l| l.port == base + 1).unwrap();
        assert_eq!(survivor.pid, 100, "the new owner must replace the stale lease");
        assert_eq!(survivor.tag, "server");
    }

    // ---- release_in_place ----

    #[test]
    fn release_in_place_removes_a_live_lease_and_reports_it_was_alive() {
        let mut leases = vec![Lease::new(3000, 100, "server", None)];
        let outcome = release_in_place(&mut leases, 3000, &AlwaysAlive).unwrap();

        assert_eq!(outcome.lease.port, 3000);
        assert_eq!(outcome.lease.pid, 100);
        assert!(outcome.was_alive);
        assert!(leases.is_empty());
    }

    #[test]
    fn release_in_place_removes_a_dead_lease_and_reports_it_was_dead() {
        let mut leases = vec![Lease::new(3000, 100, "server", None)];
        let outcome = release_in_place(&mut leases, 3000, &AlwaysDead).unwrap();

        assert!(!outcome.was_alive);
        assert!(leases.is_empty());
    }

    #[test]
    fn release_in_place_returns_none_for_a_port_with_no_lease() {
        let mut leases = vec![Lease::new(3000, 100, "server", None)];
        let outcome = release_in_place(&mut leases, 4000, &AlwaysAlive);

        assert!(outcome.is_none());
        assert_eq!(leases.len(), 1, "unrelated leases must be left untouched");
    }

    #[test]
    fn release_in_place_only_removes_the_targeted_port() {
        let mut leases = vec![
            Lease::new(3000, 100, "a", None),
            Lease::new(3001, 101, "b", None),
        ];
        release_in_place(&mut leases, 3000, &AlwaysAlive);

        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].port, 3001);
    }

    // ---- prune_in_place ----

    #[test]
    fn prune_in_place_removes_only_dead_leases_and_returns_them() {
        let mut leases = vec![
            Lease::new(3000, 100, "alive-one", None),
            Lease::new(3001, 200, "dead-one", None),
            Lease::new(3002, 201, "dead-two", None),
        ];
        let checker = AlivePids(vec![100]);

        let pruned = prune_in_place(&mut leases, &checker);

        assert_eq!(pruned.len(), 2);
        assert!(pruned.iter().any(|l| l.port == 3001));
        assert!(pruned.iter().any(|l| l.port == 3002));
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].port, 3000);
    }

    #[test]
    fn prune_in_place_with_nothing_dead_returns_empty_and_leaves_list_untouched() {
        let mut leases = vec![
            Lease::new(3000, 100, "alive-one", None),
            Lease::new(3001, 101, "alive-two", None),
        ];
        let pruned = prune_in_place(&mut leases, &AlwaysAlive);

        assert!(pruned.is_empty());
        assert_eq!(leases.len(), 2);
    }

    #[test]
    fn prune_in_place_with_empty_list_returns_empty() {
        let mut leases: Vec<Lease> = vec![];
        let pruned = prune_in_place(&mut leases, &AlwaysAlive);
        assert!(pruned.is_empty());
    }

    // ---- Store::get / release / prune ----

    #[test]
    fn store_get_returns_none_for_a_port_with_no_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        assert!(store.get(3000).unwrap().is_none());
    }

    #[test]
    fn store_get_returns_the_matching_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(3000, 100, "server".to_string(), None, &AlwaysAlive)
            .unwrap();

        let lease = store.get(3000).unwrap().unwrap();
        assert_eq!(lease.port, 3000);
        assert_eq!(lease.pid, 100);
    }

    #[test]
    fn store_release_persists_the_removal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(3000, 100, "server".to_string(), None, &AlwaysAlive)
            .unwrap();

        let outcome = store.release(3000, &AlwaysAlive).unwrap().unwrap();
        assert_eq!(outcome.lease.port, 3000);
        assert!(outcome.was_alive);
        assert!(store.get(3000).unwrap().is_none());
    }

    #[test]
    fn store_release_returns_none_for_a_port_with_no_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        assert!(store.release(3000, &AlwaysAlive).unwrap().is_none());
    }

    #[test]
    fn store_prune_persists_removal_of_dead_leases_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(3000, 100, "alive".to_string(), None, &AlivePids(vec![100]))
            .unwrap();
        store
            .claim(3001, 200, "dead".to_string(), None, &AlivePids(vec![100]))
            .unwrap();

        let pruned = store.prune(&AlivePids(vec![100])).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].port, 3001);

        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].port, 3000);
    }

    #[test]
    fn store_prune_with_nothing_dead_returns_empty_and_exits_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(3000, 100, "alive".to_string(), None, &AlwaysAlive)
            .unwrap();

        let pruned = store.prune(&AlwaysAlive).unwrap();
        assert!(pruned.is_empty());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    // ---- Unix permissions ----

    #[cfg(unix)]
    #[test]
    fn claim_leaves_the_data_dir_and_state_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(3000, 100, "server".to_string(), None, &AlwaysAlive)
            .unwrap();

        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "data dir must be owner-only (0700)");

        let file_mode = fs::metadata(store.state_file_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "leases.json must be owner-only (0600)");

        let lock_mode = fs::metadata(dir.path().join("leases.json.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600, "lock file must be owner-only (0600)");
    }
}
