//! Locked JSON persistence for leases, and the port-claiming core logic.

use crate::audit::{
    AuditActor, AuditEvent, AuditEventDraft, AuditEventKind, AuditSource, ClaimDisposition,
    HistoryPage, HistoryQuery, LeaseSnapshot,
};
use crate::lease::{Lease, PidChecker};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
const V2_STATE_FORMAT_VERSION: u32 = 2;
const STATE_FORMAT_VERSION: u32 = 3;
pub(crate) const MAX_HISTORY_EVENTS: usize = 10_000;
#[allow(dead_code)]
pub(crate) const MAX_HISTORY_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize, Serialize)]
struct StoredStateV2 {
    format_version: u32,
    leases: Vec<Lease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreState {
    pub(crate) next_event_sequence: u64,
    pub(crate) leases: Vec<Lease>,
    pub(crate) events: Vec<AuditEvent>,
}

#[allow(dead_code)]
pub(crate) enum StateUpdate<R> {
    Unchanged(R),
    Commit {
        result: R,
        events: Vec<crate::audit::AuditEventDraft>,
    },
}

#[allow(dead_code)]
pub(crate) trait Clock {
    fn now(&self) -> Result<u64>;
}

#[allow(dead_code)]
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<u64> {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs())
    }
}

trait FileOps: Send + Sync {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> Result<()>;
    fn rename_temp(&self, from: &Path, to: &Path) -> Result<()>;
}

struct RealFileOps;

impl FileOps for RealFileOps {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        fs::write(path, bytes).map_err(Into::into)
    }

    fn rename_temp(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(from, to).map_err(Into::into)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredStateV3 {
    format_version: u32,
    next_event_sequence: u64,
    leases: Vec<Lease>,
    events: Vec<AuditEvent>,
}

/// Maximum allowed length of a lease `tag`, in CHARACTERS (not bytes).
/// Tags are arbitrary caller-supplied text persisted to `leases.json` and
/// echoed back by every `ls`/`who`; an unbounded tag lets a caller push
/// megabytes into the store and every listing. 1024 chars is far beyond any
/// legitimate "what is this port for" description.
pub const MAX_TAG_CHARS: usize = 1024;

/// Maximum allowed length of a lease `session` identifier, in CHARACTERS
/// (not bytes). Same reasoning as [`MAX_TAG_CHARS`]; session ids in practice
/// are short opaque identifiers.
pub const MAX_SESSION_CHARS: usize = 512;

/// Validates `claim` inputs before anything is read or written, so a
/// rejected claim never touches the store.
///
/// Port `0` is rejected because it is not a real bindable port a process
/// could own — leasing it is always a mistake (or an attempt to poison the
/// registry with a meaningless entry). `tag` and `session` are bounded by
/// [`MAX_TAG_CHARS`] / [`MAX_SESSION_CHARS`], counted in CHARACTERS so a
/// multibyte string at the character limit is accepted regardless of its
/// byte size.
pub(crate) fn validate_claim_inputs(port: u16, tag: &str, session: Option<&str>) -> Result<()> {
    if port == 0 {
        bail!("invalid port: port must be between 1 and 65535 (port 0 cannot be leased)");
    }
    if tag.chars().count() > MAX_TAG_CHARS {
        bail!(
            "invalid tag: at most {MAX_TAG_CHARS} characters, got {}",
            tag.chars().count()
        );
    }
    if let Some(session) = session
        && session.chars().count() > MAX_SESSION_CHARS
    {
        bail!(
            "invalid session: at most {MAX_SESSION_CHARS} characters, got {}",
            session.chars().count()
        );
    }
    Ok(())
}

fn validate_actor(actor: AuditActor) -> Result<AuditActor> {
    let actor = AuditActor::new(actor.source, actor.harness, actor.session);
    actor.validate()?;
    Ok(actor)
}

/// Result of a `claim` operation: the lease that was actually created/updated,
/// and whether it landed on a different port than the one requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReassignmentReason {
    LeaseConflict,
    OsOccupied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimOutcome {
    pub lease: Lease,
    pub reassigned: bool,
    pub reassignment_reason: Option<ReassignmentReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimMutation {
    pub(crate) outcome: ClaimOutcome,
    pub(crate) disposition: ClaimDisposition,
    pub(crate) prior_lease: Option<Lease>,
    pub(crate) replaced_lease: Option<Lease>,
}

impl std::ops::Deref for ClaimMutation {
    type Target = ClaimOutcome;

    fn deref(&self) -> &Self::Target {
        &self.outcome
    }
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
    file_ops: Arc<dyn FileOps>,
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
        let store = Self {
            data_dir,
            file_ops: Arc::new(RealFileOps),
        };
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
        self.read_leases_for_read()
    }

    #[allow(dead_code)]
    pub(crate) fn history(&self, query: &HistoryQuery) -> Result<HistoryPage> {
        let _guard = self.lock_exclusive()?;
        if query.before == Some(0) {
            return Ok(HistoryPage {
                events: Vec::new(),
                has_more: false,
                next_before: None,
            });
        }
        let mut matching = self
            .read_state_unlocked()?
            .events
            .into_iter()
            .rev()
            .filter(|event| query.before.is_none_or(|before| event.sequence < before))
            .filter(|event| {
                query
                    .event
                    .is_none_or(|event_type| event.kind.event_type() == event_type)
            })
            .filter(|event| {
                query.session.as_deref().is_none_or(|session| {
                    event.actor.session.as_deref() == Some(session)
                        || event.kind.involves_session(session)
                })
            })
            .filter(|event| query.port.is_none_or(|port| event.kind.involves_port(port)));
        let mut events = Vec::with_capacity(query.limit);
        while events.len() <= query.limit {
            let Some(event) = matching.next() else { break };
            events.push(event);
        }
        let has_more = events.len() > query.limit;
        if has_more {
            events.pop();
        }
        let next_before = events.last().map(|event| event.sequence);
        Ok(HistoryPage {
            events,
            has_more,
            next_before,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn clear_history(&self, actor: AuditActor) -> Result<usize> {
        let actor = validate_actor(actor)?;
        self.update_state(|state| {
            let removed_count = state.events.len() as u64;
            state.events.clear();
            Ok(StateUpdate::Commit {
                result: removed_count as usize,
                events: vec![AuditEventDraft {
                    actor,
                    kind: AuditEventKind::HistoryCleared { removed_count },
                }],
            })
        })
    }

    /// Claims `requested_port` for `pid`, following the conflict-resolution
    /// rules documented on [`claim_in_place`].
    #[allow(dead_code)]
    pub fn claim(
        &self,
        requested_port: u16,
        pid: u32,
        tag: String,
        session: Option<String>,
        checker: &dyn PidChecker,
    ) -> Result<ClaimOutcome> {
        self.claim_with_actor(
            requested_port,
            pid,
            tag,
            session,
            AuditActor::new(AuditSource::Cli, None, None),
            checker,
        )
    }

    pub(crate) fn claim_with_actor(
        &self,
        requested_port: u16,
        pid: u32,
        tag: String,
        session: Option<String>,
        actor: AuditActor,
        checker: &dyn PidChecker,
    ) -> Result<ClaimOutcome> {
        // An empty `--session ""` (e.g. from an unbound `$VAR` expansion)
        // must behave as no session: otherwise two unrelated empty sessions
        // would recognize each other's leases as their own in the guard.
        let session = session.filter(|s| !s.is_empty());
        let actor = AuditActor::new(actor.source, actor.harness, actor.session);
        actor.validate()?;
        // Validate before taking the lock / reading state so a rejected
        // claim never touches (or even waits on) the store.
        validate_claim_inputs(requested_port, &tag, session.as_deref())?;
        self.update_state(|state| {
            let mutation = claim_in_place(
                &mut state.leases,
                requested_port,
                pid,
                tag,
                session,
                checker,
            )?;
            let draft = AuditEventDraft {
                actor,
                kind: AuditEventKind::LeaseClaimed {
                    requested_port,
                    disposition: mutation.disposition,
                    lease: LeaseSnapshot::from(&mutation.outcome.lease),
                    prior_lease: mutation.prior_lease.as_ref().map(LeaseSnapshot::from),
                    replaced_lease: mutation.replaced_lease.as_ref().map(LeaseSnapshot::from),
                },
            };
            Ok(StateUpdate::Commit {
                result: mutation.outcome,
                events: vec![draft],
            })
        })
    }

    /// Returns the lease on `port`, if any.
    pub fn get(&self, port: u16) -> Result<Option<Lease>> {
        let _guard = self.lock_exclusive()?;
        let leases = self.read_leases_for_read()?;
        Ok(leases.into_iter().find(|lease| lease.port == port))
    }

    /// Removes the lease on `port`, if any, and reports whether its owning
    /// PID was still alive at the time of removal. Returns `None` if there
    /// was no lease on `port`.
    pub fn release(
        &self,
        port: u16,
        actor: AuditActor,
        checker: &dyn PidChecker,
    ) -> Result<Option<ReleaseOutcome>> {
        let actor = validate_actor(actor)?;
        self.update_state(|state| {
            let Some(outcome) = release_in_place(&mut state.leases, port, checker) else {
                return Ok(StateUpdate::Unchanged(None));
            };
            let event = AuditEventDraft {
                actor,
                kind: AuditEventKind::LeaseReleased {
                    lease: LeaseSnapshot::from(&outcome.lease),
                    was_alive: outcome.was_alive,
                },
            };
            Ok(StateUpdate::Commit {
                result: Some(outcome),
                events: vec![event],
            })
        })
    }

    /// Removes every lease whose owning PID is dead, and returns the
    /// removed leases. Returns an empty vector if nothing was pruned.
    pub fn prune(&self, actor: AuditActor, checker: &dyn PidChecker) -> Result<Vec<Lease>> {
        let actor = validate_actor(actor)?;
        self.update_state(|state| {
            let pruned = prune_in_place(&mut state.leases, checker);
            if pruned.is_empty() {
                return Ok(StateUpdate::Unchanged(Vec::new()));
            }
            let events = pruned
                .iter()
                .map(|lease| AuditEventDraft {
                    actor: actor.clone(),
                    kind: AuditEventKind::LeasePruned {
                        lease: LeaseSnapshot::from(lease),
                    },
                })
                .collect();
            Ok(StateUpdate::Commit {
                result: pruned,
                events,
            })
        })
    }

    /// Transfers the lease on `port` from the expected wrapper identity to a
    /// verified child PID, preserving port, tag, and session.
    ///
    /// The transfer only succeeds when the recorded lease still carries the
    /// expected wrapper PID and start time and is alive, and when the child
    /// PID is alive with a resolvable start time. Only the PID, process-start
    /// identity, verification marker, and renewal timestamp change. The lease
    /// is left untouched on any rejection.
    pub(crate) fn transfer(
        &self,
        port: u16,
        expected_owner_pid: u32,
        expected_owner_start_time: u64,
        new_owner_pid: u32,
        actor: AuditActor,
        checker: &dyn PidChecker,
    ) -> Result<Lease> {
        let actor = validate_actor(actor)?;
        self.update_state(|state| {
            let index = state
                .leases
                .iter()
                .position(|lease| lease.port == port)
                .with_context(|| format!("no lease on port {port}: transfer rejected"))?;
            let existing = state.leases[index].clone();
            if existing.pid != expected_owner_pid
                || existing.process_start_time != Some(expected_owner_start_time)
            {
                bail!(
                    "lease on port {port} is not owned by expected owner {expected_owner_pid}: transfer rejected"
                );
            }
            if !existing.is_alive(checker) {
                bail!("lease on port {port} owner is no longer alive: transfer rejected");
            }
            if !checker.is_alive(new_owner_pid) {
                bail!("child pid {new_owner_pid} is not alive: transfer rejected");
            }
            let Some(child_start_time) = checker.process_start_time(new_owner_pid) else {
                bail!("child pid {new_owner_pid} has no resolvable start time: transfer rejected");
            };
            let transferred = Lease::new_with_process_start_time(
                existing.port,
                new_owner_pid,
                existing.tag.clone(),
                existing.session.clone(),
                Some(child_start_time),
            );
            state.leases[index] = transferred.clone();
            Ok(StateUpdate::Commit {
                result: transferred.clone(),
                events: vec![AuditEventDraft {
                    actor,
                    kind: AuditEventKind::LeaseTransferred {
                        wrapper_pid: expected_owner_pid,
                        wrapper_start_time: expected_owner_start_time,
                        lease: LeaseSnapshot::from(&transferred),
                    },
                }],
            })
        })
    }

    /// Ensures the data directory exists and is restricted to owner-only
    /// access (`0700` on Unix). Leases can carry PIDs and tags that reveal
    /// what a user is running, so the store directory is not world-readable.
    fn ensure_data_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).with_context(|| {
            format!(
                "failed to create data directory at {}",
                self.data_dir.display()
            )
        })?;
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
            .with_context(|| {
                format!(
                    "failed to open lock file at {}",
                    self.lock_file_path().display()
                )
            })?;
        harden_file_permissions(&self.lock_file_path())?;
        // Called explicitly via the `fs4` trait (rather than relying on the
        // newer `std::fs::File::lock` inherent method) so locking stays on a
        // dependency with documented cross-platform semantics.
        fs4::FileExt::lock(&lock_file)
            .context("failed to acquire exclusive lock on the lease store")?;
        Ok(lock_file)
    }

    #[cfg(test)]
    pub(crate) fn read_state(&self) -> Result<StoreState> {
        let _guard = self.lock_exclusive()?;
        self.read_state_unlocked()
    }

    fn read_leases_for_read(&self) -> Result<Vec<Lease>> {
        Ok(self.read_state_unlocked()?.leases)
    }

    fn read_state_unlocked(&self) -> Result<StoreState> {
        let path = self.state_file_path();
        let Some(value) = self.read_json_value()? else {
            return Ok(StoreState {
                next_event_sequence: 1,
                leases: Vec::new(),
                events: Vec::new(),
            });
        };

        match value {
            serde_json::Value::Array(value) => {
                let leases: Vec<Lease> = serde_json::from_value(serde_json::Value::Array(value)).with_context(|| {
                    format!(
                        "state file at {} contains invalid legacy lease data and was not modified",
                        path.display()
                    )
                })?;
                validate_unique_ports(&leases, &path)?;
                Ok(StoreState {
                    next_event_sequence: 1,
                    leases,
                    events: Vec::new(),
                })
            }
            serde_json::Value::Object(ref object) => {
                let version = object
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "state file at {} is an object without a numeric format_version and was not modified",
                            path.display()
                        )
                    })?;
                match version as u32 {
                    V2_STATE_FORMAT_VERSION => {
                        let state: StoredStateV2 = serde_json::from_value(value).with_context(|| {
                            format!(
                                "state file at {} contains invalid versioned lease data and was not modified",
                                path.display()
                            )
                        })?;
                        validate_unique_ports(&state.leases, &path)?;
                        Ok(StoreState {
                            next_event_sequence: 1,
                            leases: state.leases,
                            events: Vec::new(),
                        })
                    }
                    STATE_FORMAT_VERSION => {
                        let state: StoredStateV3 = serde_json::from_value(value).with_context(|| {
                            format!(
                                "state file at {} contains invalid v3 lease data and was not modified",
                                path.display()
                            )
                        })?;
                        let state = StoreState {
                            next_event_sequence: state.next_event_sequence,
                            leases: state.leases,
                            events: state.events,
                        };
                        validate_state(&state, &path)?;
                        Ok(state)
                    }
                    _ => bail!(
                        "unsupported state file format version {version} at {}; upgrade portzilla before using this state file",
                        path.display()
                    ),
                }
            }
            _ => bail!(
                "state file at {} must be a legacy lease array or a versioned object and was not modified",
                path.display()
            ),
        }
    }

    #[cfg(test)]
    fn read_leases(&self) -> Result<Vec<Lease>> {
        let path = self.state_file_path();
        let Some(value) = self.read_json_value()? else {
            return Ok(Vec::new());
        };
        match value {
            serde_json::Value::Array(value) => {
                let leases: Vec<Lease> = serde_json::from_value(serde_json::Value::Array(value)).with_context(|| {
                    format!(
                        "state file at {} contains invalid legacy lease data and was not modified",
                        path.display()
                    )
                })?;
                validate_unique_ports(&leases, &path)?;
                Ok(leases)
            }
            serde_json::Value::Object(object) => {
                let version = object
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("state file at {} is an object without a numeric format_version and was not modified", path.display()))?;
                if version != V2_STATE_FORMAT_VERSION as u64 {
                    bail!(
                        "unsupported state file format version {version} at {}; upgrade portzilla before using this state file",
                        path.display()
                    );
                }
                let state: StoredStateV2 = serde_json::from_value(serde_json::Value::Object(object)).with_context(|| {
                    format!(
                        "state file at {} contains invalid versioned lease data and was not modified",
                        path.display()
                    )
                })?;
                validate_unique_ports(&state.leases, &path)?;
                Ok(state.leases)
            }
            _ => bail!(
                "state file at {} must be a legacy lease array or a versioned object and was not modified",
                path.display()
            ),
        }
    }

    fn read_json_value(&self) -> Result<Option<serde_json::Value>> {
        let path = self.state_file_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read state file at {}", path.display()));
            }
        };
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(None);
        }
        serde_json::from_slice(&bytes).map(Some).with_context(|| {
            format!(
                "state file at {} contains invalid JSON and was not modified",
                path.display()
            )
        })
    }

    #[cfg(test)]
    fn write_v2_leases(&self, leases: &[Lease]) -> Result<()> {
        let path = self.state_file_path();
        let tmp_path = self
            .data_dir
            .join(format!(".leases.json.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&StoredStateV2 {
            format_version: V2_STATE_FORMAT_VERSION,
            leases: leases.to_vec(),
        })
        .context("failed to serialize leases to JSON")?;
        self.file_ops
            .write_temp(&tmp_path, &json)
            .with_context(|| {
                format!(
                    "failed to write temporary state file at {}",
                    tmp_path.display()
                )
            })?;
        // Harden before the rename: rename preserves the file's existing mode,
        // so this also leaves the final `leases.json` owner-only (0600).
        harden_file_permissions(&tmp_path)?;
        self.file_ops
            .rename_temp(&tmp_path, &path)
            .with_context(|| {
                format!(
                    "failed to atomically replace state file at {}",
                    path.display()
                )
            })?;
        Ok(())
    }

    #[allow(dead_code)]
    fn update_state<R>(
        &self,
        mutate: impl FnOnce(&mut StoreState) -> Result<StateUpdate<R>>,
    ) -> Result<R> {
        self.update_state_with_clock(SystemClock, mutate)
    }

    #[allow(dead_code)]
    fn update_state_with_clock<R>(
        &self,
        clock: impl Clock,
        mutate: impl FnOnce(&mut StoreState) -> Result<StateUpdate<R>>,
    ) -> Result<R> {
        let _guard = self.lock_exclusive()?;
        let mut state = self.read_state_unlocked()?;
        let update = mutate(&mut state)?;
        let StateUpdate::Commit { result, events } = update else {
            let StateUpdate::Unchanged(result) = update else {
                unreachable!();
            };
            return Ok(result);
        };
        if events.is_empty() {
            bail!("state commit requires at least one event draft");
        }
        if state.next_event_sequence == u64::MAX
            || events.len() as u64 > u64::MAX - state.next_event_sequence
        {
            bail!("state update would exhaust the event sequence");
        }
        let occurred_at = clock.now()?;
        for draft in events {
            let sequence = state.next_event_sequence;
            state.next_event_sequence += 1;
            state.events.push(AuditEvent {
                sequence,
                occurred_at,
                actor: draft.actor,
                kind: draft.kind,
            });
        }
        let excess = state.events.len().saturating_sub(MAX_HISTORY_EVENTS);
        if excess > 0 {
            state.events.drain(..excess);
        }
        validate_state(&state, &self.state_file_path())?;
        self.write_v3_state(&state)?;
        Ok(result)
    }

    #[allow(dead_code)]
    fn write_v3_state(&self, state: &StoreState) -> Result<()> {
        let path = self.state_file_path();
        let tmp_path = self
            .data_dir
            .join(format!(".leases.json.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&StoredStateV3 {
            format_version: STATE_FORMAT_VERSION,
            next_event_sequence: state.next_event_sequence,
            leases: state.leases.clone(),
            events: state.events.clone(),
        })
        .context("failed to serialize state to JSON")?;
        self.file_ops
            .write_temp(&tmp_path, &json)
            .with_context(|| {
                format!(
                    "failed to write temporary state file at {}",
                    tmp_path.display()
                )
            })?;
        harden_file_permissions(&tmp_path)?;
        self.file_ops
            .rename_temp(&tmp_path, &path)
            .with_context(|| {
                format!(
                    "failed to atomically replace state file at {}",
                    path.display()
                )
            })?;
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
        self.write_v2_leases(&leases)?;
        Ok(outcome.outcome)
    }
}

fn validate_state(state: &StoreState, path: &Path) -> Result<()> {
    validate_unique_ports(&state.leases, path)?;
    if state.next_event_sequence == 0 {
        bail!(
            "state file at {} has an invalid zero next_event_sequence and was not modified",
            path.display()
        );
    }
    if state.events.len() > MAX_HISTORY_EVENTS {
        bail!(
            "state file at {} contains more than {MAX_HISTORY_EVENTS} events and was not modified",
            path.display()
        );
    }

    let mut previous_sequence = 0;
    for event in &state.events {
        event.validate().with_context(|| {
            format!(
                "state file at {} contains an invalid audit event and was not modified",
                path.display()
            )
        })?;
        if event.sequence <= previous_sequence {
            bail!(
                "state file at {} contains duplicate or unordered event sequences and was not modified",
                path.display()
            );
        }
        previous_sequence = event.sequence;
    }
    if previous_sequence >= state.next_event_sequence {
        bail!(
            "state file at {} has next_event_sequence not above retained events and was not modified",
            path.display()
        );
    }
    Ok(())
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
) -> Result<ClaimMutation> {
    let process_start_time = checker.process_start_time(pid);
    let existing_index = leases.iter().position(|l| l.port == requested_port);

    match existing_index {
        None => {
            let (port, reassignment_reason) =
                resolve_claim_port(requested_port, leases, checker, false)?;
            let lease =
                Lease::new_with_process_start_time(port, pid, tag, session, process_start_time);
            upsert_lease(leases, lease.clone());
            let disposition = match reassignment_reason {
                Some(ReassignmentReason::LeaseConflict) => {
                    ClaimDisposition::ReassignedLeaseConflict
                }
                Some(ReassignmentReason::OsOccupied) => ClaimDisposition::ReassignedOsOccupied,
                None => ClaimDisposition::Created,
            };
            Ok(ClaimMutation {
                outcome: ClaimOutcome {
                    lease,
                    reassigned: port != requested_port,
                    reassignment_reason,
                },
                disposition,
                prior_lease: None,
                replaced_lease: None,
            })
        }
        Some(index) if leases[index].pid == pid && leases[index].is_alive(checker) => {
            let prior_lease = leases[index].clone();
            let lease = Lease::new_with_process_start_time(
                requested_port,
                pid,
                tag,
                session,
                process_start_time,
            );
            upsert_lease(leases, lease.clone());
            Ok(ClaimMutation {
                outcome: ClaimOutcome {
                    lease,
                    reassigned: false,
                    reassignment_reason: None,
                },
                disposition: ClaimDisposition::Updated,
                prior_lease: Some(prior_lease),
                replaced_lease: None,
            })
        }
        Some(index) if leases[index].is_alive(checker) => {
            let prior_lease = leases[index].clone();
            let (next_port, reassignment_reason) =
                resolve_claim_port(requested_port, leases, checker, true)?;
            let replaced_lease = leases
                .iter()
                .find(|lease| lease.port == next_port && !lease.is_alive(checker))
                .cloned();
            let lease = Lease::new_with_process_start_time(
                next_port,
                pid,
                tag,
                session,
                process_start_time,
            );
            // `next_port` may carry a stale (dead) lease that made it look
            // "available" to find_next_available_port: upsert, don't push
            // blindly, or the dead entry and the new one both survive on the
            // same port.
            upsert_lease(leases, lease.clone());
            Ok(ClaimMutation {
                outcome: ClaimOutcome {
                    lease,
                    reassigned: true,
                    reassignment_reason,
                },
                disposition: ClaimDisposition::ReassignedLeaseConflict,
                prior_lease: Some(prior_lease),
                replaced_lease,
            })
        }
        Some(_) => {
            // Dead lease on the requested port: prune it and take over the port.
            let prior_lease = leases[existing_index.unwrap()].clone();
            let (port, reassignment_reason) =
                resolve_claim_port(requested_port, leases, checker, false)?;
            if port != requested_port {
                // The requested port is occupied by an unregistered process,
                // so the dead declaration can no longer be reused. Remove it
                // rather than leaving stale ownership behind.
                leases.retain(|lease| lease.port != requested_port);
            }
            let lease =
                Lease::new_with_process_start_time(port, pid, tag, session, process_start_time);
            upsert_lease(leases, lease.clone());
            let disposition = match reassignment_reason {
                Some(ReassignmentReason::LeaseConflict) => {
                    ClaimDisposition::ReassignedLeaseConflict
                }
                Some(ReassignmentReason::OsOccupied) => ClaimDisposition::ReassignedOsOccupied,
                None => ClaimDisposition::ReplacedDead,
            };
            Ok(ClaimMutation {
                outcome: ClaimOutcome {
                    lease,
                    reassigned: port != requested_port,
                    reassignment_reason,
                },
                disposition,
                prior_lease: Some(prior_lease),
                replaced_lease: None,
            })
        }
    }
}

fn resolve_claim_port(
    requested_port: u16,
    leases: &[Lease],
    checker: &dyn PidChecker,
    lease_conflict: bool,
) -> Result<(u16, Option<ReassignmentReason>)> {
    if !lease_conflict && is_os_port_free(requested_port) {
        return Ok((requested_port, None));
    }

    let next_port = find_next_available_port(
        requested_port
            .checked_add(1)
            .context("no available port: requested port is already at the maximum")?,
        leases,
        checker,
    )
    .context(if lease_conflict {
        "no available port found while resolving a port conflict"
    } else {
        "no available port found while resolving an OS port conflict"
    })?;
    let reason = if lease_conflict {
        ReassignmentReason::LeaseConflict
    } else {
        ReassignmentReason::OsOccupied
    };
    Ok((next_port, Some(reason)))
}

fn validate_unique_ports(leases: &[Lease], path: &Path) -> Result<()> {
    let mut ports = std::collections::HashSet::with_capacity(leases.len());
    for lease in leases {
        if !ports.insert(lease.port) {
            bail!(
                "state file at {} contains duplicate lease port {} and was not modified",
                path.display(),
                lease.port
            );
        }
    }
    Ok(())
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

/// Probes whether a port is currently free by attempting to bind local
/// wildcard and loopback addresses in both families. Some platforms allow a
/// wildcard bind alongside a loopback-only listener, so both are needed for
/// reliable localhost coordination. IPv6 is optional on some platforms, so
/// unavailable IPv6 addresses are treated as unprobeable rather than occupied.
pub(crate) fn is_os_port_free(port: u16) -> bool {
    if TcpListener::bind(("0.0.0.0", port)).is_err()
        || TcpListener::bind(("127.0.0.1", port)).is_err()
    {
        return false;
    }

    ["::", "::1"]
        .into_iter()
        .all(|address| match TcpListener::bind((address, port)) {
            Ok(_) => true,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) =>
            {
                true
            }
            Err(_) => false,
        })
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
    use crate::audit::{AuditActor, AuditEventDraft, AuditEventKind, AuditEventType, AuditSource};
    use crate::lease::{
        SystemPidChecker,
        test_support::{AlivePids, AliveWithoutIdentity, AlwaysAlive, AlwaysDead, ProcessIdentity},
    };
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    fn unused_test_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn unused_adjacent_test_ports() -> (u16, u16) {
        for _ in 0..100 {
            let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let base = first.local_addr().unwrap().port();
            if let Some(second) = base
                .checked_add(1)
                .and_then(|port| TcpListener::bind(("127.0.0.1", port)).ok())
            {
                drop(second);
                drop(first);
                return (base, base + 1);
            }
        }
        panic!("could not select adjacent test ports");
    }

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
        let resolved = Store::resolve_data_dir(
            None,
            None,
            Some("/xdg/dir".to_string()),
            Some("/home/dir".to_string()),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/xdg/dir/portzilla"));
    }

    #[test]
    fn resolve_data_dir_falls_back_to_home_local_share() {
        let resolved =
            Store::resolve_data_dir(None, None, None, Some("/home/dir".to_string())).unwrap();
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

    fn v3_event_json(sequence: u64) -> String {
        format!(
            r#"{{"sequence":{sequence},"occurred_at":1,"source":"cli","actor":{{"source":"cli","harness":null,"session":null}},"event":"history_cleared","data":{{"removed_count":0}}}}"#
        )
    }

    fn write_v3_fixture(store: &Store, next_event_sequence: u64, events: &[String]) -> Vec<u8> {
        let original = format!(
            r#"{{"format_version":3,"next_event_sequence":{next_event_sequence},"leases":[],"events":[{}]}}"#,
            events.join(",")
        )
        .into_bytes();
        std::fs::write(store.state_file_path(), &original).unwrap();
        original
    }

    #[test]
    fn v2_state_decodes_with_empty_history_and_next_sequence_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        std::fs::write(
            store.state_file_path(),
            r#"{"format_version":2,"leases":[]}"#,
        )
        .unwrap();

        let state = store.read_state().unwrap();
        assert!(state.leases.is_empty());
        assert!(state.events.is_empty());
        assert_eq!(state.next_event_sequence, 1);
    }

    #[test]
    fn legacy_array_decodes_with_empty_history_and_next_sequence_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        std::fs::write(
            store.state_file_path(),
            r#"[{"port":3000,"pid":100,"tag":"legacy","created_at":1,"session":null}]"#,
        )
        .unwrap();

        let state = store.read_state().unwrap();
        assert_eq!(state.leases.len(), 1);
        assert!(state.events.is_empty());
        assert_eq!(state.next_event_sequence, 1);
    }

    #[test]
    fn v3_state_round_trips_leases_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 2, &[v3_event_json(1)]);

        let state = store.read_state().unwrap();
        assert_eq!(state.next_event_sequence, 2);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].sequence, 1);
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn read_only_calls_do_not_migrate_legacy_or_v2_state() {
        for fixture in [
            r#"[{"port":3000,"pid":100,"tag":"legacy","created_at":1,"session":null}]"#,
            r#"{"format_version":2,"leases":[]}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
            std::fs::write(store.state_file_path(), fixture).unwrap();
            let original = std::fs::read(store.state_file_path()).unwrap();
            assert!(store.read_state().is_ok());
            assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
        }
    }

    #[test]
    fn v3_rejects_zero_next_event_sequence_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 0, &[]);
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn v3_rejects_duplicate_or_unordered_event_sequences_without_rewriting() {
        for events in [
            vec![v3_event_json(1), v3_event_json(1)],
            vec![v3_event_json(2), v3_event_json(1)],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
            let original = write_v3_fixture(&store, 3, &events);
            assert!(store.read_state().is_err());
            assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
        }
    }

    #[test]
    fn v3_rejects_zero_event_sequence_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 1, &[v3_event_json(0)]);
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn v3_rejects_next_sequence_not_above_retained_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 1, &[v3_event_json(1)]);
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn v3_rejects_more_than_ten_thousand_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let events: Vec<_> = (1..=10_001).map(v3_event_json).collect();
        let original = write_v3_fixture(&store, 10_002, &events);
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn v3_rejects_oversized_bounded_event_fields_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let event = format!(
            r#"{{"sequence":1,"occurred_at":1,"actor":{{"source":"cli","harness":null,"session":"{}"}},"event":"history_cleared","data":{{"removed_count":0}}}}"#,
            "s".repeat(MAX_SESSION_CHARS + 1)
        );
        let original = write_v3_fixture(&store, 2, &[event]);
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn unknown_state_version_remains_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = br#"{"format_version":99,"leases":[]}"#;
        std::fs::write(store.state_file_path(), original).unwrap();
        assert!(store.read_state().is_err());
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
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
        let port = unused_test_port();
        let store_a = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let outcome = store_a
            .claim(port, 111, "svc".to_string(), None, &AlwaysAlive)
            .unwrap();

        let store_b = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let leases = store_b.list().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].port, outcome.lease.port);
        assert_eq!(leases[0].pid, 111);
    }

    #[test]
    fn state_file_writes_a_versioned_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let port = unused_test_port();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(port, 111, "svc".to_string(), None, &AlwaysAlive)
            .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.state_file_path()).unwrap())
                .unwrap();
        assert_eq!(state["format_version"], STATE_FORMAT_VERSION);
        assert_eq!(state["leases"][0]["port"], port);
    }

    #[test]
    fn unknown_state_format_is_rejected_without_rewriting_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = br#"{"format_version":99,"leases":[]}"#;
        std::fs::write(store.state_file_path(), original).unwrap();

        let error = store.list().unwrap_err().to_string();
        assert!(error.contains("unsupported state file format version 99"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn duplicate_ports_in_a_versioned_state_file_are_rejected_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = br#"{"format_version":2,"leases":[{"port":32001,"pid":101,"tag":"one","created_at":1,"session":null},{"port":32001,"pid":102,"tag":"two","created_at":1,"session":null}]}"#;
        std::fs::write(store.state_file_path(), original).unwrap();

        let error = store.list().unwrap_err().to_string();
        assert!(error.contains("duplicate lease port 32001"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn duplicate_ports_in_a_legacy_state_file_are_rejected_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = br#"[{"port":32002,"pid":101,"tag":"one","created_at":1,"session":null},{"port":32002,"pid":102,"tag":"two","created_at":1,"session":null}]"#;
        std::fs::write(store.state_file_path(), original).unwrap();

        let error = store.list().unwrap_err().to_string();
        assert!(error.contains("duplicate lease port 32002"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn verified_identity_persists_and_remains_alive_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let checker = ProcessIdentity {
            pid: 100,
            start_time: Some(123),
        };
        let port = unused_test_port();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(port, checker.pid, "svc".to_string(), None, &checker)
            .unwrap();

        let reloaded = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let lease = reloaded.list().unwrap().pop().unwrap();
        assert_eq!(lease.process_start_time, Some(123));
        assert!(lease.is_alive(&checker));
        assert!(
            std::fs::read_to_string(reloaded.state_file_path())
                .unwrap()
                .contains("process_identity_verified")
        );
    }

    #[test]
    fn unresolved_identity_persists_as_unverified_and_is_not_alive_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let port = unused_test_port();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(port, 100, "svc".to_string(), None, &AliveWithoutIdentity)
            .unwrap();

        let reloaded = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let lease = reloaded.list().unwrap().pop().unwrap();
        assert_eq!(lease.process_start_time, None);
        assert!(!lease.is_alive(&AliveWithoutIdentity));
        assert!(
            std::fs::read_to_string(reloaded.state_file_path())
                .unwrap()
                .contains("process_identity_verified")
        );
    }

    #[test]
    fn nonexistent_pid_claim_is_persisted_as_unverified_and_dead() {
        let dir = tempfile::tempdir().unwrap();
        let port = unused_test_port();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let outcome = store
            .claim(
                port,
                4_000_000_000,
                "pre-start".to_string(),
                None,
                &SystemPidChecker,
            )
            .unwrap();

        assert_eq!(outcome.lease.process_start_time, None);
        assert!(!outcome.lease.is_alive(&SystemPidChecker));
        let persisted = store.list().unwrap().pop().unwrap();
        assert_eq!(persisted.process_start_time, None);
        assert!(!persisted.is_alive(&SystemPidChecker));
    }

    #[test]
    fn dead_requested_lease_is_removed_when_its_port_is_os_occupied() {
        let dir = tempfile::tempdir().unwrap();
        let requested_port = unused_test_port();
        let listener = TcpListener::bind(("127.0.0.1", requested_port)).unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .write_v2_leases(&[Lease::new(requested_port, 4_000_000_000, "stale", None)])
            .unwrap();

        let outcome = store
            .claim(
                requested_port,
                4_000_000_001,
                "replacement".to_string(),
                None,
                &AlwaysDead,
            )
            .unwrap();
        drop(listener);

        assert!(outcome.reassigned);
        let leases = store.list().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].port, outcome.lease.port);
        assert_ne!(leases[0].port, requested_port);
        assert_eq!(leases[0].pid, 4_000_000_001);
    }

    #[test]
    fn legacy_lease_keeps_pid_only_liveness_after_read_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        std::fs::write(
            store.state_file_path(),
            r#"[{"port":4000,"pid":100,"tag":"legacy","created_at":1,"session":null}]"#,
        )
        .unwrap();

        assert!(store.list().unwrap()[0].is_alive(&AlwaysAlive));
        store
            .claim(
                4001,
                101,
                "new".to_string(),
                None,
                &ProcessIdentity {
                    pid: 101,
                    start_time: Some(456),
                },
            )
            .unwrap();

        let reloaded = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let leases = reloaded.list().unwrap();
        let legacy = leases.iter().find(|lease| lease.port == 4000).unwrap();
        assert!(legacy.is_alive(&AlwaysAlive));
        assert!(
            !std::fs::read_to_string(reloaded.state_file_path())
                .unwrap()
                .contains(r#""process_identity_verified":null"#)
        );
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(reloaded.state_file_path()).unwrap())
                .unwrap();
        assert_eq!(state["format_version"], STATE_FORMAT_VERSION);
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
                    .claim_with_hook(
                        5000 + i as u16,
                        200 + i,
                        "svc".to_string(),
                        None,
                        &AlwaysAlive,
                        || {
                            if in_critical_section.swap(true, Ordering::SeqCst) {
                                overlap_detected.store(true, Ordering::SeqCst);
                            }
                            thread::sleep(Duration::from_millis(20));
                            in_critical_section.store(false, Ordering::SeqCst);
                        },
                    )
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
    fn is_os_port_free_reports_ipv4_loopback_listener_as_taken() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(!is_os_port_free(port));
        drop(listener);
        assert!(is_os_port_free(port));
    }

    #[test]
    fn is_os_port_free_reports_ipv4_wildcard_listener_as_taken() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(!is_os_port_free(port));
        drop(listener);
        assert!(is_os_port_free(port));
    }

    #[test]
    fn is_os_port_free_reports_ipv6_wildcard_listener_as_taken() {
        let listener = match std::net::TcpListener::bind(("::", 0)) {
            Ok(listener) => listener,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("failed to bind IPv6 wildcard test listener: {error}"),
        };
        let port = listener.local_addr().unwrap().port();

        assert!(!is_os_port_free(port));
        drop(listener);
        assert!(is_os_port_free(port));
    }

    #[test]
    fn is_os_port_free_reports_ipv6_loopback_listener_as_taken() {
        let listener = match std::net::TcpListener::bind(("::1", 0)) {
            Ok(listener) => listener,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("failed to bind IPv6 loopback test listener: {error}"),
        };
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
        let port = unused_test_port();
        let outcome = claim_in_place(
            &mut leases,
            port,
            100,
            "server".to_string(),
            None,
            &AlwaysAlive,
        )
        .unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(outcome.lease.port, port);
        assert_eq!(outcome.lease.pid, 100);
        assert_eq!(leases.len(), 1);
    }

    #[test]
    fn claim_os_occupied_port_reassigns_and_reports_os_occupancy() {
        let base = unused_test_port();
        let listener = TcpListener::bind(("127.0.0.1", base)).unwrap();
        let mut leases = vec![];

        let outcome = claim_in_place(
            &mut leases,
            base,
            100,
            "server".to_string(),
            None,
            &AlwaysAlive,
        )
        .unwrap();

        drop(listener);

        assert!(outcome.reassigned);
        assert_eq!(
            outcome.reassignment_reason,
            Some(ReassignmentReason::OsOccupied)
        );
        assert!(outcome.lease.port > base);
    }

    #[test]
    fn claim_live_lease_conflict_reports_lease_conflict() {
        let base: u16 = 25100;
        let mut leases = vec![Lease::new(base, 999, "other-owner", None)];
        let outcome = claim_in_place(
            &mut leases,
            base,
            100,
            "server".to_string(),
            None,
            &AlivePids(vec![999]),
        )
        .unwrap();

        assert_eq!(
            outcome.reassignment_reason,
            Some(ReassignmentReason::LeaseConflict)
        );
    }

    #[test]
    fn claim_records_the_target_process_identity() {
        let mut leases = vec![];
        let port = unused_test_port();
        let outcome = claim_in_place(
            &mut leases,
            port,
            100,
            "server".to_string(),
            None,
            &ProcessIdentity {
                pid: 100,
                start_time: Some(123),
            },
        )
        .unwrap();

        assert_eq!(outcome.lease.process_start_time, Some(123));
    }

    #[test]
    fn claim_without_resolved_process_identity_is_not_alive() {
        let mut leases = vec![];
        let port = unused_test_port();
        let outcome = claim_in_place(
            &mut leases,
            port,
            100,
            "server".to_string(),
            None,
            &AliveWithoutIdentity,
        )
        .unwrap();

        assert!(!outcome.lease.is_alive(&AliveWithoutIdentity));
    }

    #[test]
    fn claim_replaces_a_dead_lease_on_the_same_port() {
        let port = unused_test_port();
        let mut leases = vec![Lease::new(port, 999, "stale-owner", None)];
        let outcome = claim_in_place(
            &mut leases,
            port,
            100,
            "server".to_string(),
            None,
            &AlwaysDead,
        )
        .unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(outcome.lease.port, port);
        assert_eq!(outcome.lease.pid, 100);
        assert_eq!(
            leases.len(),
            1,
            "dead lease should be replaced, not duplicated"
        );
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
        assert!(outcome.lease.port > base);
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
    fn claim_with_recycled_pid_replaces_stale_same_pid_lease() {
        let port = unused_test_port();
        let mut leases = vec![Lease::new_with_process_start_time(
            port,
            100,
            "old-server",
            None,
            Some(123),
        )];
        let outcome = claim_in_place(
            &mut leases,
            port,
            100,
            "new-server".to_string(),
            None,
            &ProcessIdentity {
                pid: 100,
                start_time: Some(456),
            },
        )
        .unwrap();

        assert!(!outcome.reassigned);
        assert_eq!(outcome.lease.tag, "new-server");
        assert_eq!(outcome.lease.process_start_time, Some(456));
        assert_eq!(leases.len(), 1);
    }

    #[test]
    fn claim_reassignment_skips_both_live_leased_and_os_bound_ports() {
        // Dedicated port range, isolated from other tests that also probe real
        // OS ports concurrently.
        let base: u16 = 22000;
        // base:     live lease owned by someone else (conflict, triggers scan).
        // base + 1: no lease, but OS has it bound.
        // A forward port without an OS bind should be picked.
        let listener = TcpListener::bind(("127.0.0.1", base + 1)).unwrap();

        let mut leases = vec![Lease::new(base, 999, "other-owner", None)];
        let checker = AlivePids(vec![999]);
        let outcome =
            claim_in_place(&mut leases, base, 100, "server".to_string(), None, &checker).unwrap();

        drop(listener);

        assert!(outcome.reassigned);
        assert!(outcome.lease.port > base + 1);
    }

    #[test]
    fn claim_reassignment_reuses_a_port_whose_lease_is_dead() {
        let base = unused_test_port();
        // base: live lease owned by someone else (conflict).
        // base + 1: dead lease candidate; OS occupancy may move the claim farther.
        let mut leases = vec![
            Lease::new(base, 999, "other-owner", None),
            Lease::new(base + 1, 998, "stale", None),
        ];
        let checker = AlivePids(vec![999]);
        let outcome =
            claim_in_place(&mut leases, base, 100, "server".to_string(), None, &checker).unwrap();

        assert!(outcome.reassigned);
        assert!(outcome.lease.port > base);
        assert_eq!(outcome.lease.pid, 100);

        // Regression guard: reassigning onto a port that had a stale (dead)
        // lease must upsert, not leave the old dead entry AND a new one
        // sitting on the same port. Exactly one entry per port, and the
        // survivor carrying the new PID must be the new claim, not the stale one.
        let ports: std::collections::HashSet<u16> = leases.iter().map(|l| l.port).collect();
        assert_eq!(ports.len(), leases.len(), "no two leases may share a port");
        assert_eq!(
            leases
                .iter()
                .filter(|lease| lease.port == outcome.lease.port)
                .count(),
            1,
            "the reassigned port must have exactly one lease"
        );
        let survivor = leases.iter().find(|l| l.pid == 100).unwrap();
        assert_eq!(
            survivor.pid, 100,
            "the new owner must replace the stale lease"
        );
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
            .claim(27010, 100, "server".to_string(), None, &AlwaysAlive)
            .unwrap();

        let lease = store.get(27010).unwrap().unwrap();
        assert_eq!(lease.port, 27010);
        assert_eq!(lease.pid, 100);
    }

    #[test]
    fn store_release_persists_the_removal() {
        let dir = tempfile::tempdir().unwrap();
        let port = 27011;
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .write_v2_leases(&[Lease::new_with_process_start_time(
                port,
                100,
                "server",
                None,
                Some(0),
            )])
            .unwrap();

        let outcome = store
            .release(
                port,
                AuditActor::new(AuditSource::Cli, None, None),
                &AlwaysAlive,
            )
            .unwrap()
            .unwrap();
        assert_eq!(outcome.lease.port, port);
        assert!(outcome.was_alive);
        assert!(store.get(port).unwrap().is_none());
    }

    #[test]
    fn store_release_returns_none_for_a_port_with_no_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        assert!(
            store
                .release(
                    3000,
                    AuditActor::new(AuditSource::Cli, None, None),
                    &AlwaysAlive
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn store_prune_persists_removal_of_dead_leases_only() {
        let dir = tempfile::tempdir().unwrap();
        let (alive_port, dead_port) = unused_adjacent_test_ports();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(
                alive_port,
                100,
                "alive".to_string(),
                None,
                &AlivePids(vec![100]),
            )
            .unwrap();
        store
            .claim(
                dead_port,
                200,
                "dead".to_string(),
                None,
                &AlivePids(vec![100]),
            )
            .unwrap();

        let pruned = store
            .prune(
                AuditActor::new(AuditSource::Watch, None, None),
                &AlivePids(vec![100]),
            )
            .unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].pid, 200);

        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].pid, 100);
    }

    #[test]
    fn store_prune_with_nothing_dead_returns_empty_and_exits_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(
                unused_test_port(),
                100,
                "alive".to_string(),
                None,
                &AlwaysAlive,
            )
            .unwrap();

        let pruned = store
            .prune(
                AuditActor::new(AuditSource::Watch, None, None),
                &AlwaysAlive,
            )
            .unwrap();
        assert!(pruned.is_empty());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    // ---- claim input validation ----

    #[test]
    fn claim_rejects_port_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let err = store
            .claim(0, 100, "server".to_string(), None, &AlwaysAlive)
            .expect_err("port 0 must be rejected");
        assert!(
            err.to_string().contains("port must be between 1 and 65535"),
            "error must name the valid port range, got: {err:#}"
        );
        assert!(
            store.list().unwrap().is_empty(),
            "a rejected claim must not write anything to the store"
        );
    }

    #[test]
    fn claim_accepts_a_tag_at_exactly_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let outcome = store
            .claim(
                unused_test_port(),
                100,
                "a".repeat(MAX_TAG_CHARS),
                None,
                &AlwaysAlive,
            )
            .unwrap();
        assert_eq!(outcome.lease.tag.chars().count(), MAX_TAG_CHARS);
    }

    #[test]
    fn claim_rejects_a_tag_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let err = store
            .claim(3000, 100, "a".repeat(MAX_TAG_CHARS + 1), None, &AlwaysAlive)
            .expect_err("an oversized tag must be rejected");
        assert!(
            err.to_string().contains("tag"),
            "error must say which field is too long, got: {err:#}"
        );
        assert!(
            store.list().unwrap().is_empty(),
            "a rejected claim must not write anything to the store"
        );
    }

    #[test]
    fn claim_accepts_a_session_at_exactly_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let outcome = store
            .claim(
                unused_test_port(),
                100,
                "server".to_string(),
                Some("s".repeat(MAX_SESSION_CHARS)),
                &AlwaysAlive,
            )
            .unwrap();
        assert_eq!(
            outcome.lease.session.as_deref().unwrap().chars().count(),
            MAX_SESSION_CHARS
        );
    }

    #[test]
    fn claim_rejects_a_session_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        let err = store
            .claim(
                3000,
                100,
                "server".to_string(),
                Some("s".repeat(MAX_SESSION_CHARS + 1)),
                &AlwaysAlive,
            )
            .expect_err("an oversized session must be rejected");
        assert!(
            err.to_string().contains("session"),
            "error must say which field is too long, got: {err:#}"
        );
        assert!(
            store.list().unwrap().is_empty(),
            "a rejected claim must not write anything to the store"
        );
    }

    #[test]
    fn claim_limits_count_characters_not_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();

        // MAX_TAG_CHARS multibyte characters: far more bytes than the limit,
        // but exactly the limit in characters, so it must be accepted.
        let outcome = store
            .claim(
                unused_test_port(),
                100,
                "é".repeat(MAX_TAG_CHARS),
                None,
                &AlwaysAlive,
            )
            .unwrap();
        assert_eq!(outcome.lease.tag.chars().count(), MAX_TAG_CHARS);
    }

    // ---- Unix permissions ----

    #[cfg(unix)]
    #[test]
    fn claim_leaves_the_data_dir_and_state_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .claim(
                unused_test_port(),
                100,
                "server".to_string(),
                None,
                &AlwaysAlive,
            )
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

    // ---- transfer ----

    struct TransferChecker {
        entries: std::collections::HashMap<u32, Option<u64>>,
    }

    impl TransferChecker {
        fn new(pairs: Vec<(u32, Option<u64>)>) -> Self {
            Self {
                entries: pairs.into_iter().collect(),
            }
        }

        fn wrapper_and_child(
            wrapper_pid: u32,
            wrapper_start: u64,
            child_pid: u32,
            child_start: Option<u64>,
        ) -> Self {
            let mut entries = std::collections::HashMap::new();
            entries.insert(wrapper_pid, Some(wrapper_start));
            // `None` means the PID is alive but has no resolvable start time.
            // Absence from the map means dead.
            entries.insert(child_pid, child_start);
            Self { entries }
        }
    }

    impl PidChecker for TransferChecker {
        fn is_alive(&self, pid: u32) -> bool {
            self.entries.contains_key(&pid)
        }

        fn process_start_time(&self, pid: u32) -> Option<u64> {
            self.entries.get(&pid).copied().flatten()
        }
    }

    fn seed_wrapper_lease(store: &Store, port: u16, wrapper_pid: u32, wrapper_start: u64) -> Lease {
        // Seed directly instead of claiming: `transfer` never probes the OS,
        // so these tests use no sockets at all and cannot perturb the shared
        // ephemeral-port pool that other tests draw from.
        let lease = Lease::new_with_process_start_time(
            port,
            wrapper_pid,
            "svc",
            Some("sess-1".to_string()),
            Some(wrapper_start),
        );
        store.write_v2_leases(std::slice::from_ref(&lease)).unwrap();
        lease
    }

    #[test]
    fn transfer_replaces_wrapper_pid_with_verified_child_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23200;
        let (wrapper_pid, wrapper_start) = (100u32, 111u64);
        let (child_pid, child_start) = (200u32, 222u64);
        let checker = TransferChecker::wrapper_and_child(
            wrapper_pid,
            wrapper_start,
            child_pid,
            Some(child_start),
        );
        let original = seed_wrapper_lease(&store, port, wrapper_pid, wrapper_start);

        let transferred = store
            .transfer(
                port,
                wrapper_pid,
                wrapper_start,
                child_pid,
                run_actor("run-session"),
                &checker,
            )
            .unwrap();

        assert_eq!(transferred.port, port);
        assert_eq!(transferred.pid, child_pid);
        assert_eq!(transferred.process_start_time, Some(child_start));
        assert_eq!(transferred.tag, original.tag);
        assert_eq!(transferred.session, original.session);
        assert!(transferred.is_alive(&checker));

        let persisted = store.get(port).unwrap().unwrap();
        assert_eq!(persisted, transferred);
    }

    #[test]
    fn transfer_rejects_missing_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23201;
        let checker = TransferChecker::wrapper_and_child(100, 111, 200, Some(222));

        let err = store
            .transfer(port, 100, 111, 200, run_actor("run-session"), &checker)
            .unwrap_err();
        assert!(
            err.to_string().contains("no lease"),
            "unexpected error: {err:#}"
        );
        assert!(store.get(port).unwrap().is_none());
    }

    #[test]
    fn transfer_rejects_changed_wrapper_pid() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23202;
        let checker = TransferChecker::wrapper_and_child(100, 111, 200, Some(222));
        let original = seed_wrapper_lease(&store, port, 100, 111);

        let err = store
            .transfer(port, 101, 111, 200, run_actor("run-session"), &checker)
            .unwrap_err();
        assert!(
            err.to_string().contains("expected owner"),
            "unexpected error: {err:#}"
        );
        assert_eq!(store.get(port).unwrap().unwrap(), original);
    }

    #[test]
    fn transfer_rejects_stale_wrapper_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23203;
        let checker = TransferChecker::wrapper_and_child(100, 111, 200, Some(222));
        let original = seed_wrapper_lease(&store, port, 100, 111);

        let err = store
            .transfer(port, 100, 999, 200, run_actor("run-session"), &checker)
            .unwrap_err();
        assert!(
            err.to_string().contains("expected owner"),
            "unexpected error: {err:#}"
        );
        assert_eq!(store.get(port).unwrap().unwrap(), original);
    }

    #[test]
    fn transfer_rejects_recycled_wrapper_pid() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23204;
        let original = seed_wrapper_lease(&store, port, 100, 111);
        // Wrapper PID was recycled: same PID, different start time now.
        let recycled_checker = TransferChecker::wrapper_and_child(100, 333, 200, Some(222));

        let err = store
            .transfer(
                port,
                100,
                111,
                200,
                run_actor("run-session"),
                &recycled_checker,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("no longer alive"),
            "unexpected error: {err:#}"
        );
        assert_eq!(store.get(port).unwrap().unwrap(), original);
    }

    #[test]
    fn transfer_rejects_dead_child() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23205;
        let original = seed_wrapper_lease(&store, port, 100, 111);
        // Child PID absent from the map => dead.
        let dead_child_checker = TransferChecker::new(vec![(100, Some(111))]);

        let err = store
            .transfer(
                port,
                100,
                111,
                200,
                run_actor("run-session"),
                &dead_child_checker,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("child"),
            "unexpected error: {err:#}"
        );
        assert_eq!(store.get(port).unwrap().unwrap(), original);
    }

    #[test]
    fn transfer_rejects_child_without_start_time_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = 23206;
        let original = seed_wrapper_lease(&store, port, 100, 111);
        // Child alive (present) but start time unresolvable (None).
        let no_identity_checker = TransferChecker::wrapper_and_child(100, 111, 200, None);

        let err = store
            .transfer(
                port,
                100,
                111,
                200,
                run_actor("run-session"),
                &no_identity_checker,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("child"),
            "unexpected error: {err:#}"
        );
        assert_eq!(store.get(port).unwrap().unwrap(), original);
    }

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> Result<u64> {
            Ok(self.0)
        }
    }

    fn history_clear_draft() -> AuditEventDraft {
        AuditEventDraft {
            actor: crate::audit::AuditActor::new(
                crate::audit::AuditSource::Cli,
                None,
                Some("session-a".to_string()),
            ),
            kind: crate::audit::AuditEventKind::HistoryCleared { removed_count: 0 },
        }
    }

    #[test]
    fn state_update_assigns_consecutive_sequences_with_one_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let result = store
            .update_state_with_clock(FixedClock(123), |state| {
                state.leases.clear();
                Ok(StateUpdate::Commit {
                    result: 7,
                    events: vec![history_clear_draft(), history_clear_draft()],
                })
            })
            .unwrap();

        assert_eq!(result, 7);
        let state = store.read_state().unwrap();
        assert_eq!(state.events.len(), 2);
        assert_eq!(state.events[0].sequence, 1);
        assert_eq!(state.events[1].sequence, 2);
        assert_eq!(state.events[0].occurred_at, 123);
        assert_eq!(state.events[1].occurred_at, 123);
        assert_eq!(state.next_event_sequence, 3);
    }

    #[test]
    fn state_update_trims_oldest_events_and_preserves_monotonic_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let events: Vec<_> = (1..=MAX_HISTORY_EVENTS)
            .map(|sequence| v3_event_json(sequence as u64))
            .collect();
        write_v3_fixture(&store, (MAX_HISTORY_EVENTS + 1) as u64, &events);

        store
            .update_state_with_clock(FixedClock(456), |_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: vec![history_clear_draft(), history_clear_draft()],
                })
            })
            .unwrap();

        let state = store.read_state().unwrap();
        assert_eq!(state.events.len(), MAX_HISTORY_EVENTS);
        assert_eq!(state.events[0].sequence, 3);
        assert_eq!(
            state.events.last().unwrap().sequence,
            (MAX_HISTORY_EVENTS + 2) as u64
        );
        assert_eq!(state.next_event_sequence, (MAX_HISTORY_EVENTS + 3) as u64);
    }

    #[test]
    fn state_update_rejects_sequence_exhaustion_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, u64::MAX, &[]);
        let err = store
            .update_state_with_clock(FixedClock(1), |_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: vec![history_clear_draft()],
                })
            })
            .unwrap_err();
        assert!(err.to_string().contains("sequence"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn state_update_rejects_commit_without_event_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let err = store
            .update_state_with_clock(FixedClock(1), |_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: Vec::new(),
                })
            })
            .unwrap_err();
        assert!(err.to_string().contains("event"));
    }

    #[test]
    fn state_update_does_not_write_for_unchanged_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = br#"[{"port":3000,"pid":100,"tag":"legacy","created_at":1,"session":null}]"#;
        std::fs::write(store.state_file_path(), original).unwrap();

        let result = store
            .update_state_with_clock(FixedClock(1), |_| Ok(StateUpdate::Unchanged(9)))
            .unwrap();
        assert_eq!(result, 9);
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn temporary_write_failure_preserves_previous_state_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 1, &[]);
        std::fs::create_dir(
            store
                .data_dir
                .join(format!(".leases.json.tmp.{}", std::process::id())),
        )
        .unwrap();

        let err = store
            .update_state_with_clock(FixedClock(1), |_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: vec![history_clear_draft()],
                })
            })
            .unwrap_err();
        assert!(err.to_string().contains("temporary") || err.to_string().contains("write"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    struct RenameFailFileOps;

    impl FileOps for RenameFailFileOps {
        fn write_temp(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            std::fs::write(path, bytes).map_err(Into::into)
        }

        fn rename_temp(&self, _from: &Path, _to: &Path) -> Result<()> {
            anyhow::bail!("injected rename failure")
        }
    }

    #[test]
    fn rename_failure_preserves_previous_state_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            data_dir: dir.path().to_path_buf(),
            file_ops: std::sync::Arc::new(RenameFailFileOps),
        };
        store.ensure_data_dir().unwrap();
        let original = write_v3_fixture(&store, 1, &[]);

        let err = store
            .update_state_with_clock(FixedClock(1), |_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: vec![history_clear_draft()],
                })
            })
            .unwrap_err();
        assert!(err.to_string().contains("atomically replace"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    fn assert_claim_disposition(
        mut leases: Vec<Lease>,
        requested_port: u16,
        pid: u32,
        checker: &dyn PidChecker,
        expected: ClaimDisposition,
    ) -> ClaimMutation {
        let mutation = claim_in_place(
            &mut leases,
            requested_port,
            pid,
            "claimed".to_string(),
            None,
            checker,
        )
        .unwrap();
        assert_eq!(mutation.disposition, expected);
        mutation
    }

    #[test]
    fn claim_reports_created_without_prior_lease() {
        let port = unused_test_port();
        let mutation = assert_claim_disposition(
            Vec::new(),
            port,
            100,
            &AlwaysAlive,
            ClaimDisposition::Created,
        );
        assert!(mutation.prior_lease.is_none());
        assert!(mutation.replaced_lease.is_none());
    }

    #[test]
    fn claim_reports_updated_with_prior_lease() {
        let port = unused_test_port();
        let prior = Lease::new(port, 100, "old", None);
        let mutation = assert_claim_disposition(
            vec![prior.clone()],
            port,
            100,
            &AlwaysAlive,
            ClaimDisposition::Updated,
        );
        assert_eq!(mutation.prior_lease, Some(prior));
        assert!(mutation.replaced_lease.is_none());
    }

    #[test]
    fn claim_reports_replaced_dead_with_prior_lease() {
        let port = unused_test_port();
        let prior = Lease::new(port, 100, "old", None);
        let mutation = assert_claim_disposition(
            vec![prior.clone()],
            port,
            200,
            &AlwaysDead,
            ClaimDisposition::ReplacedDead,
        );
        assert_eq!(mutation.prior_lease, Some(prior));
        assert!(mutation.replaced_lease.is_none());
    }

    #[test]
    fn claim_reports_reassigned_lease_conflict() {
        let (port, _) = unused_adjacent_test_ports();
        let prior = Lease::new(port, 100, "old", None);
        let mutation = assert_claim_disposition(
            vec![prior.clone()],
            port,
            200,
            &AlivePids(vec![100, 200]),
            ClaimDisposition::ReassignedLeaseConflict,
        );
        assert_eq!(mutation.prior_lease, Some(prior));
        assert!(mutation.replaced_lease.is_none());
    }

    #[test]
    fn claim_reports_reassigned_os_occupied() {
        let (port, _) = unused_adjacent_test_ports();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let mutation = assert_claim_disposition(
            Vec::new(),
            port,
            200,
            &AlwaysAlive,
            ClaimDisposition::ReassignedOsOccupied,
        );
        assert!(mutation.prior_lease.is_none());
        drop(listener);
    }

    #[test]
    fn claim_reassignment_preserves_stale_destination_snapshot() {
        let (requested_port, destination_port) = unused_adjacent_test_ports();
        let prior = Lease::new(requested_port, 100, "requested", None);
        let stale = Lease::new(destination_port, 300, "stale", None);
        let mutation = assert_claim_disposition(
            vec![prior.clone(), stale.clone()],
            requested_port,
            200,
            &AlivePids(vec![100, 200]),
            ClaimDisposition::ReassignedLeaseConflict,
        );
        assert_eq!(mutation.prior_lease, Some(prior));
        assert_eq!(mutation.replaced_lease, Some(stale));
    }

    #[test]
    fn claim_persists_lease_and_claim_event_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let port = unused_test_port();
        let outcome = store
            .claim_with_actor(
                port,
                100,
                "service".to_string(),
                None,
                AuditActor::new(AuditSource::Cli, None, Some("session-a".to_string())),
                &AlwaysAlive,
            )
            .unwrap();

        let state = store.read_state().unwrap();
        assert_eq!(state.leases, vec![outcome.lease.clone()]);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].actor.session.as_deref(), Some("session-a"));
        let AuditEventKind::LeaseClaimed {
            requested_port,
            disposition,
            lease,
            prior_lease,
            replaced_lease,
        } = &state.events[0].kind
        else {
            panic!("claim must persist a lease_claimed event");
        };
        assert_eq!(*requested_port, port);
        assert_eq!(*disposition, ClaimDisposition::Created);
        assert_eq!(lease, &LeaseSnapshot::from(&outcome.lease));
        assert!(prior_lease.is_none());
        assert!(replaced_lease.is_none());
    }

    #[test]
    fn claim_with_invalid_actor_does_not_write_lease_or_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 1, &[]);
        let err = store
            .claim_with_actor(
                unused_test_port(),
                100,
                "service".to_string(),
                None,
                AuditActor::new(
                    AuditSource::Cli,
                    None,
                    Some("s".repeat(MAX_SESSION_CHARS + 1)),
                ),
                &AlwaysAlive,
            )
            .unwrap_err();
        assert!(err.to_string().contains("session"));
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    fn run_actor(session: &str) -> AuditActor {
        AuditActor::new(AuditSource::Run, None, Some(session.to_string()))
    }

    #[test]
    fn transfer_persists_child_lease_and_transfer_event_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let checker = TransferChecker::wrapper_and_child(100, 111, 200, Some(222));
        seed_wrapper_lease(&store, 23220, 100, 111);

        store
            .transfer(23220, 100, 111, 200, run_actor("run-session"), &checker)
            .unwrap();
        let state = store.read_state().unwrap();
        assert_eq!(state.events.len(), 1);
        assert_eq!(
            state.events[0].actor.session.as_deref(),
            Some("run-session")
        );
        assert!(matches!(
            state.events[0].kind,
            AuditEventKind::LeaseTransferred { .. }
        ));
        assert_eq!(state.leases[0].pid, 200);
        assert_eq!(state.leases[0].session.as_deref(), Some("sess-1"));
    }

    #[test]
    fn failed_transfer_does_not_emit_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let checker = TransferChecker::wrapper_and_child(100, 111, 200, Some(222));
        seed_wrapper_lease(&store, 23221, 100, 111);
        let original = std::fs::read(store.state_file_path()).unwrap();

        assert!(
            store
                .transfer(23221, 100, 999, 200, run_actor("run-session"), &checker)
                .is_err()
        );
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
        assert!(store.read_state().unwrap().events.is_empty());
    }

    #[test]
    fn release_persists_removal_and_release_event_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let lease = seed_wrapper_lease(&store, 23222, 100, 111);

        store
            .release(
                23222,
                AuditActor::new(AuditSource::Cli, None, Some("cli-session".into())),
                &AlwaysAlive,
            )
            .unwrap();
        let state = store.read_state().unwrap();
        assert!(state.leases.is_empty());
        assert_eq!(state.events.len(), 1);
        assert_eq!(
            state.events[0].actor.session.as_deref(),
            Some("cli-session")
        );
        assert!(
            matches!(state.events[0].kind, AuditEventKind::LeaseReleased { lease: ref snapshot, .. } if snapshot == &LeaseSnapshot::from(&lease))
        );
    }

    #[test]
    fn release_missing_port_does_not_emit_or_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let original = write_v3_fixture(&store, 1, &[]);

        assert!(
            store
                .release(
                    23223,
                    AuditActor::new(AuditSource::Cli, None, None),
                    &AlwaysAlive
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn prune_persists_one_event_per_removed_lease_in_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        store
            .write_v2_leases(&[
                Lease::new(23224, 100, "one", Some("owner-one".to_string())),
                Lease::new(23225, 200, "two", Some("owner-two".to_string())),
            ])
            .unwrap();

        let pruned = store
            .prune(AuditActor::new(AuditSource::Watch, None, None), &AlwaysDead)
            .unwrap();
        assert_eq!(pruned.len(), 2);
        let state = store.read_state().unwrap();
        assert!(state.leases.is_empty());
        assert_eq!(state.events.len(), 2);
        assert!(
            state
                .events
                .iter()
                .all(|event| event.actor.source == AuditSource::Watch)
        );
        assert!(
            state
                .events
                .iter()
                .all(|event| matches!(event.kind, AuditEventKind::LeasePruned { .. }))
        );
    }

    #[test]
    fn prune_without_dead_leases_does_not_emit_or_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        seed_wrapper_lease(&store, 23226, 100, 111);
        let original = std::fs::read(store.state_file_path()).unwrap();

        assert!(
            store
                .prune(
                    AuditActor::new(AuditSource::Watch, None, None),
                    &TransferChecker::wrapper_and_child(100, 111, 200, Some(222))
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(std::fs::read(store.state_file_path()).unwrap(), original);
    }

    fn append_history(store: &Store, kinds: Vec<AuditEventKind>) {
        store
            .update_state(|_| {
                Ok(StateUpdate::Commit {
                    result: (),
                    events: kinds
                        .into_iter()
                        .map(|kind| AuditEventDraft {
                            actor: AuditActor::new(
                                AuditSource::Cli,
                                None,
                                Some("actor-session".into()),
                            ),
                            kind,
                        })
                        .collect(),
                })
            })
            .unwrap();
    }

    fn history_query(
        session: Option<&str>,
        port: Option<u16>,
        event: Option<AuditEventType>,
        before: Option<u64>,
        limit: usize,
    ) -> HistoryQuery {
        HistoryQuery::try_new(session.map(str::to_owned), port, event, before, limit).unwrap()
    }

    #[test]
    fn history_returns_matching_events_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![
                AuditEventKind::HistoryCleared { removed_count: 0 },
                AuditEventKind::GuardWarned {
                    target: crate::audit::GuardTarget::Port { port: 23000 },
                    reason: crate::audit::GuardWarnReason::UnresolvableProcessName,
                },
            ],
        );

        let page = store
            .history(&history_query(None, None, None, None, 100))
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(!page.has_more);
        assert_eq!(page.next_before, Some(1));
    }

    #[test]
    fn history_combines_filters_with_logical_and() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        let lease = LeaseSnapshot::from(&Lease::new(
            23001,
            1234,
            "tag",
            Some("owner-session".into()),
        ));
        append_history(
            &store,
            vec![
                AuditEventKind::LeaseReleased {
                    lease: lease.clone(),
                    was_alive: false,
                },
                AuditEventKind::LeasePruned { lease },
            ],
        );
        let page = store
            .history(&history_query(
                Some("owner-session"),
                Some(23001),
                Some(AuditEventType::LeasePruned),
                None,
                100,
            ))
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].sequence, 2);
    }

    #[test]
    fn history_session_matches_actor_or_affected_lease_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![AuditEventKind::LeaseReleased {
                lease: LeaseSnapshot::from(&Lease::new(
                    23002,
                    1234,
                    "tag",
                    Some("owner-session".into()),
                )),
                was_alive: false,
            }],
        );
        let page = store
            .history(&history_query(Some("owner-session"), None, None, None, 100))
            .unwrap();
        assert_eq!(page.events.len(), 1);
    }

    #[test]
    fn history_before_sequence_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![
                AuditEventKind::HistoryCleared { removed_count: 0 },
                AuditEventKind::HistoryCleared { removed_count: 0 },
                AuditEventKind::HistoryCleared { removed_count: 0 },
            ],
        );
        let page = store
            .history(&history_query(None, None, None, Some(3), 100))
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].sequence, 2);
    }

    #[test]
    fn history_uses_one_extra_match_to_calculate_has_more() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![
                AuditEventKind::HistoryCleared { removed_count: 0 },
                AuditEventKind::HistoryCleared { removed_count: 0 },
                AuditEventKind::HistoryCleared { removed_count: 0 },
            ],
        );
        let page = store
            .history(&history_query(None, None, None, None, 2))
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.next_before, Some(2));
    }

    #[test]
    fn history_rejects_empty_or_oversized_session_filters() {
        assert!(HistoryQuery::try_new(Some(String::new()), None, None, None, 100).is_err());
        assert!(
            HistoryQuery::try_new(
                Some("x".repeat(MAX_SESSION_CHARS + 1)),
                None,
                None,
                None,
                100,
            )
            .is_err()
        );
    }

    #[test]
    fn history_rejects_limits_outside_one_through_one_thousand() {
        assert!(HistoryQuery::try_new(None, None, None, None, 0).is_err());
        assert!(HistoryQuery::try_new(None, None, None, None, 1001).is_err());
    }

    #[test]
    fn history_before_zero_returns_an_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![AuditEventKind::HistoryCleared { removed_count: 0 }],
        );
        let page = store
            .history(&history_query(None, None, None, Some(0), 100))
            .unwrap();
        assert!(page.events.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn store_clear_history_is_atomic_and_preserves_next_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        append_history(
            &store,
            vec![AuditEventKind::HistoryCleared { removed_count: 0 }],
        );
        let removed = store
            .clear_history(AuditActor::new(
                AuditSource::Cli,
                None,
                Some("clear-session".into()),
            ))
            .unwrap();
        assert_eq!(removed, 1);
        let state = store.read_state().unwrap();
        assert_eq!(state.next_event_sequence, 3);
        assert_eq!(state.events.len(), 1);
        assert!(matches!(
            state.events[0].kind,
            AuditEventKind::HistoryCleared { removed_count: 1 }
        ));
        assert_eq!(
            state.events[0].actor.session.as_deref(),
            Some("clear-session")
        );
    }

    #[test]
    fn store_clear_history_on_empty_state_records_zero_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            store
                .clear_history(AuditActor::new(AuditSource::Cli, None, None))
                .unwrap(),
            0
        );
        let state = store.read_state().unwrap();
        assert_eq!(state.next_event_sequence, 2);
        assert!(matches!(
            state.events[0].kind,
            AuditEventKind::HistoryCleared { removed_count: 0 }
        ));
    }
}
