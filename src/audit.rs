use crate::lease::Lease;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_AUDIT_TARGET_CHARS: usize = 512;
pub(crate) const DEFAULT_HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditEvent {
    pub(crate) sequence: u64,
    pub(crate) occurred_at: u64,
    pub(crate) actor: AuditActor,
    #[serde(flatten)]
    pub(crate) kind: AuditEventKind,
}

#[allow(dead_code)]
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
    #[allow(dead_code)]
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

    pub(crate) fn validate(&self) -> Result<()> {
        validate_optional_string(self.session.as_deref(), "actor session")
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

impl LeaseSnapshot {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.tag.chars().count() > crate::store::MAX_TAG_CHARS {
            bail!("audit lease tag exceeds the maximum length");
        }
        validate_optional_string(self.session.as_deref(), "lease session")
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

impl GuardTarget {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Self::ProcessName { name } = self
            && (name.is_empty() || name.chars().count() > MAX_AUDIT_TARGET_CHARS)
        {
            bail!("audit process name must be between 1 and {MAX_AUDIT_TARGET_CHARS} characters");
        }
        Ok(())
    }
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

impl AuditEventType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LeaseClaimed => "lease_claimed",
            Self::LeaseTransferred => "lease_transferred",
            Self::LeaseReleased => "lease_released",
            Self::LeasePruned => "lease_pruned",
            Self::ProcessExited => "process_exited",
            Self::GuardDenied => "guard_denied",
            Self::GuardWarned => "guard_warned",
            Self::HistoryCleared => "history_cleared",
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryQuery {
    pub(crate) session: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) event: Option<AuditEventType>,
    pub(crate) before: Option<u64>,
    pub(crate) limit: usize,
}

#[allow(dead_code)]
impl HistoryQuery {
    pub(crate) fn try_new(
        session: Option<String>,
        port: Option<u16>,
        event: Option<AuditEventType>,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Self> {
        if session.as_deref() == Some("") {
            bail!("history session must not be empty");
        }
        if let Some(value) = &session
            && value.chars().count() > crate::store::MAX_SESSION_CHARS
        {
            bail!("history session exceeds the maximum length");
        }
        if !(1..=crate::store::MAX_HISTORY_LIMIT).contains(&limit) {
            bail!(
                "history limit must be between 1 and {}",
                crate::store::MAX_HISTORY_LIMIT
            );
        }
        Ok(Self {
            session,
            port,
            event,
            before,
            limit,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HistoryPage {
    pub(crate) events: Vec<AuditEvent>,
    pub(crate) has_more: bool,
    pub(crate) next_before: Option<u64>,
}

impl AuditEventKind {
    #[allow(dead_code)]
    pub(crate) fn event_type(&self) -> AuditEventType {
        match self {
            Self::LeaseClaimed { .. } => AuditEventType::LeaseClaimed,
            Self::LeaseTransferred { .. } => AuditEventType::LeaseTransferred,
            Self::LeaseReleased { .. } => AuditEventType::LeaseReleased,
            Self::LeasePruned { .. } => AuditEventType::LeasePruned,
            Self::ProcessExited { .. } => AuditEventType::ProcessExited,
            Self::GuardDenied { .. } => AuditEventType::GuardDenied,
            Self::GuardWarned { .. } => AuditEventType::GuardWarned,
            Self::HistoryCleared { .. } => AuditEventType::HistoryCleared,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn involves_session(&self, session: &str) -> bool {
        let matches = |snapshot: &LeaseSnapshot| snapshot.session.as_deref() == Some(session);
        match self {
            Self::LeaseClaimed {
                lease,
                prior_lease,
                replaced_lease,
                ..
            } => {
                matches(lease)
                    || prior_lease.as_ref().is_some_and(matches)
                    || replaced_lease.as_ref().is_some_and(matches)
            }
            Self::LeaseTransferred { lease, .. }
            | Self::LeaseReleased { lease, .. }
            | Self::LeasePruned { lease }
            | Self::ProcessExited { lease, .. }
            | Self::GuardDenied { lease, .. } => matches(lease),
            Self::GuardWarned { .. } | Self::HistoryCleared { .. } => false,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn involves_port(&self, port: u16) -> bool {
        let matches = |snapshot: &LeaseSnapshot| snapshot.port == port;
        match self {
            Self::LeaseClaimed {
                requested_port,
                lease,
                prior_lease,
                replaced_lease,
                ..
            } => {
                *requested_port == port
                    || matches(lease)
                    || prior_lease.as_ref().is_some_and(matches)
                    || replaced_lease.as_ref().is_some_and(matches)
            }
            Self::LeaseTransferred { lease, .. }
            | Self::LeaseReleased { lease, .. }
            | Self::LeasePruned { lease }
            | Self::ProcessExited { lease, .. }
            | Self::GuardDenied { lease, .. } => matches(lease),
            Self::GuardWarned { target, .. } => {
                matches!(target, GuardTarget::Port { port: target_port } if *target_port == port)
            }
            Self::HistoryCleared { .. } => false,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::LeaseClaimed {
                lease,
                prior_lease,
                replaced_lease,
                ..
            } => {
                lease.validate()?;
                if let Some(snapshot) = prior_lease {
                    snapshot.validate()?;
                }
                if let Some(snapshot) = replaced_lease {
                    snapshot.validate()?;
                }
            }
            Self::LeaseTransferred { lease, .. }
            | Self::LeaseReleased { lease, .. }
            | Self::LeasePruned { lease }
            | Self::ProcessExited { lease, .. }
            | Self::GuardDenied { lease, .. } => lease.validate()?,
            Self::GuardWarned { target, .. } => target.validate()?,
            Self::HistoryCleared { .. } => {}
        }
        if let Self::GuardDenied { target, .. } = self {
            target.validate()?;
        }
        Ok(())
    }
}

impl AuditEvent {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.sequence == 0 {
            bail!("audit event sequence must be greater than zero");
        }
        self.actor.validate()?;
        self.kind.validate()
    }
}

fn validate_optional_string(value: Option<&str>, label: &str) -> Result<()> {
    if let Some(value) = value
        && value.chars().count() > crate::store::MAX_SESSION_CHARS
    {
        bail!("{label} exceeds the maximum length");
    }
    Ok(())
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
