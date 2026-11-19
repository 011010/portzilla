//! Locked persistence for environment slots: ports, cache directories, the
//! env file, and the database a worktree's slot owns.
//!
//! # Key model
//!
//! A slot is keyed by **(worktree, name)**. Two linked worktrees of the same
//! repository can both have a slot called `auth`, because their ports, cache
//! directories, and env files are all different resources. The same name in
//! one worktree is a collision and is refused.
//!
//! `repo_key` (the git common dir) is stored for grouping and display only;
//! it is never part of the key. `worktree` is always canonical, so two
//! spellings of one directory (`/var` versus `/private/var`) cannot create two
//! records for what is really one slot.
//!
//! # Durability
//!
//! Like `leases.json` and `intents.json`, the file is a versioned envelope
//! written through a temp file plus rename under an exclusive `fs4` lock.
//! Slots outlive the process that created them by design: there is no TTL and
//! no daemon, so `slot down` (or `slot prune` after the worktree disappears)
//! is the only thing that removes a record.
//!
//! Nothing here touches the lease store or the filesystem outside the data
//! directory. Creating ports and directories, provisioning databases, and
//! rolling all of that back on failure belong to [`crate::slot_runtime`].

use crate::intent::{canonicalize_or_owned, normalize_path};
use crate::store::Store;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Version of the `slots.json` envelope. Bump it when the stored shape changes
/// incompatibly so an older binary refuses to touch a newer file instead of
/// corrupting it.
const SLOT_FORMAT_VERSION: u32 = 1;

/// Maximum number of slot records. Slots are explicit and long-lived, so this
/// is far beyond any legitimate number of worktrees on one machine while
/// still bounding a listing of a corrupted or hostile file.
const MAX_SLOTS: usize = 4096;

/// The database a slot owns. The names are rendered from the config at `up`
/// time; the deprovision argv is rendered too, so `slot down` does not depend
/// on the config file still existing or still saying the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbRuntime {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprovision: Option<Vec<String>>,
}

/// One live environment slot as persisted in `slots.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRecord {
    pub name: String,
    /// Canonical worktree root the slot belongs to.
    pub worktree: PathBuf,
    /// Repository identity (git common dir, or the worktree outside git).
    pub repo_key: String,
    /// Port name -> the port actually allocated (may differ from the config's
    /// preferred base when the base was taken).
    pub ports: BTreeMap<String, u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<DbRuntime>,
    /// Distinct cache directories this slot created, in sorted order.
    #[serde(default)]
    pub cache_dirs: Vec<PathBuf>,
    /// The `.env.portzilla` file written in the worktree.
    pub env_file: PathBuf,
    pub created_at: u64,
    /// Captured at `up` time so `down` behaves as the recipe that created the
    /// slot intended, even if the config changes or disappears.
    #[serde(default)]
    pub purge_cache_on_down: bool,
    /// The complete environment rendered by `up`. `slot env` and `slot run`
    /// use it directly, so they keep working without reading the config.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredSlots {
    format_version: u32,
    slots: Vec<SlotRecord>,
}

/// Filesystem seam mirroring the lease and intent stores': production writes
/// and renames for real, while a test can inject a rename failure and prove a
/// failed commit leaves the previous `slots.json` bytes untouched.
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

/// Why a slot operation was refused.
///
/// Typed because callers branch on it: the CLI maps [`SlotError::NotFound`] to
/// exit code 2 while everything else is a general error (exit 1).
#[derive(Debug)]
pub enum SlotError {
    /// A slot with this name already exists in this worktree.
    AlreadyExists { name: String, worktree: PathBuf },
    /// No slot with this name exists in this worktree.
    NotFound { name: String, worktree: PathBuf },
    /// The store would exceed [`MAX_SLOTS`].
    Capacity { max: usize },
    /// The underlying locked store failed.
    Store(anyhow::Error),
}

impl fmt::Display for SlotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists { name, worktree } => write!(
                formatter,
                "slot {name} is already up in {}; run `portzilla slot down {name}` first or pass --replace",
                worktree.display()
            ),
            Self::NotFound { name, worktree } => {
                write!(formatter, "slot {name} is not up in {}", worktree.display())
            }
            Self::Capacity { max } => {
                write!(
                    formatter,
                    "slot store already holds the maximum of {max} slots"
                )
            }
            Self::Store(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for SlotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for SlotError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

/// Result of a slot-store operation whose failures callers may match on.
pub type SlotResult<T> = std::result::Result<T, SlotError>;

/// A locked JSON-backed store of environment slots rooted at a data directory.
///
/// Mirrors [`IntentStore`](crate::intent::IntentStore): the only in-memory
/// state is the data directory, every read-modify-write runs under an
/// exclusive `fs4` lock on `slots.json.lock`, and commits replace
/// `slots.json` atomically through a temp file plus rename. Reads take the
/// lock too, so a reader never observes a partially written file.
#[derive(Clone)]
pub struct SlotStore {
    data_dir: PathBuf,
    file_ops: Arc<dyn FileOps>,
}

impl SlotStore {
    /// Opens the slot store, resolving the same data directory as the lease
    /// and intent stores unless `override_dir` is given. The directory is
    /// created owner-only on Unix.
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

    /// Serializes a whole slot operation (`up`, `down`, `prune`) across
    /// processes, not just a single store read-modify-write.
    ///
    /// `slots.json`'s own lock protects the file; this lock protects the
    /// *machine*: two concurrent `up` runs must not both conclude that a
    /// cache directory is free, and `up --replace` must not race another
    /// teardown of the same name. The returned handle releases the lock when
    /// it drops. It is a separate file so the store's short-lived operations
    /// can still take `slots.json.lock` while an operation lock is held.
    pub fn lock_operations(&self) -> Result<fs::File> {
        self.ensure_data_dir()?;
        let lock_path = self.data_dir.join("slots.ops.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file at {}", lock_path.display()))?;
        harden_file_permissions(&lock_path)?;
        fs4::FileExt::lock(&lock_file).context("failed to acquire the slot operation lock")?;
        Ok(lock_file)
    }

    /// Path to the JSON slots file inside the data directory.
    pub fn state_file_path(&self) -> PathBuf {
        self.data_dir.join("slots.json")
    }

    fn lock_file_path(&self) -> PathBuf {
        self.data_dir.join("slots.json.lock")
    }

    /// Persists `record`, refusing a duplicate name in the same worktree.
    ///
    /// The record's worktree is canonicalized here, so callers may pass either
    /// spelling; the returned record is the one that was written.
    pub fn insert(&self, record: SlotRecord) -> SlotResult<SlotRecord> {
        let mut record = record;
        record.worktree = canonicalize_or_owned(&record.worktree);
        record.env_file = normalize_path(&record.env_file);
        record.cache_dirs = record
            .cache_dirs
            .iter()
            .map(|path| normalize_path(path))
            .collect();
        record.cache_dirs.sort();
        record.cache_dirs.dedup();

        let _guard = self.lock_exclusive()?;
        let mut slots = self.read_slots_unlocked()?;
        if slots.iter().any(|stored| {
            stored.name == record.name && same_worktree(&stored.worktree, &record.worktree)
        }) {
            return Err(SlotError::AlreadyExists {
                name: record.name,
                worktree: record.worktree,
            });
        }
        if slots.len() >= MAX_SLOTS {
            return Err(SlotError::Capacity { max: MAX_SLOTS });
        }
        slots.push(record.clone());
        self.write_slots_unlocked(&slots)?;
        Ok(record)
    }

    /// Returns the record for `(worktree, name)`, if any.
    #[allow(clippy::result_large_err)]
    pub fn get(&self, worktree: &Path, name: &str) -> SlotResult<Option<SlotRecord>> {
        let worktree = canonicalize_or_owned(worktree);
        let _guard = self.lock_exclusive()?;
        let slots = self.read_slots_unlocked()?;
        Ok(slots
            .into_iter()
            .find(|stored| stored.name == name && same_worktree(&stored.worktree, &worktree)))
    }

    /// Returns every stored slot, in insertion order. Callers sort for output.
    #[allow(clippy::result_large_err)]
    pub fn list(&self) -> SlotResult<Vec<SlotRecord>> {
        let _guard = self.lock_exclusive()?;
        let slots = self.read_slots_unlocked()?;
        Ok(slots)
    }

    /// Removes and returns the record for `(worktree, name)`.
    ///
    /// Removal is idempotent at the store level (`Ok(None)` when absent); the
    /// CLI decides whether absence is an error. Callers must run deprovision,
    /// release leases, and purge directories *before* removing the record, so
    /// a failure in any of those steps leaves the recipe in place to retry.
    #[allow(clippy::result_large_err)]
    pub fn remove(&self, worktree: &Path, name: &str) -> SlotResult<Option<SlotRecord>> {
        let worktree = canonicalize_or_owned(worktree);
        let _guard = self.lock_exclusive()?;
        let mut slots = self.read_slots_unlocked()?;
        let Some(index) = slots
            .iter()
            .position(|stored| stored.name == name && same_worktree(&stored.worktree, &worktree))
        else {
            return Ok(None);
        };
        let removed = slots.remove(index);
        self.write_slots_unlocked(&slots)?;
        Ok(Some(removed))
    }

    /// Removes every record whose worktree no longer exists, returning them.
    ///
    /// This is the only place that deletes records it did not just act on,
    /// because a vanished worktree cannot be acted on at all. Leases and
    /// database cleanup for the returned records are the caller's job.
    #[allow(clippy::result_large_err)]
    pub fn prune(&self) -> SlotResult<Vec<SlotRecord>> {
        let _guard = self.lock_exclusive()?;
        let mut slots = self.read_slots_unlocked()?;
        let mut removed = Vec::new();
        slots.retain(|slot| {
            if slot.worktree.is_dir() {
                true
            } else {
                removed.push(slot.clone());
                false
            }
        });
        if !removed.is_empty() {
            self.write_slots_unlocked(&slots)?;
        }
        Ok(removed)
    }

    /// Ensures the data directory exists and is owner-only (`0700` on Unix),
    /// matching the lease and intent stores.
    fn ensure_data_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).with_context(|| {
            format!(
                "failed to create data directory at {}",
                self.data_dir.display()
            )
        })?;
        harden_dir_permissions(&self.data_dir)
    }

    /// Acquires the exclusive lock guarding `slots.json`, released when the
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
        fs4::FileExt::lock(&lock_file)
            .context("failed to acquire exclusive lock on the slot store")?;
        Ok(lock_file)
    }

    fn read_slots_unlocked(&self) -> Result<Vec<SlotRecord>> {
        let path = self.state_file_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read slots file at {}", path.display()));
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        let stored: StoredSlots = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "slots file at {} contains invalid JSON and was not modified",
                path.display()
            )
        })?;
        if stored.format_version != SLOT_FORMAT_VERSION {
            bail!(
                "slots.json at {} uses format_version {}, which this build does not \
                 understand; upgrade portzilla. The file was not modified",
                path.display(),
                stored.format_version
            );
        }
        validate_stored_slots(&stored.slots, &path)?;
        Ok(stored.slots)
    }

    fn write_slots_unlocked(&self, slots: &[SlotRecord]) -> Result<()> {
        let path = self.state_file_path();
        let tmp_path = self
            .data_dir
            .join(format!(".slots.json.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&StoredSlots {
            format_version: SLOT_FORMAT_VERSION,
            slots: slots.to_vec(),
        })
        .context("failed to serialize slots to JSON")?;
        self.file_ops
            .write_temp(&tmp_path, &json)
            .with_context(|| {
                format!(
                    "failed to write temporary slots file at {}",
                    tmp_path.display()
                )
            })?;
        // Harden before the rename: rename preserves the mode, so this also
        // leaves the final `slots.json` owner-only (0600).
        harden_file_permissions(&tmp_path)?;
        self.file_ops
            .rename_temp(&tmp_path, &path)
            .with_context(|| {
                format!(
                    "failed to atomically replace slots file at {}",
                    path.display()
                )
            })?;
        Ok(())
    }
}

/// Equality of two worktree paths under the store's canonical-spelling rule.
fn same_worktree(left: &Path, right: &Path) -> bool {
    left == right
}

/// Rejects a persisted file that violates the store's invariants: more records
/// than [`MAX_SLOTS`], or two records for one `(worktree, name)`. The file is
/// left untouched on rejection.
fn validate_stored_slots(slots: &[SlotRecord], path: &Path) -> Result<()> {
    if slots.len() > MAX_SLOTS {
        bail!(
            "slots file at {} contains more than {MAX_SLOTS} records and was not modified",
            path.display()
        );
    }
    let mut keys = HashSet::with_capacity(slots.len());
    for slot in slots {
        let worktree = canonicalize_or_owned(&slot.worktree);
        if !keys.insert((worktree, slot.name.clone())) {
            bail!(
                "slots file at {} contains duplicate slot {:?} for worktree {} and was not modified",
                path.display(),
                slot.name,
                slot.worktree.display()
            );
        }
    }
    Ok(())
}

/// Restricts a directory to owner-only access (`0700`) on Unix. Mirrors the
/// lease and intent stores' helper.
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

    struct TempStore {
        _dir: tempfile::TempDir,
        store: SlotStore,
    }

    impl std::ops::Deref for TempStore {
        type Target = SlotStore;

        fn deref(&self) -> &Self::Target {
            &self.store
        }
    }

    fn temp_store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let store = SlotStore::open(Some(dir.path().to_path_buf())).unwrap();
        TempStore { _dir: dir, store }
    }

    fn record(worktree: &Path, name: &str) -> SlotRecord {
        let worktree = canonicalize_or_owned(worktree);
        let mut ports = BTreeMap::new();
        ports.insert("web".to_string(), 3000);
        ports.insert("api".to_string(), 3100);
        let mut env = BTreeMap::new();
        env.insert("PORTZILLA_SLOT".to_string(), name.to_string());
        SlotRecord {
            name: name.to_string(),
            worktree: worktree.clone(),
            repo_key: "/repo/.git".to_string(),
            ports,
            db: Some(DbRuntime {
                name: format!("app_{name}"),
                schema: Some(format!("slot_{name}")),
                deprovision: Some(vec!["dropdb".to_string(), "--if-exists".to_string()]),
            }),
            cache_dirs: vec![worktree.join(".slot").join(name).join("target")],
            env_file: worktree.join(".env.portzilla"),
            created_at: 1_795_000_000,
            purge_cache_on_down: true,
            env,
        }
    }

    fn worktree() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn up_then_get_round_trips_a_slot() {
        let store = temp_store();
        let tree = worktree();
        let original = record(tree.path(), "auth");

        store.insert(original.clone()).unwrap();
        let got = store
            .get(tree.path(), "auth")
            .unwrap()
            .expect("stored slot");

        assert_eq!(got.name, "auth");
        assert_eq!(got.ports.get("web"), Some(&3000));
        assert_eq!(got.ports.get("api"), Some(&3100));
        assert_eq!(got.db.as_ref().unwrap().name, "app_auth");
        assert_eq!(
            got.db.as_ref().unwrap().schema.as_deref(),
            Some("slot_auth")
        );
        assert_eq!(
            got.db
                .as_ref()
                .unwrap()
                .deprovision
                .as_deref()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            got.cache_dirs,
            vec![canonicalize_or_owned(tree.path()).join(".slot/auth/target")]
        );
        assert_eq!(
            got.worktree,
            canonicalize_or_owned(tree.path()),
            "the stored worktree is canonical"
        );
        assert!(got.env_file.ends_with(".env.portzilla"));
        assert_eq!(got.created_at, 1_795_000_000);
        assert!(got.purge_cache_on_down);
        assert_eq!(
            got.env.get("PORTZILLA_SLOT").map(String::as_str),
            Some("auth")
        );
    }

    #[test]
    fn a_second_slot_with_the_same_name_in_one_worktree_is_refused() {
        let store = temp_store();
        let tree = worktree();
        store.insert(record(tree.path(), "auth")).unwrap();

        let err = store.insert(record(tree.path(), "auth")).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("already up"),
            "unexpected error: {message}"
        );
        assert!(message.contains("--replace"));
        assert!(matches!(err, SlotError::AlreadyExists { .. }));
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn the_same_name_in_two_worktrees_coexists() {
        let store = temp_store();
        let first = worktree();
        let second = worktree();

        store.insert(record(first.path(), "auth")).unwrap();
        store.insert(record(second.path(), "auth")).unwrap();

        let slots = store.list().unwrap();
        assert_eq!(slots.len(), 2);
        assert!(
            store
                .get(first.path(), "auth")
                .unwrap()
                .is_some_and(|slot| slot.worktree == canonicalize_or_owned(first.path()))
        );
        assert!(
            store
                .get(second.path(), "auth")
                .unwrap()
                .is_some_and(|slot| slot.worktree == canonicalize_or_owned(second.path()))
        );
        assert!(store.get(first.path(), "other").unwrap().is_none());
    }

    #[test]
    fn an_unknown_format_version_is_rejected_without_rewriting() {
        let store = temp_store();
        let original = br#"{"format_version":99,"slots":[]}"#.to_vec();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.list().unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("format_version 99"),
            "unexpected error: {message}"
        );
        assert!(message.contains("was not modified"));
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn prune_removes_slots_whose_worktree_is_gone() {
        let store = temp_store();
        let live = worktree();
        let gone = worktree();
        let gone_path = canonicalize_or_owned(gone.path());
        store.insert(record(live.path(), "live")).unwrap();
        store.insert(record(gone.path(), "gone")).unwrap();
        drop(gone);

        let removed = store.prune().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "gone");
        assert_eq!(removed[0].worktree, gone_path);

        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "live");
    }

    #[test]
    fn remove_returns_the_record_and_is_absent_a_second_time() {
        let store = temp_store();
        let tree = worktree();
        store.insert(record(tree.path(), "auth")).unwrap();

        let removed = store.remove(tree.path(), "auth").unwrap().unwrap();
        assert_eq!(removed.name, "auth");
        assert!(store.remove(tree.path(), "auth").unwrap().is_none());
        assert!(store.get(tree.path(), "auth").unwrap().is_none());
    }

    #[test]
    fn slots_file_uses_the_versioned_envelope() {
        let store = temp_store();
        let tree = worktree();
        store.insert(record(tree.path(), "auth")).unwrap();

        let path = store.state_file_path();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "slots.json");
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["format_version"].as_u64(), Some(1));
        assert_eq!(value["slots"].as_array().unwrap().len(), 1);
        assert_eq!(value["slots"][0]["name"], "auth");
        assert_eq!(value["slots"][0]["ports"]["web"], 3000);
        assert_eq!(value["slots"][0]["db"]["name"], "app_auth");
    }

    #[test]
    fn a_corrupt_file_reports_the_cause_and_is_not_rewritten() {
        let store = temp_store();
        let original = b"{ this is not json".to_vec();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.list().unwrap_err();
        assert!(
            format!("{err:#}").contains("contains invalid JSON"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }

    #[test]
    fn duplicate_stored_keys_are_rejected_without_rewriting() {
        let store = temp_store();
        let tree = worktree();
        let worktree = canonicalize_or_owned(tree.path());
        let duplicate = serde_json::json!({
            "format_version": 1,
            "slots": [
                {"name":"auth","worktree":worktree,"repo_key":"r","ports":{},"cache_dirs":[],"env_file":worktree.join(".env.portzilla"),"created_at":1},
                {"name":"auth","worktree":worktree,"repo_key":"r","ports":{},"cache_dirs":[],"env_file":worktree.join(".env.portzilla"),"created_at":2}
            ]
        });
        let original = serde_json::to_vec(&duplicate).unwrap();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.list().unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate slot"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
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
        let store = SlotStore::open(Some(dir.path().to_path_buf())).unwrap();
        let tree = worktree();
        store.insert(record(tree.path(), "auth")).unwrap();
        let before = fs::read(store.state_file_path()).unwrap();

        let failing = SlotStore {
            data_dir: dir.path().to_path_buf(),
            file_ops: Arc::new(RenameFailFileOps),
        };
        let err = failing.insert(record(tree.path(), "other")).unwrap_err();
        assert!(format!("{err:#}").contains("injected rename failure"));
        assert_eq!(fs::read(store.state_file_path()).unwrap(), before);
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_store() {
        let store = temp_store();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_file_above_the_record_cap_reads_as_an_error() {
        let store = temp_store();
        let slots: Vec<serde_json::Value> = (0..=MAX_SLOTS)
            .map(|index| {
                serde_json::json!({
                    "name": format!("slot-{index}"),
                    "worktree": format!("/worktrees/{index}"),
                    "repo_key": "/repo/.git",
                    "ports": {},
                    "cache_dirs": [],
                    "env_file": format!("/worktrees/{index}/.env.portzilla"),
                    "created_at": 1
                })
            })
            .collect();
        let original = serde_json::to_vec(&serde_json::json!({
            "format_version": 1,
            "slots": slots,
        }))
        .unwrap();
        fs::write(store.state_file_path(), &original).unwrap();

        let err = store.list().unwrap_err();
        assert!(
            format!("{err:#}").contains("more than 4096 records"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read(store.state_file_path()).unwrap(), original);
    }
}
