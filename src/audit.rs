use crate::lease::Lease;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditEvent {
    pub(crate) sequence: u64,
    pub(crate) occurred_at: u64,
    pub(crate) actor: AuditActor,
    #[serde(flatten)]
    pub(crate) kind: AuditEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditEventDraft {
    pub(crate) actor: AuditActor,
    pub(crate) kind: AuditEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditActor {
    pub(crate) source: AuditSource,
    pub(crate) harness: Option<AuditHarness>,
    pub(crate) session: Option<String>,
}

impl AuditActor {
    pub(crate) fn new(
        source: AuditSource,
        harness: Option<AuditHarness>,
        session: Option<String>,
    ) -> Self {
        Self {
            source,
            harness,
            session: session.filter(|value| !value.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditSource {
    Cli,
    Mcp,
    Run,
    Watch,
    Guard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditHarness {
    #[serde(rename = "opencode")]
    OpenCode,
    ClaudeCode,
    Cursor,
    Gemini,
    Codex,
    Kimi,
    Windsurf,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseSnapshot {
    pub(crate) port: u16,
    pub(crate) pid: u32,
    pub(crate) tag: String,
    pub(crate) created_at: u64,
    pub(crate) session: Option<String>,
    pub(crate) process_start_time: Option<u64>,
}

impl From<&Lease> for LeaseSnapshot {
    fn from(lease: &Lease) -> Self {
        Self {
            port: lease.port,
            pid: lease.pid,
            tag: lease.tag.clone(),
            created_at: lease.created_at,
            session: lease.session.clone(),
            process_start_time: lease.process_start_time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClaimDisposition {
    Created,
    Updated,
    ReplacedDead,
    ReassignedLeaseConflict,
    ReassignedOsOccupied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum ProcessExitOutcome {
    Code(i32),
    Signal(i32),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum GuardTarget {
    Pid { pid: u32 },
    Port { port: u16 },
    ProcessName { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuardDenyReason {
    ForeignLiveLease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuardWarnReason {
    UnresolvableProcessName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub(crate) enum AuditEventKind {
    LeaseClaimed {
        requested_port: u16,
        disposition: ClaimDisposition,
        lease: LeaseSnapshot,
        prior_lease: Option<LeaseSnapshot>,
        replaced_lease: Option<LeaseSnapshot>,
    },
    LeaseTransferred {
        wrapper_pid: u32,
        wrapper_start_time: u64,
        lease: LeaseSnapshot,
    },
    LeaseReleased {
        lease: LeaseSnapshot,
        was_alive: bool,
    },
    LeasePruned {
        lease: LeaseSnapshot,
    },
    ProcessExited {
        lease: LeaseSnapshot,
        outcome: ProcessExitOutcome,
    },
    GuardDenied {
        target: GuardTarget,
        reason: GuardDenyReason,
        lease: LeaseSnapshot,
    },
    GuardWarned {
        target: GuardTarget,
        reason: GuardWarnReason,
    },
    HistoryCleared {
        removed_count: u64,
    },
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditEventType {
    #[value(name = "lease_claimed")]
    LeaseClaimed,
    #[value(name = "lease_transferred")]
    LeaseTransferred,
    #[value(name = "lease_released")]
    LeaseReleased,
    #[value(name = "lease_pruned")]
    LeasePruned,
    #[value(name = "process_exited")]
    ProcessExited,
    #[value(name = "guard_denied")]
    GuardDenied,
    #[value(name = "guard_warned")]
    GuardWarned,
    #[value(name = "history_cleared")]
    HistoryCleared,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Lease;

    fn snapshot() -> LeaseSnapshot {
        LeaseSnapshot::from(&Lease::new(
            3000,
            1234,
            "dev-server",
            Some("owner-session".into()),
        ))
    }

    fn event_kind_variants() -> Vec<AuditEventKind> {
        let lease = snapshot();
        vec![
            AuditEventKind::LeaseClaimed {
                requested_port: 3000,
                disposition: ClaimDisposition::Created,
                lease: lease.clone(),
                prior_lease: None,
                replaced_lease: None,
            },
            AuditEventKind::LeaseTransferred {
                wrapper_pid: 1234,
                wrapper_start_time: 1,
                lease: lease.clone(),
            },
            AuditEventKind::LeaseReleased {
                lease: lease.clone(),
                was_alive: true,
            },
            AuditEventKind::LeasePruned {
                lease: lease.clone(),
            },
            AuditEventKind::ProcessExited {
                lease: lease.clone(),
                outcome: ProcessExitOutcome::Code(0),
            },
            AuditEventKind::GuardDenied {
                target: GuardTarget::Pid { pid: 1234 },
                reason: GuardDenyReason::ForeignLiveLease,
                lease,
            },
            AuditEventKind::GuardWarned {
                target: GuardTarget::ProcessName {
                    name: "node".into(),
                },
                reason: GuardWarnReason::UnresolvableProcessName,
            },
            AuditEventKind::HistoryCleared { removed_count: 3 },
        ]
    }

    #[test]
    fn audit_event_serializes_with_tagged_event_and_data() {
        let event = AuditEvent {
            sequence: 7,
            occurred_at: 100,
            actor: AuditActor::new(AuditSource::Run, None, Some("session-a".into())),
            kind: AuditEventKind::HistoryCleared { removed_count: 3 },
        };

        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["sequence"], 7);
        assert_eq!(value["event"], "history_cleared");
        assert_eq!(value["data"]["removed_count"], 3);
        assert!(value.get("command").is_none());
    }

    #[test]
    fn opencode_harness_has_stable_wire_name() {
        assert_eq!(
            serde_json::to_value(AuditHarness::OpenCode).unwrap(),
            "opencode"
        );
    }

    #[test]
    fn all_audit_event_variants_round_trip() {
        for kind in event_kind_variants() {
            let event = AuditEvent {
                sequence: 1,
                occurred_at: 100,
                actor: AuditActor::new(AuditSource::Cli, Some(AuditHarness::Generic), None),
                kind,
            };
            let encoded = serde_json::to_value(&event).unwrap();
            let decoded: AuditEvent = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn all_actor_sources_and_harnesses_round_trip() {
        let sources = [
            AuditSource::Cli,
            AuditSource::Mcp,
            AuditSource::Run,
            AuditSource::Watch,
            AuditSource::Guard,
        ];
        let harnesses = [
            AuditHarness::OpenCode,
            AuditHarness::ClaudeCode,
            AuditHarness::Cursor,
            AuditHarness::Gemini,
            AuditHarness::Codex,
            AuditHarness::Kimi,
            AuditHarness::Windsurf,
            AuditHarness::Generic,
        ];
        for source in sources {
            let actor = AuditActor::new(source, None, Some("session".into()));
            let encoded = serde_json::to_value(&actor).unwrap();
            assert_eq!(
                serde_json::from_value::<AuditActor>(encoded).unwrap(),
                actor
            );
        }
        for harness in harnesses {
            let encoded = serde_json::to_value(harness).unwrap();
            assert_eq!(
                serde_json::from_value::<AuditHarness>(encoded).unwrap(),
                harness
            );
        }
    }

    #[test]
    fn empty_actor_session_normalizes_to_none() {
        assert_eq!(
            AuditActor::new(AuditSource::Cli, None, Some(String::new())).session,
            None
        );
    }

    #[test]
    fn lease_snapshot_omits_internal_identity_marker() {
        let value = serde_json::to_value(snapshot()).unwrap();
        assert!(value.get("process_identity_verified").is_none());
        assert_eq!(value["session"], "owner-session");
    }

    #[test]
    fn guard_targets_are_structured_and_have_no_raw_command_field() {
        let value = serde_json::to_value(GuardTarget::Port { port: 3000 }).unwrap();
        assert_eq!(value["kind"], "port");
        assert_eq!(value["port"], 3000);
        assert!(value.get("command").is_none());
    }
}
