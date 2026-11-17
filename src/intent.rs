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
//! Keys are opaque: [`key_for_file`] produces them and the store compares them,
//! nothing parses them. The `:` separator is ambiguous on Windows
//! (`C:\repo\.git:src\auth.rs`), so never split a key on it.
//!
//! # Expiry
//!
//! Every intent carries a `created_at` and an `expires_at` (Unix seconds). A
//! record is live while `now < expires_at`; expiry is evaluated at read time by
//! the caller, never by a background task. There is no heartbeat and no daemon:
//! a crashed session keeps its file until the TTL lapses, which bounds the
//! damage without adding a moving part. `claim` reaps expired records under the
//! lock, so the file does not accumulate stale entries forever.
//!
//! # Reaping versus the lease store
//!
//! `claim` drops expired records for every key while it holds the lock. That is
//! a deliberate divergence from [`Store`](crate::store::Store), which exposes an
//! explicit `prune` instead of pruning as a side effect of `claim`. The
//! tradeoff: reaping keeps `intents.json` free of stale entries without a daemon
//! and without a separate command for users to remember, at the cost of a
//! mutating call doing housekeeping.
//!
//! The safety rule is narrower than "only one mutation path": `claim` is the
//! only operation allowed to delete records it does not own. `release` also
//! mutates the file, but it removes only the caller's own record. If another
//! operation ever needs to delete records it does not own (an `intent prune`
//! command, a transfer, a bulk import), the reaping must move into that
//! explicit operation and `claim` must stop doing it.

use crate::store::{MAX_SESSION_CHARS, Store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Version of the `intents.json` envelope. Bump it when the stored shape
/// changes incompatibly so an older binary refuses to touch a newer file
/// instead of corrupting it.
const INTENT_FORMAT_VERSION: u32 = 1;

/// Upper bound on a normalized intent key, in CHARACTERS. A key normally
/// holds a repository path plus a relative path; 8192 characters is far beyond
/// any legitimate value. This bounds the size of a single record, not the file:
/// the number of records is bounded separately by [`MAX_INTENTS`] and reaping.
pub const MAX_INTENT_KEY_CHARS: usize = 8192;

/// Maximum number of records `intents.json` may hold. Matches the 65,536-entry
/// port-space precedent: far beyond the number of files a set of agent sessions
/// edits inside one TTL window, while keeping a single listing bounded.
const MAX_INTENTS: usize = 65_536;

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
    fn is_live_at(&self, now: u64) -> bool {
        now < self.expires_at
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredIntents {
    format_version: u32,
    intents: Vec<Intent>,
}

/// Filesystem seam mirroring the lease store's: production code writes and
/// renames for real, while a test can inject a rename failure and prove that a
/// failed commit leaves the previous `intents.json` bytes untouched.
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

/// Returns the intent key for `file` as seen from `cwd`.
///
/// Inside a git work tree the key is `<git-common-dir>:<repo-relative-path>`.
/// Repository identity is the *common* dir, not the worktree root, so the same
/// relative file in two linked worktrees of one repository produces one key —
/// the collision this store exists to catch. Outside a git repository, or when
/// any git call fails, the key is the canonical absolute path of `file`.
///
/// Both sides of the key are canonicalized. Git needs its relative output
/// resolved (`--git-common-dir` prints `.git` at the top of the main worktree
/// and `../../.git` from a nested directory), and the file is resolved through
/// its deepest existing component so a file that does not exist yet still gets
/// the key its future self will have. Without that, two spellings of one
/// location — `/var` versus `/private/var`, or a path through a symlinked
/// directory — would produce different keys, and a peer's claim would be
/// silently invisible.
///
/// Keys are opaque and must never be parsed; see the module documentation.
pub fn key_for_file(cwd: &Path, file: &Path) -> String {
    let cwd = canonicalize_or_owned(cwd);
    let absolute = canonicalize_deepest_existing(&absolute_path(&cwd, file));
    let outside_git = || absolute.to_string_lossy().into_owned();

    let Some(git_paths) = git_stdout(&cwd, &["rev-parse", "--show-toplevel", "--git-common-dir"])
    else {
        return outside_git();
    };
    let mut lines = git_paths.lines();
    let (Some(toplevel), Some(common_dir)) = (lines.next(), lines.next()) else {
        return outside_git();
    };

    let toplevel = canonicalize_or_owned(Path::new(toplevel));
    let Some(relative) = relative_to(&absolute, &toplevel) else {
        return outside_git();
    };
    let common_dir = resolve_common_dir(&cwd, Path::new(common_dir));
    format!(
        "{}:{}",
        common_dir.to_string_lossy(),
        relative.to_string_lossy()
    )
}

/// Runs `git -C cwd <args>` and returns the non-empty stdout with trailing
/// newlines removed. Returns `None` when git is missing, exits non-zero, or
/// prints nothing: outside a repository that is a normal fallback, not an
/// error.
///
/// Plumbing variables are cleared from the child environment so a hook fired
/// inside git tooling cannot override `-C` and change the resulting key.
fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    // A fork that lands while another thread holds a TCP listener duplicates
    // that fd until the child execs, so the parent's `drop` cannot release the
    // port and the next bind reports it occupied. In tests this makes the
    // socket-based assertions flaky under load; serialize forks against those
    // tests by taking the same lock. Production never shares its process with
    // them, so the guard compiles out.
    #[cfg(test)]
    let _fork_guard = crate::store::port_test_support::lock();
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths = stdout.trim_end_matches(['\r', '\n']);
    (!paths.is_empty()).then(|| paths.to_string())
}

/// Resolves the path printed by `rev-parse --git-common-dir`. Relative output
/// is relative to `cwd` (the directory git was pointed at), and the result is
/// canonicalized so every worktree of one repository agrees on one string.
fn resolve_common_dir(cwd: &Path, printed: &Path) -> PathBuf {
    if printed.is_absolute() {
        canonicalize_or_owned(printed)
    } else {
        canonicalize_or_owned(&cwd.join(printed))
    }
}

/// Returns `file` as an absolute path relative to `cwd`, normalized lexically.
/// Symlinks are resolved afterwards, in [`canonicalize_deepest_existing`]; this
/// step only anchors the path and removes `.` / `..` components.
fn absolute_path(cwd: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        return normalize_path(file);
    }
    // A relative `cwd` (rare, but valid) is first anchored to the process
    // directory so the fallback key is always absolute.
    let base = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    };
    normalize_path(&base.join(file))
}

/// Lexically removes `.` and `..` components. `..` that crosses a symlink can
/// name a different directory than the OS would follow; the caller resolves the
/// existing prefix afterwards, which corrects that wherever the target exists.
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Resolves `path` through its deepest existing component and appends the
/// non-existent remainder.
///
/// Claims are routinely made for a file that is about to be created, so the
/// whole path cannot be canonicalized. Resolving the part that does exist is
/// enough to make two spellings of one location agree: `/var` versus
/// `/private/var` on macOS, or a path that crosses a symlinked directory.
fn canonicalize_deepest_existing(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonicalize_deepest_existing(parent).join(name),
        _ => normalize_path(path),
    })
}

/// Returns `file` relative to `toplevel` when it is inside. Both sides are
/// canonical by the time this runs — `file` through its deepest existing
/// component and `toplevel` from git — so a lexical comparison is exact and no
/// second canonicalization pass is needed.
fn relative_to(file: &Path, toplevel: &Path) -> Option<PathBuf> {
    file.strip_prefix(toplevel)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Canonicalizes `path`, falling back to its lexical normalization when it
/// cannot be resolved (for example because it does not exist yet).
fn canonicalize_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
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
    HeldBy {
        key: String,
        holder: String,
        expires_at: u64,
    },
    /// A release was attempted by a session that does not own the key.
    NotOwner {
        key: String,
        holder: String,
        requester: String,
        expires_at: u64,
    },
    /// The store already holds the maximum number of records.
    Capacity { max: usize },
    /// The caller passed an input this store refuses.
    Invalid(String),
    /// The underlying locked store failed.
    Store(anyhow::Error),
}

impl fmt::Display for IntentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeldBy { key, holder, .. } => {
                write!(
                    formatter,
                    "file intent for {key} is held by session {holder}"
                )
            }
            Self::NotOwner {
                key,
                holder,
                requester,
                ..
            } => write!(
                formatter,
                "cannot release file intent for {key}: it is owned by session {holder}, not by session {requester}"
            ),
            Self::Capacity { max } => write!(
                formatter,
                "intent store already holds the maximum of {max} records"
            ),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Store(error) => write!(formatter, "{error:#}"),
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
    file_ops: Arc<dyn FileOps>,
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
        let store = Self {
            data_dir,
            file_ops: Arc::new(RealFileOps),
        };
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
    /// - no record, or a record that is not live at `now`: open a new window
    ///   starting at `now`
    /// - the same session on a live record: refresh `expires_at` and keep the
    ///   original `created_at`, so the age of the claim stays visible
    /// - a different live session: refuse with [`IntentError::HeldBy`]
    ///
    /// While the file is open for writing, expired records for other keys are
    /// dropped too. `claim` is the only operation allowed to delete records it
    /// does not own, so it doubles as the reaper. A new key is refused once the
    /// store holds [`MAX_INTENTS`] records; refreshing an existing key still
    /// works at the cap because it replaces its own record.
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
        let position = intents.iter().position(|stored| stored.key == key);
        let live_holder = position
            .map(|index| intents[index].clone())
            .filter(|stored| stored.is_live_at(now));
        let intent = match live_holder {
            Some(holder) if holder.session != session => {
                return Err(IntentError::HeldBy {
                    key: key.to_string(),
                    holder: holder.session,
                    expires_at: holder.expires_at,
                });
            }
            Some(holder) => Intent {
                key: key.to_string(),
                session: session.to_string(),
                created_at: holder.created_at,
                expires_at,
            },
            None => Intent {
                key: key.to_string(),
                session: session.to_string(),
                created_at: now,
                expires_at,
            },
        };
        // Expired entries are reaped here, including an expired record for
        // `key`, which the new intent below replaces.
        intents.retain(|stored| stored.key == key || stored.is_live_at(now));
        if position.is_none() && intents.len() >= MAX_INTENTS {
            return Err(IntentError::Capacity { max: MAX_INTENTS });
        }
        match intents.iter().position(|stored| stored.key == key) {
            Some(index) => intents[index] = intent.clone(),
            None => intents.push(intent.clone()),
        }
        self.write_intents_unlocked(&intents)?;
        Ok(intent)
    }

    /// Returns the stored record for `key`, if any.
    ///
    /// This reports the record exactly as persisted, expiry included: it takes
    /// no clock. Liveness is a caller concern — use [`IntentStore::list`] for
    /// the live view at a given time.
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
    /// naming the holder and the requester.
    ///
    /// Expiry is not consulted here because the call carries no clock. That is
    /// harmless: a lapsed record blocks nothing, since the next claim takes it
    /// over, and its owner may still release it early.
    ///
    /// This path must never reap: deleting a record this session does not own
    /// would break the single-owner contract that makes `claim`'s reaping safe.
    pub fn release(&self, key: &str, session: &str) -> IntentResult<Option<Intent>> {
        let _guard = self.lock_exclusive()?;
        let mut intents = self.read_intents_unlocked()?;
        let Some(index) = intents.iter().position(|intent| intent.key == key) else {
            return Ok(None);
        };
        let existing = &intents[index];
        if existing.session != session {
            return Err(IntentError::NotOwner {
                key: key.to_string(),
                holder: existing.session.clone(),
                requester: session.to_string(),
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
        self.file_ops
            .write_temp(&tmp_path, &json)
            .with_context(|| {
                format!(
                    "failed to write temporary intents file at {}",
                    tmp_path.display()
                )
            })?;
        // Harden before the rename: rename preserves the mode, so this also
        // leaves the final `intents.json` owner-only (0600).
        harden_file_permissions(&tmp_path)?;
        self.file_ops
            .rename_temp(&tmp_path, &path)
            .with_context(|| {
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
fn validate_claim_inputs(key: &str, session: &str, now: u64, expires_at: u64) -> IntentResult<()> {
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

/// Rejects a persisted file that violates the store's invariants: more records
/// than [`MAX_INTENTS`], or two records for one key. The first is a resource
/// bound, so a corrupted or hostile file cannot force `list` to materialize
/// millions of entries; the second is the single-owner rule, so such a file is
/// corrupt rather than a list of competing claims. The file is left untouched
/// on rejection.
fn validate_stored_intents(intents: &[Intent], path: &Path) -> Result<()> {
    if intents.len() > MAX_INTENTS {
        bail!(
            "intents file at {} contains more than {MAX_INTENTS} records and was not modified",
            path.display()
        );
    }
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
    use std::process::Command;

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

    impl TempStore {
        fn dir_path(&self) -> PathBuf {
            self._dir.path().to_path_buf()
        }
    }

    fn temp_store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
        TempStore { _dir: dir, store }
    }

    /// Writes `count` synthetic records directly, bypassing `claim` so a full
    /// store can be reached without thousands of write cycles.
    fn seed_intents_file(store: &IntentStore, count: usize, expires_at: u64) {
        let intents: Vec<Intent> = (0..count)
            .map(|index| Intent {
                key: format!("repo:file{index}.rs"),
                session: format!("session-{index}"),
                created_at: 1_000,
                expires_at,
            })
            .collect();
        let json = serde_json::to_vec(&StoredIntents {
            format_version: INTENT_FORMAT_VERSION,
            intents,
        })
        .unwrap();
        fs::write(store.state_file_path(), json).unwrap();
    }

    #[test]
    fn claim_then_get_returns_the_same_intent() {
        let store = temp_store();
        store
            .claim("repo:src/auth.rs", "session-a", 1_000, 1_800)
            .unwrap();
        let got = store.get("repo:src/auth.rs").unwrap().unwrap();
        assert_eq!(got.session, "session-a");
        assert_eq!(got.expires_at, 1_800);
    }

    #[test]
    fn other_session_cannot_claim_a_live_intent() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let err = store
            .claim("repo:a.rs", "session-b", 1_100, 1_900)
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("repo:a.rs"));
        assert!(message.contains("session-a"));
        assert!(matches!(
            err,
            IntentError::HeldBy {
                ref key,
                ref holder,
                expires_at,
            } if key == "repo:a.rs" && holder == "session-a" && expires_at == 1_800
        ));
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
        assert_eq!(
            store.get("repo:a.rs").unwrap().unwrap().session,
            "session-b"
        );
    }

    #[test]
    fn release_only_by_owner() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let err = store.release("repo:a.rs", "session-b").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("repo:a.rs"));
        assert!(message.contains("session-a"));
        assert!(message.contains("session-b"));
        // Absolute expiries stay structured; they are not for human prose.
        assert!(!message.contains("1800"));
        assert!(matches!(
            err,
            IntentError::NotOwner {
                ref key,
                ref holder,
                ref requester,
                expires_at,
            } if key == "repo:a.rs"
                && holder == "session-a"
                && requester == "session-b"
                && expires_at == 1_800
        ));
        store.release("repo:a.rs", "session-a").unwrap();
        assert!(store.get("repo:a.rs").unwrap().is_none());
    }

    #[test]
    fn release_of_an_absent_key_is_idempotent() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let before = fs::read(store.state_file_path()).unwrap();
        assert!(matches!(
            store.release("repo:never-claimed.rs", "any-session"),
            Ok(None)
        ));
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);
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
        store
            .claim("repo:live.rs", "session-a", 1_000, 1_800)
            .unwrap();
        store
            .claim("repo:stale.rs", "session-b", 500, 1_500)
            .unwrap();
        let listed = store.list(1_600).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "repo:live.rs");
    }

    #[test]
    fn list_does_not_persist_pruning() {
        let store = temp_store();
        store
            .claim("repo:live.rs", "session-a", 1_000, 3_000)
            .unwrap();
        store
            .claim("repo:stale.rs", "session-b", 1_000, 1_500)
            .unwrap();
        let before = fs::read(store.state_file_path()).unwrap();
        assert_eq!(store.list(2_000).unwrap().len(), 1);
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);
    }

    #[test]
    fn claiming_one_key_reaps_expired_records_but_keeps_live_ones() {
        let store = temp_store();
        store
            .claim("repo:expired.rs", "session-b", 1_000, 1_500)
            .unwrap();
        store
            .claim("repo:live.rs", "session-c", 1_000, 3_000)
            .unwrap();
        store
            .claim("repo:claimed.rs", "session-a", 1_600, 2_000)
            .unwrap();

        // The first record is gone from disk, not just from a listing.
        assert!(store.get("repo:expired.rs").unwrap().is_none());
        assert!(store.get("repo:live.rs").unwrap().is_some());
        assert_eq!(
            store.get("repo:claimed.rs").unwrap().unwrap().session,
            "session-a"
        );
    }

    #[test]
    fn intents_file_uses_the_versioned_envelope() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let path = store.state_file_path();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "intents.json");

        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["format_version", "intents"]);
        assert_eq!(value["format_version"].as_u64(), Some(1));
        assert_eq!(value["intents"].as_array().unwrap().len(), 1);
    }

    struct RenameFailFileOps;

    impl FileOps for RenameFailFileOps {
        fn write_temp(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            fs::write(path, bytes).map_err(Into::into)
        }

        fn rename_temp(&self, _from: &Path, _to: &Path) -> Result<()> {
            anyhow::bail!("injected rename failure")
        }
    }

    #[test]
    fn rename_failure_preserves_previous_state_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
        let before = fs::read(store.state_file_path()).unwrap();

        let failing = IntentStore {
            data_dir: dir.path().to_path_buf(),
            file_ops: Arc::new(RenameFailFileOps),
        };
        let err = failing
            .claim("repo:b.rs", "session-b", 1_000, 1_800)
            .unwrap_err();
        assert!(format!("{err:#}").contains("injected rename failure"));
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);
    }

    #[test]
    fn a_new_key_is_refused_when_the_store_is_full() {
        let store = temp_store();
        seed_intents_file(&store, MAX_INTENTS, 10_000);
        let before = fs::read(store.state_file_path()).unwrap();

        let err = store
            .claim("repo:new.rs", "session-new", 1_000, 2_000)
            .unwrap_err();
        assert!(matches!(err, IntentError::Capacity { max: MAX_INTENTS }));
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);

        // An existing key still refreshes: it replaces its own record, so the
        // record count does not grow.
        store
            .claim("repo:file0.rs", "session-0", 1_000, 20_000)
            .unwrap();
        assert_eq!(
            store.get("repo:file0.rs").unwrap().unwrap().expires_at,
            20_000
        );
    }

    #[test]
    fn a_file_above_the_record_cap_reads_as_an_error() {
        let store = temp_store();
        seed_intents_file(&store, MAX_INTENTS + 1, 10_000);
        let before = fs::read(store.state_file_path()).unwrap();

        let err = store.list(1_000).unwrap_err();
        assert!(
            format!("{err:#}").contains("more than 65536 records"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);
    }

    #[test]
    fn claim_inputs_at_the_limits_are_accepted() {
        let store = temp_store();
        let max_key = "k".repeat(MAX_INTENT_KEY_CHARS);
        let max_session = "s".repeat(MAX_SESSION_CHARS);

        store.claim(&max_key, &max_session, 1_000, 1_800).unwrap();
        assert_eq!(store.get(&max_key).unwrap().unwrap().session, max_session);
    }

    #[test]
    fn claim_inputs_beyond_the_limits_are_rejected() {
        let store = temp_store();
        let long_key = "k".repeat(MAX_INTENT_KEY_CHARS + 1);
        let long_session = "s".repeat(MAX_SESSION_CHARS + 1);

        let err = store
            .claim(&long_key, "session-a", 1_000, 1_800)
            .unwrap_err();
        assert!(matches!(err, IntentError::Invalid(message) if message.contains("at most")));

        let err = store
            .claim("repo:a.rs", &long_session, 1_000, 1_800)
            .unwrap_err();
        assert!(matches!(err, IntentError::Invalid(message) if message.contains("at most")));
    }

    #[test]
    fn empty_inputs_and_a_non_future_expiry_are_rejected() {
        let store = temp_store();
        assert!(matches!(
            store.claim("", "session-a", 1_000, 1_800).unwrap_err(),
            IntentError::Invalid(message) if message.contains("key must not be empty")
        ));
        assert!(matches!(
            store.claim("repo:a.rs", "", 1_000, 1_800).unwrap_err(),
            IntentError::Invalid(message) if message.contains("session must not be empty")
        ));
        for expires_at in [1_000, 999] {
            let err = store
                .claim("repo:a.rs", "session-a", 1_000, expires_at)
                .unwrap_err();
            assert!(
                matches!(err, IntentError::Invalid(message) if message.contains("must be after"))
            );
        }
        assert!(store.get("repo:a.rs").unwrap().is_none());
    }

    #[test]
    fn corrupt_intents_file_reports_the_cause_chain_without_panicking() {
        let store = temp_store();
        let original = b"{ this is not json".to_vec();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.get("repo:a.rs").unwrap_err();
        // `Display` itself must carry the anyhow context chain, so a plain
        // `to_string()` is enough; callers do not need `{:#}`.
        let message = err.to_string();
        assert!(
            message.contains("contains invalid JSON"),
            "unexpected error: {message}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn unsupported_format_version_is_rejected_without_rewriting() {
        let store = temp_store();
        let original = br#"{"format_version":99,"intents":[]}"#.to_vec();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.get("repo:a.rs").unwrap_err();
        assert!(
            format!("{err:#}").contains("unsupported intents file format version 99"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn duplicate_stored_keys_are_rejected_without_rewriting() {
        let store = temp_store();
        let original = br#"{"format_version":1,"intents":[
            {"key":"repo:a.rs","session":"session-a","created_at":1,"expires_at":1800},
            {"key":"repo:a.rs","session":"session-b","created_at":2,"expires_at":1800}
        ]}"#
        .to_vec();
        fs::write(store.state_file_path(), &original).unwrap();

        // Use the write path to prove a rejected read never clobbers the file.
        let err = store
            .claim("repo:new.rs", "session-c", 1_000, 2_000)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate intent key"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn expiry_boundary_is_exclusive_for_list_and_claim() {
        let store = temp_store();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();

        // At exactly `expires_at` the record is no longer visible...
        assert!(store.list(1_800).unwrap().is_empty());
        // ...and a different session may take it at that instant.
        store.claim("repo:a.rs", "session-b", 1_800, 2_800).unwrap();
        assert_eq!(
            store.get("repo:a.rs").unwrap().unwrap().session,
            "session-b"
        );
    }

    #[test]
    fn a_second_store_handle_sees_claims_from_the_first() {
        let store = temp_store();
        let peer = IntentStore::open(Some(store.dir_path())).unwrap();
        store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();

        let got = peer.get("repo:a.rs").unwrap().unwrap();
        assert_eq!(got.session, "session-a");
    }

    const RACING_SESSIONS: usize = 8;

    #[test]
    fn concurrent_claims_of_one_key_have_exactly_one_winner() {
        use std::sync::{Arc, Barrier};

        let store = temp_store();
        let shared = Arc::new(store.store.clone());
        let barrier = Arc::new(Barrier::new(RACING_SESSIONS));
        let handles: Vec<_> = (0..RACING_SESSIONS)
            .map(|index| {
                let store = Arc::clone(&shared);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let session = format!("session-{index}");
                    store
                        .claim("repo:a.rs", &session, 1_000, 1_800)
                        .map(|_| session)
                })
            })
            .collect();

        let winners: Vec<String> = handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "expected exactly one winner, got {winners:?}"
        );

        let raw = fs::read_to_string(store.state_file_path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["intents"].as_array().unwrap().len(), 1);
        assert_eq!(store.get("repo:a.rs").unwrap().unwrap().session, winners[0]);
    }

    /// A repository plus one linked worktree, both containing `src/auth.rs`.
    struct GitRepo {
        root: tempfile::TempDir,
        main: PathBuf,
        linked: PathBuf,
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git must be installed to run the file-intent key tests");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_repo_with_worktree() -> GitRepo {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        fs::create_dir(&main).unwrap();
        run_git(&main, &["init", "--quiet"]);
        fs::create_dir_all(main.join("src")).unwrap();
        fs::write(main.join("src/auth.rs"), "fn auth() {}\n").unwrap();
        run_git(&main, &["add", "."]);
        // Identity and signing flags are passed explicitly so a developer's
        // global git config cannot make the fixture commit fail.
        run_git(
            &main,
            &[
                "-c",
                "user.email=portzilla@example.com",
                "-c",
                "user.name=Portzilla Test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        );
        let linked = root.path().join("linked");
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked-worktree",
                linked.to_str().unwrap(),
            ],
        );
        GitRepo { root, main, linked }
    }

    #[test]
    fn same_relative_file_in_two_worktrees_shares_a_key() {
        let repo = git_repo_with_worktree();
        let main_key = key_for_file(&repo.main, Path::new("src/auth.rs"));
        let linked_key = key_for_file(&repo.linked, Path::new("src/auth.rs"));
        assert_eq!(main_key, linked_key);

        // Repository identity is the common dir, not the worktree root: the key
        // must be rooted at the main checkout's `.git`, which is exactly what a
        // swap from `--git-common-dir` to `--show-toplevel` would break.
        let common_dir = fs::canonicalize(repo.main.join(".git")).unwrap();
        assert!(
            main_key.starts_with(&format!("{}:", common_dir.display())),
            "key {main_key} should be rooted at the common dir {}",
            common_dir.display()
        );
        assert!(
            main_key.ends_with(&format!(":{}", Path::new("src/auth.rs").display())),
            "unexpected key {main_key}"
        );
    }

    #[test]
    fn outside_git_falls_back_to_an_absolute_path_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let expected = fs::canonicalize(dir.path()).unwrap().join("notes.md");

        let relative = key_for_file(dir.path(), Path::new("notes.md"));
        let absolute = key_for_file(dir.path(), &dir.path().join("notes.md"));

        assert_eq!(relative, expected.to_string_lossy());
        assert_eq!(absolute, expected.to_string_lossy());
    }

    #[test]
    fn file_outside_the_repository_falls_back_to_its_absolute_path() {
        let repo = git_repo_with_worktree();
        let outside = repo.root.path().join("notes.md");
        let key = key_for_file(&repo.main, &outside);
        // The fallback is the deep-canonicalized path, not the caller's spelling.
        let expected = fs::canonicalize(repo.root.path()).unwrap().join("notes.md");
        assert_eq!(key, expected.to_string_lossy());
    }

    #[test]
    fn relative_paths_are_normalized_against_the_given_cwd() {
        let repo = git_repo_with_worktree();
        let from_root = key_for_file(&repo.main, Path::new("src/auth.rs"));
        let from_subdir = key_for_file(&repo.main.join("src"), Path::new("./auth.rs"));
        assert_eq!(from_root, from_subdir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_does_not_change_the_key() {
        let repo = git_repo_with_worktree();
        std::os::unix::fs::symlink(".", repo.main.join("alias")).unwrap();

        let direct = key_for_file(&repo.main, Path::new("src/auth.rs"));
        let through_alias = key_for_file(&repo.main, Path::new("alias/src/auth.rs"));

        assert_eq!(through_alias, direct);
    }

    #[test]
    fn not_yet_created_file_agrees_across_relative_and_absolute_spellings() {
        let repo = git_repo_with_worktree();
        let canonical_main = fs::canonicalize(&repo.main).unwrap();

        let relative = key_for_file(&repo.main, Path::new("src/brand_new.rs"));
        let absolute = key_for_file(&repo.main, &canonical_main.join("src/brand_new.rs"));
        // The raw spelling may hold symlinked roots (macOS `/var`), which is
        // precisely the case that used to fall back to a second key.
        let unresolved = key_for_file(&repo.main, &repo.main.join("src/brand_new.rs"));

        assert_eq!(relative, absolute);
        assert_eq!(relative, unresolved);
        assert!(
            relative.ends_with(&format!(":{}", Path::new("src/brand_new.rs").display())),
            "unexpected key {relative}"
        );
    }
}
