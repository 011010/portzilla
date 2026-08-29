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
    /// Start time is reported by `sysinfo` as Unix epoch seconds. This
    /// second-level resolution improves PID reuse detection but cannot make
    /// PID identity perfect when a PID is recycled within the same second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_identity_verified: Option<bool>,
}

impl Lease {
    /// Creates a new lease with `created_at` set to the current time.
    ///
    /// This constructor intentionally leaves process identity absent, making
    /// it useful for synthetic leases in tests and for legacy data fixtures.
    #[allow(dead_code)]
    pub fn new(port: u16, pid: u32, tag: impl Into<String>, session: Option<String>) -> Self {
        let mut lease = Self::new_with_process_start_time(port, pid, tag, session, None);
        lease.process_identity_verified = None;
        lease
    }

    /// Creates a new lease with a process identity resolved by a [`PidChecker`].
    pub fn new_with_process_start_time(
        port: u16,
        pid: u32,
        tag: impl Into<String>,
        session: Option<String>,
        process_start_time: Option<u64>,
    ) -> Self {
        Self {
            port,
            pid,
            tag: tag.into(),
            created_at: current_unix_timestamp(),
            session,
            process_start_time,
            process_identity_verified: Some(process_start_time.is_some()),
        }
    }

    /// Returns `true` if the lease's owning process is currently alive,
    /// according to the given liveness checker.
    pub fn is_alive(&self, checker: &dyn PidChecker) -> bool {
        if !checker.is_alive(self.pid) {
            return false;
        }

        match (self.process_identity_verified, self.process_start_time) {
            (None, None) => true,
            (Some(_), None) => false,
            (_, Some(expected)) => checker.process_start_time(self.pid) == Some(expected),
        }
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

    /// Returns the process start time, or `None` when the checker cannot
    /// provide process identity for this PID. Recorded leases require a
    /// matching `Some` value; legacy leases without one remain PID-only.
    fn process_start_time(&self, _pid: u32) -> Option<u64> {
        None
    }
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

    fn process_start_time(&self, pid: u32) -> Option<u64> {
        use sysinfo::{Pid, ProcessesToUpdate, System};

        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
        system
            .process(Pid::from_u32(pid))
            .map(|process| process.start_time())
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

        fn process_start_time(&self, _pid: u32) -> Option<u64> {
            Some(0)
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

        fn process_start_time(&self, pid: u32) -> Option<u64> {
            self.0.contains(&pid).then_some(0)
        }
    }

    /// Test fixture: every PID is reported alive, but no process identity is resolved.
    pub struct AliveWithoutIdentity;
    impl PidChecker for AliveWithoutIdentity {
        fn is_alive(&self, _pid: u32) -> bool {
            true
        }
    }

    pub struct ProcessIdentity {
        pub pid: u32,
        pub start_time: Option<u64>,
    }

    impl PidChecker for ProcessIdentity {
        fn is_alive(&self, pid: u32) -> bool {
            self.pid == pid
        }

        fn process_start_time(&self, pid: u32) -> Option<u64> {
            (self.pid == pid).then_some(self.start_time).flatten()
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
    fn lease_constructor_can_carry_captured_process_identity() {
        let lease = Lease::new_with_process_start_time(3000, 1234, "tag", None, Some(123));

        assert_eq!(lease.process_start_time, Some(123));
    }

    #[test]
    fn lease_is_alive_when_checker_says_so() {
        let lease = Lease::new(3000, 1234, "tag", None);
        assert!(lease.is_alive(&test_support::AlwaysAlive));
        assert!(!lease.is_alive(&test_support::AlwaysDead));
    }

    #[test]
    fn lease_is_alive_when_process_identity_matches() {
        let lease = Lease {
            process_start_time: Some(123),
            ..Lease::new(3000, 1234, "tag", None)
        };

        assert!(lease.is_alive(&test_support::ProcessIdentity {
            pid: 1234,
            start_time: Some(123),
        }));
    }

    #[test]
    fn lease_is_not_alive_when_process_identity_mismatches() {
        let lease = Lease {
            process_start_time: Some(123),
            ..Lease::new(3000, 1234, "tag", None)
        };

        assert!(!lease.is_alive(&test_support::ProcessIdentity {
            pid: 1234,
            start_time: Some(456),
        }));
    }

    #[test]
    fn legacy_lease_uses_pid_only_when_process_identity_is_unavailable() {
        let lease = Lease::new(3000, 1234, "tag", None);

        assert!(lease.is_alive(&test_support::ProcessIdentity {
            pid: 1234,
            start_time: None,
        }));
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
        let lease = Lease::new_with_process_start_time(
            8080,
            42,
            "web",
            Some("agent-1".to_string()),
            Some(123),
        );
        let json = serde_json::to_string(&lease).unwrap();
        let restored: Lease = serde_json::from_str(&json).unwrap();
        assert_eq!(lease, restored);
        assert!(json.contains(r#""process_start_time":123"#));
    }

    #[test]
    fn lease_deserializes_without_process_start_time() {
        let lease: Lease = serde_json::from_str(
            r#"{"port":8080,"pid":42,"tag":"web","created_at":1,"session":null}"#,
        )
        .unwrap();

        assert_eq!(lease.process_start_time, None);
    }

    #[test]
    fn lease_without_process_identity_omits_the_field_from_json() {
        let lease = Lease::new(8080, 42, "web", None);
        let json = serde_json::to_string(&lease).unwrap();

        assert!(!json.contains("process_start_time"));
    }

    #[test]
    fn recorded_lease_is_dead_when_checker_cannot_report_identity() {
        let lease = Lease::new_with_process_start_time(3000, 1234, "tag", None, Some(123));

        assert!(!lease.is_alive(&test_support::AliveWithoutIdentity));
    }

    #[test]
    fn newly_created_lease_without_identity_is_not_alive() {
        let lease = Lease::new_with_process_start_time(3000, 1234, "tag", None, None);

        assert!(!lease.is_alive(&test_support::AliveWithoutIdentity));
    }

    #[test]
    fn legacy_lease_without_identity_marker_keeps_pid_only_behavior() {
        let lease: Lease = serde_json::from_str(
            r#"{"port":3000,"pid":1234,"tag":"tag","created_at":1,"session":null}"#,
        )
        .unwrap();

        assert!(lease.is_alive(&test_support::AlwaysAlive));
    }
}
