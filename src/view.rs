//! Shared JSON view types for lease data.
//!
//! Both the CLI's `--json` output and the MCP server's tool results need to
//! emit the exact same flat JSON shape for a lease (and for a claim
//! outcome). Rather than defining that shape twice and risking drift between
//! the two surfaces, both go through these types.

use crate::lease::{Lease, PidChecker, current_unix_timestamp};
use crate::store::ClaimOutcome;
use serde::Serialize;

/// Flat view of a lease: the shape used by `ls`, `who`, `release`, `prune`
/// (as a single object for `who`/`release`, as an array for `ls`/`prune`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeaseView {
    pub port: u16,
    pub pid: u32,
    pub tag: String,
    pub created_at: u64,
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_start_time: Option<u64>,
    pub age_secs: u64,
    pub alive: bool,
}

/// Builds a [`LeaseView`] for `lease`, computing `age_secs` and `alive` at
/// the current moment using `checker` for the liveness check.
pub fn to_view(lease: &Lease, checker: &dyn PidChecker) -> LeaseView {
    let now = current_unix_timestamp();
    LeaseView {
        port: lease.port,
        pid: lease.pid,
        tag: lease.tag.clone(),
        created_at: lease.created_at,
        session: lease.session.clone(),
        process_start_time: lease.process_start_time,
        age_secs: now.saturating_sub(lease.created_at),
        alive: lease.is_alive(checker),
    }
}

/// Flat view of a `claim` outcome: the shape used by `claim --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimView {
    pub port: u16,
    pub pid: u32,
    pub tag: String,
    pub created_at: u64,
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_start_time: Option<u64>,
    pub requested_port: u16,
    pub reassigned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reassignment_reason: Option<crate::store::ReassignmentReason>,
}

/// Builds a [`ClaimView`] for `outcome`, given the port that was originally
/// requested (which may differ from `outcome.lease.port` when reassigned).
pub fn to_claim_view(outcome: &ClaimOutcome, requested_port: u16) -> ClaimView {
    ClaimView {
        port: outcome.lease.port,
        pid: outcome.lease.pid,
        tag: outcome.lease.tag.clone(),
        created_at: outcome.lease.created_at,
        session: outcome.lease.session.clone(),
        process_start_time: outcome.lease.process_start_time,
        requested_port,
        reassigned: outcome.reassigned,
        reassignment_reason: outcome.reassignment_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::test_support::{AlwaysAlive, AlwaysDead};

    #[test]
    fn to_view_carries_lease_fields_and_computes_age_and_liveness() {
        let lease = Lease::new(3000, 100, "server", Some("sess-1".to_string()));

        let alive_view = to_view(&lease, &AlwaysAlive);
        assert_eq!(alive_view.port, 3000);
        assert_eq!(alive_view.pid, 100);
        assert_eq!(alive_view.tag, "server");
        assert_eq!(alive_view.session.as_deref(), Some("sess-1"));
        assert_eq!(alive_view.created_at, lease.created_at);
        assert!(alive_view.alive);
        // Freshly created, so age must be tiny (not a hardcoded 0, to avoid a
        // flaky test if the clock ticks over a second boundary mid-run).
        assert!(alive_view.age_secs < 5);

        let dead_view = to_view(&lease, &AlwaysDead);
        assert!(!dead_view.alive);
    }

    #[test]
    fn to_view_serializes_to_the_documented_flat_shape() {
        let lease = Lease::new(3000, 100, "server", None);
        let view = to_view(&lease, &AlwaysAlive);

        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        let keys: std::collections::HashSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::HashSet<&str> = [
            "port",
            "pid",
            "tag",
            "created_at",
            "session",
            "age_secs",
            "alive",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn to_view_exposes_process_start_time_when_recorded() {
        let lease = Lease::new_with_process_start_time(3000, 100, "server", None, Some(123));
        let view = to_view(&lease, &AlwaysAlive);

        assert_eq!(view.process_start_time, Some(123));
        assert_eq!(
            serde_json::to_value(view).unwrap()["process_start_time"],
            123
        );
    }

    #[test]
    fn to_claim_view_reports_the_requested_port_and_reassignment_flag() {
        let outcome = ClaimOutcome {
            lease: Lease::new(3001, 100, "server", None),
            reassigned: true,
            reassignment_reason: Some(crate::store::ReassignmentReason::LeaseConflict),
        };

        let view = to_claim_view(&outcome, 3000);
        assert_eq!(view.requested_port, 3000);
        assert_eq!(view.port, 3001);
        assert!(view.reassigned);
        assert_eq!(
            view.reassignment_reason,
            Some(crate::store::ReassignmentReason::LeaseConflict)
        );
    }

    #[test]
    fn to_claim_view_serializes_to_the_documented_flat_shape() {
        let outcome = ClaimOutcome {
            lease: Lease::new(3000, 100, "server", None),
            reassigned: false,
            reassignment_reason: None,
        };
        let view = to_claim_view(&outcome, 3000);

        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        let keys: std::collections::HashSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::HashSet<&str> = [
            "port",
            "pid",
            "tag",
            "created_at",
            "session",
            "requested_port",
            "reassigned",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn to_claim_view_exposes_process_start_time_when_recorded() {
        let outcome = ClaimOutcome {
            lease: Lease::new_with_process_start_time(3000, 100, "server", None, Some(123)),
            reassigned: false,
            reassignment_reason: None,
        };
        let view = to_claim_view(&outcome, 3000);

        assert_eq!(view.process_start_time, Some(123));
        assert_eq!(
            serde_json::to_value(view).unwrap()["process_start_time"],
            123
        );
    }
}
