//! Locked file-intent store: which session is currently editing which file.
//!
//! File intents answer a different question than port leases ("who owns this
//! port?"), so they live in their own file and their own lock rather than as a
//! new kind of lease. That keeps the `leases.json` format untouched and means
//! a slow file-intent write never serializes behind a port claim.
//!
//! # Key model
//!
//! An intent is keyed by [`key_for_file`]:
//!
//! - inside a git work tree: `<git-common-dir>:<path relative to the worktree>`
//! - everywhere else: the absolute path
//!
//! The common dir — not the worktree root — is the repository identity, so
//! `src/auth.rs` claimed from the main checkout and from a linked worktree
//! share one key. That is exactly the collision this store exists to catch.
//!
//! # Expiry
//!
//! Every intent carries a `created_at` and an `expires_at` (Unix seconds). A
//! record is live while `now < expires_at`; expiry is evaluated at read time by
//! the caller, never by a background task. There is no heartbeat and no daemon:
//! a crashed session keeps its file until the TTL lapses, which bounds the
//! damage without adding a moving part. `claim` reaps expired records under the
//! lock, so the file does not accumulate stale entries forever.

use crate::store::{MAX_SESSION_CHARS, Store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

/// Version of the `intents.json` envelope. Bump it when the stored shape
/// changes incompatibly so an older binary refuses to touch a newer file
/// instead of corrupting it.
const INTENT_FORMAT_VERSION: u32 = 1;

/// Upper bound on a normalized intent key, in CHARACTERS. A key normally
/// holds a repository path plus a relative path; 8192 characters is far
/// beyond any legitimate value and keeps a hostile hook payload from growing
/// `intents.json` (and every listing) without limit.
pub const MAX_INTENT_KEY_CHARS: usize = 8192;

/// A claim that `session` is editing `key` until `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Intent {
    pub key: String,
    pub session: String,
    /// Unix timestamp when this ownership window started. Refreshing a
    /// same-session claim keeps the original value so the age of the claim
    /// stays visible.
    pub created_at: u64,
    /// Unix timestamp after which the claim is no longer binding.
    pub expires_at: u64,
}

impl Intent {
    /// Returns `true` while the claim is binding at `now`.
    ///
    /// The boundary is exclusive: an intent with `expires_at == now` is
    /// already expired. Using one rule for both `list` and `claim` keeps
    /// "visible" and "stealable" from disagreeing at the exact second.
    #[allow(dead_code)]
    pub fn is_live_at(&self, now: u64) -> bool {
        now < self.expires_at
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredIntents {
    format_version: u32,
    intents: Vec<Intent>,
}

/// Why an intent operation was refused.
///
/// These are typed — rather than `anyhow` — because callers branch on them:
/// the hook adapter turns [`IntentError::HeldBy`] into a deny message that
/// names the holder, while store failures are reported generically. `Store`
/// wraps everything else (lock, I/O, malformed JSON) so short-circuiting with
/// `?` stays ergonomic inside this module.
#[derive(Debug)]
#[allow(dead_code)]
pub enum IntentError {
    /// Another live session owns the key.
    HeldBy { session: String, expires_at: u64 },
    /// A release was attempted by a session that does not own the key.
    NotOwner { session: String, expires_at: u64 },
    /// The caller passed an input this store refuses.
    Invalid(String),
    /// The underlying locked store failed.
    Store(anyhow::Error),
}

impl fmt::Display for IntentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeldBy {
                session,
                expires_at,
            } => write!(
                formatter,
                "file intent is held by session {session} until {expires_at}"
            ),
            Self::NotOwner {
                session,
                expires_at,
            } => write!(
                formatter,
                "file intent is held by session {session} until {expires_at}, not by the releasing session"
            ),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for IntentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for IntentError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

/// Result of an intent operation whose failures callers may need to match on.
pub type IntentResult<T> = std::result::Result<T, IntentError>;

/// A locked JSON-backed store of file intents rooted at a data directory.
///
/// Mirrors [`Store`](crate::store::Store): the only in-memory state is the data
/// directory, every read-modify-write runs under an exclusive `fs4` lock on
/// `intents.json.lock`, and commits replace `intents.json` atomically through a
/// temp file plus rename. Reads take the lock too, so a reader never observes a
/// partially written file or races a rename.
#[derive(Clone)]
#[allow(dead_code)]
pub struct IntentStore {
    data_dir: PathBuf,
}

#[allow(dead_code)]
impl IntentStore {
    /// Opens the intent store, resolving the same data directory as the lease
    /// store (`PORTZILLA_DATA_DIR`, then XDG/HOME) unless `override_dir` is
    /// given. The directory is created owner-only on Unix.
    pub fn open(override_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = Store::resolve_data_dir(
            override_dir,
            std::env::var("PORTZILLA_DATA_DIR").ok(),
            std::env::var("XDG_DATA_HOME").ok(),
            std::env::var("HOME").ok(),
        )?;
        let store = Self { data_dir };
        store.ensure_data_dir()?;
        Ok(store)
    }

    /// Path to the JSON intents file inside the data directory.
    pub fn state_file_path(&self) -> PathBuf {
        self.data_dir.join("intents.json")
    }

    fn lock_file_path(&self) -> PathBuf {
        self.data_dir.join("intents.json.lock")
    }

    /// Records `session` as the editor of `key` until `expires_at`.
    ///
    /// The rules, applied while the exclusive lock is held:
    ///
    /// - no record, or an expired one: open a new window starting at `now`
    /// - the same session on a live record: refresh `expires_at` and keep the
    ///   original `created_at`, so the age of the claim stays visible
    /// - a different session on a live record: refuse with
    ///   [`IntentError::HeldBy`], naming the current holder
    ///
    /// While the file is open for writing, expired records for other keys are
    /// dropped too. `claim` is the only mutation path, so it doubles as the
    /// reaper; that is what keeps `intents.json` bounded without a background
    /// task.
    pub fn claim(
        &self,
        key: &str,
        session: &str,
        now: u64,
        expires_at: u64,
    ) -> IntentResult<Intent> {
        validate_claim_inputs(key, session, now, expires_at)?;
        let _guard = self.lock_exclusive()?;
        let mut intents = self.read_intents_unlocked()?;
        let existing = intents
            .iter()
            .find(|intent| intent.key == key)
            .map(|intent| (intent.session.clone(), intent.created_at, intent.expires_at));
        let intent = match existing {
            Some((holder, _, holder_expires_at))
                if holder_expires_at > now && holder != session =>
            {
                return Err(IntentError::HeldBy {
                    session: holder,
                    expires_at: holder_expires_at,
                });
            }
            Some((_, created_at, holder_expires_at)) if holder_expires_at > now => Intent {
                key: key.to_string(),
                session: session.to_string(),
                created_at,
                expires_at,
            },
            _ => Intent {
                key: key.to_string(),
                session: session.to_string(),
                created_at: now,
                expires_at,
            },
        };
        // Expired entries are reaped here, including an expired record for
        // `key`, which the new intent below replaces.
        intents.retain(|stored| stored.key == key || stored.is_live_at(now));
        match intents.iter().position(|stored| stored.key == key) {
            Some(index) => intents[index] = intent.clone(),
            None => intents.push(intent.clone()),
        }
        self.write_intents_unlocked(&intents)?;
        Ok(intent)
    }

    /// Returns the stored record for `key`, if any.
    ///
    /// Expiry is deliberately NOT applied here: the caller supplies the clock
    /// (`list(now)`, `claim(..., now, ...)`) so tests and hook adapters can use
    /// synthetic timestamps. A record returned by `get` may already be expired.
    pub fn get(&self, key: &str) -> IntentResult<Option<Intent>> {
        let _guard = self.lock_exclusive()?;
        let intents = self.read_intents_unlocked()?;
        Ok(intents.into_iter().find(|intent| intent.key == key))
    }

    /// Returns every intent that is still live at `now`.
    ///
    /// Expired records are filtered in memory, NOT persisted. A read holding
    /// the lock for a moment is fine; turning it into a write would make a
    /// monitoring call contend with real claims and widen the window for a
    /// failed rename. The next `claim` reaps the entries under the lock, so the
    /// file still stays bounded.
    pub fn list(&self, now: u64) -> IntentResult<Vec<Intent>> {
        let _guard = self.lock_exclusive()?;
        let intents = self.read_intents_unlocked()?;
        Ok(intents
            .into_iter()
            .filter(|intent| intent.is_live_at(now))
            .collect())
    }

    /// Removes the record for `key` when `session` owns it.
    ///
    /// Returns `Ok(None)` when no record exists: release is idempotent, so a
    /// session retrying after a crash does not have to tell "already gone" from
    /// success. A different session is refused with [`IntentError::NotOwner`],
    /// naming the current holder.
    ///
    /// Expiry is not consulted here because the call carries no clock. That is
    /// harmless: a lapsed record blocks nothing, since the next claim takes it
    /// over, and its owner may still release it early.
    pub fn release(&self, key: &str, session: &str) -> IntentResult<Option<Intent>> {
        let _guard = self.lock_exclusive()?;
        let mut intents = self.read_intents_unlocked()?;
        let Some(index) = intents.iter().position(|intent| intent.key == key) else {
            return Ok(None);
        };
        let existing = &intents[index];
        if existing.session != session {
            return Err(IntentError::NotOwner {
                session: existing.session.clone(),
                expires_at: existing.expires_at,
            });
        }
        let removed = intents.remove(index);
        self.write_intents_unlocked(&intents)?;
        Ok(Some(removed))
    }

    /// Ensures the data directory exists and is owner-only (`0700` on Unix),
    /// matching the lease store. Intent keys reveal repository and file names,
    /// so this file is not world-readable.
    fn ensure_data_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).with_context(|| {
            format!(
                "failed to create data directory at {}",
                self.data_dir.display()
            )
        })?;
        harden_dir_permissions(&self.data_dir)
    }

    /// Acquires the exclusive lock guarding `intents.json`. Released when the
    /// returned handle drops.
    fn lock_exclusive(&self) -> Result<fs::File> {
        self.ensure_data_dir()?;
        let lock_path = self.lock_file_path();
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file at {}", lock_path.display()))?;
        harden_file_permissions(&lock_path)?;
        // Explicit trait call (not `std::fs::File::lock`) to stay on the
        // dependency whose cross-platform semantics this project already uses.
        fs4::FileExt::lock(&lock_file)
            .context("failed to acquire exclusive lock on the intent store")?;
        Ok(lock_file)
    }

    fn read_intents_unlocked(&self) -> Result<Vec<Intent>> {
        let path = self.state_file_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read intents file at {}", path.display()));
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        let stored: StoredIntents = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "intents file at {} contains invalid JSON and was not modified",
                path.display()
            )
        })?;
        if stored.format_version != INTENT_FORMAT_VERSION {
            bail!(
                "unsupported intents file format version {} at {}; expected {}",
                stored.format_version,
                path.display(),
                INTENT_FORMAT_VERSION
            );
        }
        validate_stored_intents(&stored.intents, &path)?;
        Ok(stored.intents)
    }

    fn write_intents_unlocked(&self, intents: &[Intent]) -> Result<()> {
        let path = self.state_file_path();
        let tmp_path = self
            .data_dir
            .join(format!(".intents.json.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&StoredIntents {
            format_version: INTENT_FORMAT_VERSION,
            intents: intents.to_vec(),
        })
        .context("failed to serialize file intents to JSON")?;
        fs::write(&tmp_path, &json).with_context(|| {
            format!(
                "failed to write temporary intents file at {}",
                tmp_path.display()
            )
        })?;
        // Harden before the rename: rename preserves the mode, so this also
        // leaves the final `intents.json` owner-only (0600).
        harden_file_permissions(&tmp_path)?;
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "failed to atomically replace intents file at {}",
                path.display()
            )
        })?;
        Ok(())
    }
}

/// Rejects claim inputs before anything is read or written, so a refused claim
/// never touches the store. An empty or absurdly long key/session is refused
/// for the same reason the lease store bounds its tags: these values are
/// persisted and echoed back by every listing. `expires_at` must be in the
/// future, otherwise the "claim" would be born expired and could never bind.
fn validate_claim_inputs(
    key: &str,
    session: &str,
    now: u64,
    expires_at: u64,
) -> IntentResult<()> {
    if key.is_empty() {
        return Err(IntentError::Invalid(
            "intent key must not be empty".to_string(),
        ));
    }
    if key.chars().count() > MAX_INTENT_KEY_CHARS {
        return Err(IntentError::Invalid(format!(
            "invalid intent key: at most {MAX_INTENT_KEY_CHARS} characters, got {}",
            key.chars().count()
        )));
    }
    if session.is_empty() {
        return Err(IntentError::Invalid(
            "intent session must not be empty".to_string(),
        ));
    }
    if session.chars().count() > MAX_SESSION_CHARS {
        return Err(IntentError::Invalid(format!(
            "invalid intent session: at most {MAX_SESSION_CHARS} characters, got {}",
            session.chars().count()
        )));
    }
    if expires_at <= now {
        return Err(IntentError::Invalid(format!(
            "intent expiry {expires_at} must be after the claim time {now}"
        )));
    }
    Ok(())
}

/// Rejects a persisted file with two records for one key: the store's core
/// invariant is one owner per key, so such a file is corrupt rather than a
/// list of competing claims. The file is left untouched on rejection.
fn validate_stored_intents(intents: &[Intent], path: &Path) -> Result<()> {
    let mut keys = std::collections::HashSet::with_capacity(intents.len());
    for intent in intents {
        if !keys.insert(intent.key.as_str()) {
            bail!(
                "intents file at {} contains duplicate intent key {:?} and was not modified",
                path.display(),
                intent.key
            );
        }
    }
    Ok(())
}

/// Restricts a directory to owner-only access (`0700`) on Unix. Mirrors the
/// lease store's helper; it is duplicated rather than shared because the lease
/// store's copy is private and this module must not touch `store.rs`.
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

/// Restricts a file to owner-only access (`0600`) on Unix.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Deref;

    /// Owns the temporary directory alongside the store so the directory
    /// outlives every operation the test performs on it.
    struct TempStore {
        _dir: tempfile::TempDir,
        store: IntentStore,
    }

    impl Deref for TempStore {
        type Target = IntentStore;

        fn deref(&self) -> &Self::Target {
            &self.store
        }
    }

    fn temp_store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
        TempStore { _dir: dir, store }
    }

    #[test]
    fn claim_then_get_returns_the_same_intent() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
        store.claim("repo:src/auth.rs", "session-a", 1_000, 1_800).unwrap();
        let got = store.get("repo:src/auth.rs").unwrap().unwrap();
        assert_eq!(got.session, "session-a");
        assert_eq!(got.expires_at, 1_800);
    }

    #[test]
    fn other_session_cannot_claim_a_live_intent() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let err = store.claim("repo:a.rs", "session-b", 1_100, 1_900).unwrap_err();
        assert!(matches!(err, IntentError::HeldBy { ref session, .. } if session == "session-a"));
    }

    #[test]
    fn same_session_refreshes_its_own_intent() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        store.claim("repo:a.rs", "session-a", 1_500, 2_300).unwrap();
        let got = store.get("repo:a.rs").unwrap().unwrap();
        assert_eq!(got.expires_at, 2_300);
        assert_eq!(got.created_at, 1_000);
    }

    #[test]
    fn expired_intent_can_be_taken_by_another_session() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        store.claim("repo:a.rs", "session-b", 1_801, 3_601).unwrap();
        assert_eq!(store.get("repo:a.rs").unwrap().unwrap().session, "session-b");
    }

    #[test]
    fn release_only_by_owner() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        assert!(store.release("repo:a.rs", "session-b").is_err());
        store.release("repo:a.rs", "session-a").unwrap();
        assert!(store.get("repo:a.rs").unwrap().is_none());
    }

    #[test]
    fn list_drops_expired_entries_on_read() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        assert!(store.list(2_000).unwrap().is_empty());
    }

    #[test]
    fn list_keeps_live_entries_while_dropping_expired_ones() {
        let store = temp_store();
        store.claim("repo:live.rs", "session-a", 1_000, 1_800).unwrap();
        store.claim("repo:stale.rs", "session-b", 500, 1_500).unwrap();
        let listed = store.list(1_600).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "repo:live.rs");
    }
}
