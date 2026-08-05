//! Domain model for a port lease: which process claimed a port, why, and since when.

use serde::{Deserialize, Serialize};

/// A lease recorded by a process on a specific local port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub port: u16,
    pub pid: u32,
    pub tag: String,
    /// Unix timestamp (seconds since epoch) when the lease was created or last renewed.
    pub created_at: u64,
    pub session: Option<String>,
}

impl Lease {
    /// Creates a new lease with `created_at` set to the current time.
    pub fn new(port: u16, pid: u32, tag: impl Into<String>, session: Option<String>) -> Self {
        Self {
            port,
            pid,
            tag: tag.into(),
            created_at: current_unix_timestamp(),
            session,
        }
    }

    /// Returns `true` if the lease's owning process is currently alive,
    /// according to the given liveness checker.
    pub fn is_alive(&self, checker: &dyn PidChecker) -> bool {
        checker.is_alive(self.pid)
    }
}

/// Returns the current time as seconds since the Unix epoch, using only `std`.
pub fn current_unix_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

/// Abstraction over "is this PID alive?" so callers can substitute a fake
/// implementation in tests instead of depending on the real process table.
pub trait PidChecker {
    fn is_alive(&self, pid: u32) -> bool;
}

/// Real liveness checker backed by `sysinfo`'s process table.
pub struct SystemPidChecker;

impl PidChecker for SystemPidChecker {
    fn is_alive(&self, pid: u32) -> bool {
        use sysinfo::{Pid, ProcessesToUpdate, System};

        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
        system.process(Pid::from_u32(pid)).is_some()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::PidChecker;

    /// Test fixture: every PID is reported alive.
    pub struct AlwaysAlive;
    impl PidChecker for AlwaysAlive {
        fn is_alive(&self, _pid: u32) -> bool {
            true
        }
    }

    /// Test fixture: every PID is reported dead.
    pub struct AlwaysDead;
    impl PidChecker for AlwaysDead {
        fn is_alive(&self, _pid: u32) -> bool {
            false
        }
    }

    /// Test fixture: only the given PIDs are reported alive, everything else is dead.
    pub struct AlivePids(pub Vec<u32>);
    impl PidChecker for AlivePids {
        fn is_alive(&self, pid: u32) -> bool {
            self.0.contains(&pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_lease_carries_given_fields() {
        let lease = Lease::new(3000, 1234, "my-server", Some("session-a".to_string()));

        assert_eq!(lease.port, 3000);
        assert_eq!(lease.pid, 1234);
        assert_eq!(lease.tag, "my-server");
        assert_eq!(lease.session.as_deref(), Some("session-a"));
    }

    #[test]
    fn new_lease_has_recent_created_at() {
        let before = current_unix_timestamp();
        let lease = Lease::new(3000, 1234, "tag", None);
        let after = current_unix_timestamp();

        assert!(lease.created_at >= before && lease.created_at <= after);
    }

    #[test]
    fn lease_is_alive_when_checker_says_so() {
        let lease = Lease::new(3000, 1234, "tag", None);
        assert!(lease.is_alive(&test_support::AlwaysAlive));
        assert!(!lease.is_alive(&test_support::AlwaysDead));
    }

    #[test]
    fn own_process_pid_is_reported_alive_by_system_checker() {
        let own_pid = std::process::id();
        let lease = Lease::new(3000, own_pid, "tag", None);
        assert!(lease.is_alive(&SystemPidChecker));
    }

    #[test]
    fn wildly_high_pid_is_reported_dead_by_system_checker() {
        // Not a guaranteed invariant on every OS, but a PID this large is not
        // going to be a real running process on any supported platform.
        let lease = Lease::new(3000, 4_000_000_000, "tag", None);
        assert!(!lease.is_alive(&SystemPidChecker));
    }

    #[test]
    fn lease_round_trips_through_json() {
        let lease = Lease::new(8080, 42, "web", Some("agent-1".to_string()));
        let json = serde_json::to_string(&lease).unwrap();
        let restored: Lease = serde_json::from_str(&json).unwrap();
        assert_eq!(lease, restored);
    }
}
