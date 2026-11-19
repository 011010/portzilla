//! The side effects of an environment slot: claim ports, create directories,
//! render the environment, write the env file, and undo all of it on failure.
//!
//! The store ([`crate::slot`]) is only the recipe; this module is where the
//! recipe touches the machine. The rules it enforces:
//!
//! - Ports are allocated through [`Store::claim_durable`], so the existing
//!   allocator decides conflicts and the reservation is visible to `claim`,
//!   `run`, `prune`, `who`, and the kill guard.
//! - A cache directory is created only through [`create_dir_tracked`], which
//!   records the directories that did not exist before. Rollback removes only
//!   those; a directory the user already had is never deleted.
//! - Every cache directory must resolve inside the worktree. That is what
//!   makes `down`'s purge safe: a config cannot point it at `$HOME` or `/`.
//! - Failure rolls back ports, directories, the env file, and (when
//!   provisioning was attempted) the database. Nothing is persisted until
//!   every step has succeeded.
//! - `down` is retryable: a failed deprovision leaves the record, the ports,
//!   and the directories in place. `--force` is the only way to discard them.

use crate::audit::{AuditActor, AuditSource};
use crate::intent::{canonicalize_or_owned, repository_identity};
use crate::lease::{PidChecker, current_unix_timestamp};
use crate::slot::{DbRuntime, SlotError, SlotRecord, SlotStore};
use crate::slot_config::{DbConfig, SlotConfig, cache_entries, distinct_cache_dirs};
use crate::store::{Store, TaggedRelease};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The generated environment file in the worktree. The name is fixed by the
/// design so shell snippets and direnv users can rely on it.
pub const ENV_FILE_NAME: &str = ".env.portzilla";

/// A resolved slot context: the worktree, its repository identity, and the
/// validated config. Resolution is read-only; nothing is created before this
/// succeeds.
#[derive(Debug, Clone)]
pub struct SlotContext {
    pub worktree: PathBuf,
    pub repo_key: String,
    pub config: SlotConfig,
}

impl SlotContext {
    /// Resolves the worktree from `cwd` (git toplevel, or `cwd` outside git)
    /// and loads the config from `--config` or `<worktree>/portzilla.toml`.
    pub fn resolve(cwd: &Path, explicit_config: Option<&Path>) -> Result<Self> {
        let (worktree, common_dir) = repository_identity(cwd);
        let config = SlotConfig::load(&worktree, explicit_config)?;
        Ok(Self {
            repo_key: common_dir.to_string_lossy().into_owned(),
            worktree,
            config,
        })
    }
}

/// The stores and liveness checker a slot operation needs. Grouping them keeps
/// the public functions to a readable argument list and lets unit tests inject
/// a store whose OS probe never binds a real socket.
pub struct SlotRuntime<'a> {
    pub leases: &'a Store,
    pub slots: &'a SlotStore,
    pub checker: &'a dyn PidChecker,
}

/// Result of `slot up`.
#[derive(Debug)]
pub struct UpOutcome {
    pub record: SlotRecord,
    pub replaced: bool,
    pub warnings: Vec<String>,
}

/// Result of `slot down`.
#[derive(Debug)]
pub struct DownOutcome {
    pub record: SlotRecord,
    pub released_ports: Vec<u16>,
    pub purged_dirs: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub forced: bool,
}

/// Result of `slot prune`.
#[derive(Debug)]
pub struct PruneOutcome {
    pub removed: Vec<SlotRecord>,
    pub warnings: Vec<String>,
}

impl<'a> SlotRuntime<'a> {
    pub fn new(leases: &'a Store, slots: &'a SlotStore, checker: &'a dyn PidChecker) -> Self {
        Self {
            leases,
            slots,
            checker,
        }
    }

    /// Brings `name` up in `context`'s worktree.
    ///
    /// See the module docs for the rollback contract. `replace` runs a full
    /// `down` first; if that fails, nothing new is created.
    pub fn up(&self, context: &SlotContext, name: &str, replace: bool) -> Result<UpOutcome> {
        crate::slot_config::validate_slot_name(name)?;
        // One slot operation at a time per data directory: the file lock in
        // the store only covers single writes, while `up` makes decisions
        // (is this directory free? is this slot already up?) across many.
        let _operations = self.slots.lock_operations()?;

        let mut warnings = Vec::new();
        let mut replaced = false;
        if let Some(existing) = self.slots.get(&context.worktree, name)? {
            if !replace {
                return Err(SlotError::AlreadyExists {
                    name: name.to_string(),
                    worktree: canonicalize_or_owned(&existing.worktree),
                }
                .into());
            }
            // Already under the operation lock: call the body directly, or
            // the recursive lock acquisition would deadlock.
            self.down_locked(&context.worktree, name, false)
                .with_context(|| format!("failed to tear down the existing slot {name}"))?;
            replaced = true;
        }

        let cache = cache_entries(&context.config, &context.worktree, name);
        for (key, path) in &cache {
            ensure_inside_worktree(&context.worktree, path)
                .with_context(|| format!("[slot.cache] {key}"))?;
        }
        let cache_dirs = distinct_cache_dirs(&cache);
        self.refuse_cache_collisions(name, &context.worktree, &cache_dirs)?;

        let env_file = context.worktree.join(ENV_FILE_NAME);
        for other in self.slots.list()? {
            if other.name != name
                && same_worktree(&other.worktree, &context.worktree)
                && other.env_file == env_file
            {
                warnings.push(format!(
                    "slot {} in the same worktree writes the same {}; the last `slot up` wins",
                    other.name, ENV_FILE_NAME
                ));
            }
        }

        let mut transaction = UpTransaction {
            runtime: self,
            claimed: Vec::new(),
            created_dirs: Vec::new(),
            env_file,
            previous_env: None,
            env_written: false,
            env: BTreeMap::new(),
            deprovision: None,
            provision_attempted: false,
        };
        match self.up_inner(context, name, &cache, &cache_dirs, &mut transaction) {
            Ok(record) => {
                transaction.disarm();
                Ok(UpOutcome {
                    record,
                    replaced,
                    warnings,
                })
            }
            Err(error) => Err(transaction.rollback(error)),
        }
    }

    fn up_inner(
        &self,
        context: &SlotContext,
        name: &str,
        cache: &[(String, PathBuf)],
        cache_dirs: &[PathBuf],
        transaction: &mut UpTransaction<'_, '_>,
    ) -> Result<SlotRecord> {
        let mut ports = BTreeMap::new();
        for (port_name, base) in &context.config.ports {
            let tag = lease_tag(name, port_name);
            let outcome = self.leases.claim_durable(
                *base,
                tag.clone(),
                Some(name.to_string()),
                self.checker,
            )?;
            let port = outcome.lease.port;
            transaction.claimed.push((port, tag));
            ports.insert(port_name.clone(), port);
        }

        for dir in cache_dirs {
            create_dir_tracked(dir, &mut transaction.created_dirs)?;
        }

        let db = build_db_runtime(&context.config, name, &context.worktree);
        let env = render_environment(
            &context.worktree,
            name,
            &ports,
            db.as_ref(),
            &context.config,
            cache,
        )?;
        transaction.env = env.clone();
        transaction.deprovision = db.as_ref().and_then(|db| db.deprovision.clone());

        // Provision after the environment is complete so the command sees the
        // same PORTZILLA_* values the slot exports, including PORTZILLA_DB and
        // PORTZILLA_DB_SCHEMA. From here on, rollback also runs deprovision:
        // a half-created database is exactly the kind of partial state the
        // rollback contract exists to remove.
        if let Some(db) = &db
            && let Some(config_db) = &context.config.db
            && !config_db.provision.is_empty()
        {
            let argv = render_db_argv(
                &config_db.provision,
                name,
                &db.name,
                db.schema.as_deref(),
                &context.worktree,
            );
            transaction.provision_attempted = true;
            run_argv(&argv, &env, "provision")
                .with_context(|| format!("failed to provision the database for slot {name}"))?;
        }

        let previous = fs::read(&transaction.env_file).ok();
        transaction.previous_env = previous;
        write_env_file(&transaction.env_file, &env)?;
        transaction.env_written = true;

        let record = SlotRecord {
            name: name.to_string(),
            worktree: canonicalize_or_owned(&context.worktree),
            repo_key: context.repo_key.clone(),
            ports,
            db,
            cache_dirs: cache_dirs.to_vec(),
            env_file: transaction.env_file.clone(),
            created_at: current_unix_timestamp(),
            purge_cache_on_down: context.config.purge_cache_on_down,
            env,
        };
        Ok(self.slots.insert(record)?)
    }

    /// Tears `name` down: deprovision, release the slot's leases, purge the
    /// recorded directories, remove the env file, then remove the record.
    ///
    /// Deprovision runs before anything destructive, so its failure leaves
    /// the record, ports, directories, and env file untouched. A release
    /// failure (an I/O error in the lease store, not a tag mismatch) also
    /// aborts with the record in place, though ports released before it stay
    /// released; deprovision must be idempotent because a retry runs it again.
    /// A lease whose tag no longer matches is never released: another owner
    /// may have taken that port after the slot lost it.
    pub fn down(&self, worktree: &Path, name: &str, force: bool) -> Result<DownOutcome> {
        let _operations = self.slots.lock_operations()?;
        self.down_locked(worktree, name, force)
    }

    fn down_locked(&self, worktree: &Path, name: &str, force: bool) -> Result<DownOutcome> {
        let worktree = canonicalize_or_owned(worktree);
        let record = self
            .slots
            .get(&worktree, name)?
            .ok_or_else(|| SlotError::NotFound {
                name: name.to_string(),
                worktree: worktree.clone(),
            })?;
        let mut warnings = Vec::new();

        if let Some(db) = &record.db
            && let Some(deprovision) = &db.deprovision
            && let Err(error) = run_argv(deprovision, &record.env, "deprovision")
        {
            if !force {
                return Err(error.context(format!(
                    "slot {name} was left in place; fix the deprovision command and \
                     retry, or pass --force"
                )));
            }
            warnings.push(format!("deprovision failed (forced): {error:#}"));
        }

        let mut released_ports = Vec::new();
        for (port_name, port) in &record.ports {
            let tag = lease_tag(&record.name, port_name);
            match self
                .leases
                .release_if_tagged(*port, &tag, slot_actor(&record.name), self.checker)
            {
                Ok(TaggedRelease::Released(_)) => released_ports.push(*port),
                Ok(TaggedRelease::NotOurs(lease)) => warnings.push(format!(
                    "port {port} is leased by {:?}, not by slot {}; leaving it alone",
                    lease.tag, record.name
                )),
                Ok(TaggedRelease::Absent) => {}
                Err(error) if force => {
                    warnings.push(format!("failed to release port {port} (forced): {error:#}"));
                }
                Err(error) => {
                    return Err(error.context(format!(
                        "failed to release port {port}; slot {name} was left in place. \
                         Retry, or pass --force"
                    )));
                }
            }
        }

        let purged_dirs = if record.purge_cache_on_down {
            purge_cache_dirs(&record.worktree, &record.cache_dirs, &mut warnings)
        } else {
            Vec::new()
        };

        remove_env_file(&record, &mut warnings);

        self.slots.remove(&record.worktree, &record.name)?;
        Ok(DownOutcome {
            record,
            released_ports,
            purged_dirs,
            warnings,
            forced: force,
        })
    }

    /// Removes slots whose worktree no longer exists, releasing their leases
    /// and attempting deprovision. Directory purging is skipped: the
    /// directories went away with the worktree.
    pub fn prune(&self) -> Result<PruneOutcome> {
        let _operations = self.slots.lock_operations()?;
        let mut warnings = Vec::new();
        let pruned = self.slots.prune()?;
        for record in &pruned {
            if let Some(db) = &record.db
                && let Some(deprovision) = &db.deprovision
                && let Err(error) = run_argv(deprovision, &record.env, "deprovision")
            {
                warnings.push(format!(
                    "deprovision failed for pruned slot {}: {error:#}",
                    record.name
                ));
            }
            for (port_name, port) in &record.ports {
                let tag = lease_tag(&record.name, port_name);
                match self.leases.release_if_tagged(
                    *port,
                    &tag,
                    slot_actor(&record.name),
                    self.checker,
                ) {
                    Ok(TaggedRelease::Released(_) | TaggedRelease::Absent) => {}
                    Ok(TaggedRelease::NotOurs(lease)) => warnings.push(format!(
                        "port {port} is leased by {:?}, not by pruned slot {}; leaving it alone",
                        lease.tag, record.name
                    )),
                    Err(error) => warnings.push(format!(
                        "failed to release port {port} of pruned slot {}: {error:#}; \
                         the record is gone, so release it manually",
                        record.name
                    )),
                }
            }
        }
        Ok(PruneOutcome {
            removed: pruned,
            warnings,
        })
    }

    /// Finds the slot `name` in the worktree containing `cwd`.
    pub fn find(&self, cwd: &Path, name: &str) -> Result<SlotRecord, SlotError> {
        let (worktree, _) = repository_identity(cwd);
        self.slots.get(&worktree, name)?.ok_or(SlotError::NotFound {
            name: name.to_string(),
            worktree,
        })
    }

    /// Refuses a cache directory that another slot has already recorded (or
    /// whose ancestor/descendant it recorded): purging one slot must never
    /// delete another slot's files.
    fn refuse_cache_collisions(
        &self,
        name: &str,
        worktree: &Path,
        cache_dirs: &[PathBuf],
    ) -> Result<()> {
        for other in self.slots.list()? {
            if other.name == name && same_worktree(&other.worktree, worktree) {
                continue;
            }
            for ours in cache_dirs {
                if let Some(theirs) = other
                    .cache_dirs
                    .iter()
                    .find(|theirs| paths_overlap(ours, theirs))
                {
                    bail!(
                        "cache directory {} overlaps {} recorded by slot {} in {}; \
                         give each slot its own directory",
                        ours.display(),
                        theirs.display(),
                        other.name,
                        other.worktree.display()
                    );
                }
            }
        }
        Ok(())
    }
}

/// The lease tag that identifies one port of one slot. Stable across
/// re-allocation: the tag names the port *name*, not the number, so teardown
/// can find the lease after the allocator moved it.
pub fn lease_tag(slot: &str, port_name: &str) -> String {
    format!("slot:{slot}:{port_name}")
}

/// Renders the complete slot environment.
///
/// Precedence, lowest to highest:
/// 1. computed values: `PORTZILLA_SLOT`, `PORTZILLA_WORKTREE`,
///    `PORTZILLA_PORT[_<NAME>]`, `PORTZILLA_DB`, `PORTZILLA_DB_SCHEMA`.
///    `PORTZILLA_PORT` is the first port in name order (alphabetical, from
///    the same `BTreeMap` used to render every file), a convenience for the
///    common single-port slot.
/// 2. `[slot.env]`
/// 3. `[slot.cache]` (the resolved absolute path wins on a key conflict)
pub fn render_environment(
    worktree: &Path,
    name: &str,
    ports: &BTreeMap<String, u16>,
    db: Option<&DbRuntime>,
    config: &SlotConfig,
    cache: &[(String, PathBuf)],
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    env.insert("PORTZILLA_SLOT".to_string(), name.to_string());
    env.insert(
        "PORTZILLA_WORKTREE".to_string(),
        worktree.to_string_lossy().into_owned(),
    );
    for (port_name, port) in ports {
        env.insert(
            format!("PORTZILLA_PORT_{}", port_env_suffix(port_name)),
            port.to_string(),
        );
    }
    if let Some((_, first)) = ports.iter().next() {
        env.insert("PORTZILLA_PORT".to_string(), first.to_string());
    }
    if let Some(db) = db {
        env.insert("PORTZILLA_DB".to_string(), db.name.clone());
        if let Some(schema) = &db.schema {
            env.insert("PORTZILLA_DB_SCHEMA".to_string(), schema.clone());
        }
    }
    for (key, value) in &config.env {
        env.insert(key.clone(), value.clone());
    }
    for (key, path) in cache {
        env.insert(key.clone(), path.to_string_lossy().into_owned());
    }
    for (key, value) in &env {
        if value.chars().any(char::is_control) {
            bail!(
                "environment value for {key} contains a control character and cannot be written \
                 to {ENV_FILE_NAME}"
            );
        }
    }
    Ok(env)
}

/// One `KEY=value` line per variable, sorted, ready for `eval` or direnv.
/// Values are double-quoted only when they contain anything a shell would
/// interpret; the quotes escape `\`, `"`, `$`, and backticks.
pub fn render_env_file(env: &BTreeMap<String, String>) -> String {
    let mut output = String::new();
    for (key, value) in env {
        output.push_str(key);
        output.push('=');
        output.push_str(&shell_value(value));
        output.push('\n');
    }
    output
}

fn shell_value(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-./:@%+,=".contains(character));
    if safe {
        return value.to_string();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn port_env_suffix(port_name: &str) -> String {
    port_name
        .chars()
        .map(|character| {
            if character == '-' {
                '_'
            } else {
                character.to_ascii_uppercase()
            }
        })
        .collect()
}

fn same_worktree(left: &Path, right: &Path) -> bool {
    canonicalize_or_owned(left) == canonicalize_or_owned(right)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// A cache directory must live inside the worktree. This is what makes
/// `purge_cache_on_down` safe; a config cannot aim teardown at `/` or `$HOME`.
fn ensure_inside_worktree(worktree: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(worktree) || path == worktree {
        bail!(
            "cache path {} is outside the worktree {}; cache directories must live inside it",
            path.display(),
            worktree.display()
        );
    }
    Ok(())
}

/// Creates `path` and records every directory this call actually created,
/// parents first. Rollback reverses the list and removes deepest first.
fn create_dir_tracked(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            break;
        }
        missing.push(candidate.to_path_buf());
        current = candidate.parent();
    }
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create cache directory {}", path.display()))?;
    created.extend(missing.into_iter().rev());
    Ok(())
}

fn write_env_file(path: &Path, env: &BTreeMap<String, String>) -> Result<()> {
    let contents = render_env_file(env);
    let tmp = path.with_file_name(format!("{ENV_FILE_NAME}.tmp.{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions on {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Removes the env file only when it still contains this slot's rendering.
/// Two slots may share one worktree and therefore one file; `down` must not
/// remove a file the other slot wrote.
fn remove_env_file(record: &SlotRecord, warnings: &mut Vec<String>) {
    match fs::read_to_string(&record.env_file) {
        Ok(contents) if contents == render_env_file(&record.env) => {
            if let Err(error) = fs::remove_file(&record.env_file) {
                warnings.push(format!(
                    "failed to remove {}: {error}",
                    record.env_file.display()
                ));
            }
        }
        Ok(_) => warnings.push(format!(
            "left {} in place: it no longer matches this slot",
            record.env_file.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!(
            "failed to read {}: {error}",
            record.env_file.display()
        )),
    }
}

/// Deletes the recorded cache directories, deepest first, only when they are
/// inside the worktree. Missing directories are fine (a previous partial
/// teardown or a manual `rm -rf`).
fn purge_cache_dirs(worktree: &Path, dirs: &[PathBuf], warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut sorted: Vec<&PathBuf> = dirs.iter().collect();
    sorted.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    let mut purged = Vec::new();
    for path in sorted {
        if !path.starts_with(worktree) || path == worktree {
            warnings.push(format!(
                "refusing to purge {}: it is outside the worktree",
                path.display()
            ));
            continue;
        }
        match fs::remove_dir_all(path) {
            Ok(()) => purged.push(path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warnings.push(format!("failed to purge {}: {error}", path.display())),
        }
    }
    purged
}

fn slot_actor(name: &str) -> AuditActor {
    AuditActor::new(AuditSource::Cli, None, Some(name.to_string()))
}

/// Runs one config command with the slot environment. argv only: no shell is
/// implied. stderr is included verbatim in the error so a failing `createdb`
/// says why.
fn run_argv(argv: &[String], env: &BTreeMap<String, String>, action: &str) -> Result<()> {
    let Some((program, arguments)) = argv.split_first() else {
        bail!("{action} command is empty");
    };
    let output = Command::new(program)
        .args(arguments)
        .envs(env)
        .output()
        .with_context(|| format!("failed to start {action} command {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{action} command `{}` failed with {}: {}",
            argv.join(" "),
            output.status,
            stderr.trim_end()
        );
    }
    Ok(())
}

/// Everything `up` has done so far, in the order it must be undone.
struct UpTransaction<'r, 'a> {
    runtime: &'r SlotRuntime<'a>,
    claimed: Vec<(u16, String)>,
    created_dirs: Vec<PathBuf>,
    env_file: PathBuf,
    previous_env: Option<Vec<u8>>,
    env_written: bool,
    env: BTreeMap<String, String>,
    deprovision: Option<Vec<String>>,
    provision_attempted: bool,
}

impl UpTransaction<'_, '_> {
    fn disarm(&mut self) {
        self.claimed.clear();
        self.created_dirs.clear();
        self.env_written = false;
        self.deprovision = None;
        self.provision_attempted = false;
    }

    /// Undoes the transaction in reverse order. Cleanup failures are attached
    /// to the original error rather than replacing it: the cause is what the
    /// user needs first, and the caveats still need to be visible.
    fn rollback(mut self, cause: anyhow::Error) -> anyhow::Error {
        let mut problems = Vec::new();

        if self.provision_attempted
            && let Some(deprovision) = &self.deprovision
            && !deprovision.is_empty()
            && let Err(error) = run_argv(deprovision, &self.env, "deprovision")
        {
            problems.push(format!("deprovision failed: {error:#}"));
        }

        for (port, tag) in self.claimed.drain(..).rev() {
            match self.runtime.leases.release_if_tagged(
                port,
                &tag,
                slot_actor("rollback"),
                self.runtime.checker,
            ) {
                Ok(TaggedRelease::Released(_) | TaggedRelease::Absent) => {}
                Ok(TaggedRelease::NotOurs(lease)) => problems.push(format!(
                    "port {port} is leased by {:?}; leaving it alone",
                    lease.tag
                )),
                Err(error) => problems.push(format!("failed to release port {port}: {error:#}")),
            }
        }

        for dir in self.created_dirs.drain(..).rev() {
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    problems.push(format!("failed to remove {}: {error}", dir.display()));
                }
            }
        }

        if self.env_written {
            let restored = match &self.previous_env {
                Some(bytes) => fs::write(&self.env_file, bytes).map_err(|error| {
                    format!("failed to restore {}: {error}", self.env_file.display())
                }),
                None => match fs::remove_file(&self.env_file) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(format!(
                        "failed to remove {}: {error}",
                        self.env_file.display()
                    )),
                },
            };
            if let Err(problem) = restored {
                problems.push(problem);
            }
        }

        if problems.is_empty() {
            cause
        } else {
            cause.context(format!("rollback also failed: {}", problems.join("; ")))
        }
    }
}

/// Renders the database identity and deprovision argv from the config.
///
/// Kept here rather than in the store: the store records what `up` decided,
/// while this is the config-to-runtime translation. The deprovision argv is
/// rendered now, with the worktree, so `slot down` can run it after the config
/// has changed or disappeared.
pub fn build_db_runtime(config: &SlotConfig, name: &str, worktree: &Path) -> Option<DbRuntime> {
    let db: &DbConfig = config.db.as_ref()?;
    let rendered_name = config.db_name(name)?;
    let schema = config.db_schema(name);
    let deprovision = db
        .deprovision
        .as_ref()
        .filter(|argv| !argv.is_empty())
        .map(|argv| render_db_argv(argv, name, &rendered_name, schema.as_deref(), worktree));
    Some(DbRuntime {
        name: rendered_name,
        schema,
        deprovision,
    })
}

/// Substitutes `{slot}`, `{db}`, `{schema}`, and `{worktree}` in every argv
/// element. Used for provision (at `up`) and for deprovision, whose rendered
/// form is persisted in the record.
fn render_db_argv(
    argv: &[String],
    name: &str,
    db_name: &str,
    schema: Option<&str>,
    worktree: &Path,
) -> Vec<String> {
    argv.iter()
        .map(|argument| {
            crate::slot_config::substitute(
                argument,
                &[
                    ("slot", name),
                    ("db", db_name),
                    ("schema", schema.unwrap_or("")),
                    ("worktree", &worktree.to_string_lossy()),
                ],
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::test_support::AlwaysAlive;
    use crate::slot_config::load_for_tests;
    use crate::store::probe_test_support::{AllPortsFree, OccupiedPorts};
    use std::sync::Arc;

    struct Fixture {
        _data: tempfile::TempDir,
        worktree: tempfile::TempDir,
        leases: Store,
        slots: SlotStore,
    }

    fn fixture() -> Fixture {
        fixture_with_probe(Arc::new(AllPortsFree))
    }

    fn fixture_with_probe(probe: Arc<dyn crate::store::PortProbe>) -> Fixture {
        let data = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let leases = Store::open_with_probe(Some(data.path().to_path_buf()), probe).unwrap();
        let slots = SlotStore::open(Some(data.path().to_path_buf())).unwrap();
        Fixture {
            _data: data,
            worktree,
            leases,
            slots,
        }
    }

    fn ctx(fixture: &Fixture, toml: &str) -> SlotContext {
        let worktree = canonicalize_or_owned(fixture.worktree.path());
        SlotContext {
            config: load_for_tests(&worktree, toml),
            repo_key: worktree.to_string_lossy().into_owned(),
            worktree,
        }
    }

    fn rt<'a>(fixture: &'a Fixture, checker: &'a dyn PidChecker) -> SlotRuntime<'a> {
        SlotRuntime::new(&fixture.leases, &fixture.slots, checker)
    }

    const BASIC: &str = r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3000
api = 3100
"#;

    #[test]
    fn up_allocates_a_port_per_name_and_claims_it_as_a_lease() {
        let fixture = fixture();
        let context = ctx(&fixture, BASIC);
        let runtime = rt(&fixture, &AlwaysAlive);

        let outcome = runtime.up(&context, "auth", false).unwrap();

        assert_eq!(outcome.record.ports.get("web"), Some(&3000));
        assert_eq!(outcome.record.ports.get("api"), Some(&3100));
        assert_eq!(
            fixture.leases.get(3000).unwrap().unwrap().tag,
            "slot:auth:web"
        );
        assert_eq!(
            fixture.leases.get(3100).unwrap().unwrap().tag,
            "slot:auth:api"
        );
        assert!(fixture.leases.get(3000).unwrap().unwrap().durable);
        assert!(
            fixture
                .slots
                .get(fixture.worktree.path(), "auth")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn up_walks_forward_when_the_preferred_port_is_taken() {
        // A durable lease from another slot: the preferred port is taken by
        // the allocator itself, no socket involved.
        let fixture = fixture();
        let runtime = rt(&fixture, &AlwaysAlive);
        let first = ctx(&fixture, BASIC);
        runtime.up(&first, "first", false).unwrap();

        let second = ctx(&fixture, BASIC);
        let outcome = runtime.up(&second, "second", false).unwrap();

        assert_eq!(outcome.record.ports.get("web"), Some(&3001));
        assert_eq!(outcome.record.ports.get("api"), Some(&3101));
        assert_eq!(
            fixture.leases.get(3000).unwrap().unwrap().tag,
            "slot:first:web"
        );

        // And the OS side of the walk, scripted through the injected probe.
        let occupied_fixture =
            fixture_with_probe(Arc::new(OccupiedPorts([3000u16].into_iter().collect())));
        let occupied_context = ctx(&occupied_fixture, BASIC);
        let occupied = rt(&occupied_fixture, &AlwaysAlive)
            .up(&occupied_context, "occupied", false)
            .unwrap();
        assert_eq!(occupied.record.ports.get("web"), Some(&3001));
    }

    #[test]
    fn up_creates_cache_dirs_and_writes_the_env_file() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
XDG_CACHE_HOME = ".slot/{name}/cache"

[slot.env]
RUST_LOG = "debug"
GREETING = "hello world"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);

        let outcome = runtime.up(&context, "auth", false).unwrap();

        let target = context.worktree.join(".slot/auth/target");
        let cache = context.worktree.join(".slot/auth/cache");
        assert!(target.is_dir());
        assert!(cache.is_dir());
        assert_eq!(
            outcome.record.cache_dirs,
            vec![cache.clone(), target.clone()]
        );

        let env_file = fs::read_to_string(context.worktree.join(ENV_FILE_NAME)).unwrap();
        assert!(env_file.contains("PORTZILLA_SLOT=auth\n"));
        assert!(env_file.contains("PORTZILLA_WORKTREE="));
        assert!(env_file.contains("PORTZILLA_PORT_WEB=3000\n"));
        assert!(env_file.contains("PORTZILLA_PORT=3000\n"));
        assert!(env_file.contains(&format!(
            "CARGO_TARGET_DIR={}\n",
            shell_value(&target.to_string_lossy())
        )));
        assert!(env_file.contains("XDG_CACHE_HOME="));
        assert!(env_file.contains("RUST_LOG=debug\n"));
        assert!(env_file.contains("GREETING=\"hello world\"\n"));
        assert!(!env_file.contains("PORTZILLA_DB"));
    }

    #[test]
    fn a_failure_after_allocation_releases_the_ports_and_removes_created_dirs() {
        let fixture = fixture();
        // The cache path is a file, so create_dir_all fails after the ports
        // were already claimed and before anything is persisted.
        let blocker = fixture.worktree.path().join(".slot");
        fs::write(&blocker, "not a directory").unwrap();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);

        let error = runtime.up(&context, "auth", false).unwrap_err();

        assert!(
            format!("{error:#}").contains("failed to create cache directory"),
            "unexpected error: {error:#}"
        );
        assert!(fixture.leases.get(3000).unwrap().is_none(), "port released");
        assert!(
            fixture
                .slots
                .get(fixture.worktree.path(), "auth")
                .unwrap()
                .is_none(),
            "nothing persisted"
        );
        assert!(!fixture.worktree.path().join(ENV_FILE_NAME).exists());
        assert_eq!(fs::read(&blocker).unwrap(), b"not a directory");
    }

    #[test]
    fn env_rendering_is_stable_and_quotes_values_with_spaces() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3000

[slot.env]
ZED = "last"
ALPHA = "first"
SPACED = "a b"
HASH = "a#b"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        let env = fixture
            .slots
            .get(fixture.worktree.path(), "auth")
            .unwrap()
            .unwrap()
            .env;
        let rendered = render_env_file(&env);
        // Sorted by key: stable output regardless of config order.
        let keys: Vec<&str> = rendered
            .lines()
            .map(|line| line.split_once('=').unwrap().0)
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert!(rendered.contains("SPACED=\"a b\"\n"));
        assert!(rendered.contains("HASH=\"a#b\"\n"));
        assert!(rendered.contains("ALPHA=first\n"));
        assert!(rendered.contains("ZED=last\n"));

        // Rendering the same environment twice is byte-identical.
        assert_eq!(rendered, render_env_file(&env));
    }

    #[test]
    fn a_value_that_would_break_the_env_file_is_quoted_and_escaped() {
        assert_eq!(shell_value("a b"), "\"a b\"");
        assert_eq!(shell_value("a#b"), "\"a#b\"");
        assert_eq!(shell_value("a$b"), "\"a\\$b\"");
        assert_eq!(shell_value("plain/path-1"), "plain/path-1");
        assert_eq!(shell_value(""), "\"\"");
    }

    #[test]
    fn a_second_up_without_replace_is_refused_and_leaves_the_first_alone() {
        let fixture = fixture();
        let context = ctx(&fixture, BASIC);
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        let error = runtime.up(&context, "auth", false).unwrap_err();
        assert!(format!("{error:#}").contains("already up"));
        assert_eq!(
            fixture.leases.get(3000).unwrap().unwrap().tag,
            "slot:auth:web"
        );
    }

    #[test]
    fn replace_tears_the_old_slot_down_before_creating_the_new_one() {
        let fixture = fixture();
        let context = ctx(&fixture, BASIC);
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        let replaced = runtime.up(&context, "auth", true).unwrap();
        assert!(replaced.replaced);
        assert_eq!(replaced.record.ports.get("web"), Some(&3000));
        assert_eq!(fixture.slots.list().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replace_creates_nothing_when_the_old_slot_cannot_be_deprovisioned() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3206

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"

[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", "true"]
deprovision = ["sh", "-c", "exit 9"]
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();
        let target = context.worktree.join(".slot/auth/target");
        assert!(target.is_dir());

        let error = runtime.up(&context, "auth", true).unwrap_err();

        assert!(
            format!("{error:#}").contains("failed to tear down the existing slot"),
            "unexpected error: {error:#}"
        );
        let record = fixture
            .slots
            .get(fixture.worktree.path(), "auth")
            .unwrap()
            .expect("the old slot survives a failed replace");
        assert_eq!(record.ports.get("web"), Some(&3206));
        assert_eq!(
            fixture.leases.get(3206).unwrap().unwrap().tag,
            "slot:auth:web"
        );
        assert!(target.is_dir(), "the old cache directory is untouched");
    }

    #[test]
    fn a_cache_path_outside_the_worktree_is_refused_before_allocating() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = "../outside/{name}"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);

        let error = runtime.up(&context, "auth", false).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside the worktree"),
            "unexpected error: {error:#}"
        );
        assert!(fixture.leases.get(3000).unwrap().is_none());
        // "../outside" must not have been created beside the worktree.
        let escaped = fixture.worktree.path().parent().unwrap().join("outside");
        assert!(!escaped.exists(), "refused path must not be created");
    }

    #[test]
    fn a_cache_path_owned_by_another_slot_is_refused() {
        let fixture = fixture();
        let shared = r#"
[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = ".slot/shared"
"#;
        let first = ctx(&fixture, shared);
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&first, "first", false).unwrap();

        let second = ctx(&fixture, shared);
        let error = runtime.up(&second, "second", false).unwrap_err();
        assert!(
            format!("{error:#}").contains("overlaps"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn down_releases_ports_purges_dirs_and_removes_the_record() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();
        let target = context.worktree.join(".slot/auth/target");
        assert!(target.is_dir());

        let outcome = runtime.down(&context.worktree, "auth", false).unwrap();

        assert_eq!(outcome.released_ports, vec![3000]);
        assert_eq!(outcome.purged_dirs, vec![target.clone()]);
        assert!(!target.exists());
        assert!(fixture.leases.get(3000).unwrap().is_none());
        assert!(fixture.slots.list().unwrap().is_empty());
        assert!(!context.worktree.join(ENV_FILE_NAME).exists());
    }

    #[test]
    fn down_keeps_dirs_when_purge_is_disabled() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3000

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();
        let target = context.worktree.join(".slot/auth/target");

        let outcome = runtime.down(&context.worktree, "auth", false).unwrap();
        assert!(outcome.purged_dirs.is_empty());
        assert!(target.is_dir());
        assert!(fixture.slots.list().unwrap().is_empty());
    }

    #[test]
    fn down_never_releases_a_lease_another_owner_has_taken() {
        let fixture = fixture();
        let context = ctx(&fixture, BASIC);
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        // Simulate the slot's lease disappearing and another owner taking the
        // port before teardown runs.
        fixture
            .leases
            .release_if_tagged(3000, "slot:auth:web", slot_actor("test"), &AlwaysAlive)
            .unwrap();
        let foreign = fixture
            .leases
            .claim_durable(
                3000,
                "slot:other:web".to_string(),
                Some("other".to_string()),
                &AlwaysAlive,
            )
            .unwrap();
        assert_eq!(foreign.lease.port, 3000);

        let outcome = runtime.down(&context.worktree, "auth", false).unwrap();
        assert!(!outcome.released_ports.contains(&3000));
        assert_eq!(outcome.released_ports, vec![3100]);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("not by slot auth")),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
        assert_eq!(
            fixture.leases.get(3000).unwrap().unwrap().tag,
            "slot:other:web"
        );
    }

    #[test]
    fn down_refuses_to_remove_an_env_file_another_slot_wrote() {
        let fixture = fixture();
        let worktree = canonicalize_or_owned(fixture.worktree.path());
        let context = ctx(&fixture, BASIC);
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        // A second slot in the same worktree overwrites the shared env file.
        let other = ctx(&fixture, BASIC);
        runtime.up(&other, "other", false).unwrap();
        let env_file = worktree.join(ENV_FILE_NAME);
        let other_contents = fs::read_to_string(&env_file).unwrap();

        // `down auth` no longer owns 3000 (other took it) and must not remove
        // the file the second slot wrote.
        let outcome = runtime.down(&worktree, "auth", false).unwrap();
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("no longer matches"))
        );
        assert_eq!(fs::read_to_string(&env_file).unwrap(), other_contents);

        runtime.down(&worktree, "other", false).unwrap();
        assert!(!env_file.exists());
    }

    #[test]
    fn prune_removes_vanished_worktrees_and_frees_their_ports() {
        let fixture = fixture();
        let context = ctx(&fixture, BASIC);
        let runtime = SlotRuntime::new(&fixture.leases, &fixture.slots, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        // Drop the worktree so its path no longer exists. The runtime above
        // borrows only the store fields, so this partial move is allowed.
        let worktree = fixture.worktree.path().to_path_buf();
        drop(fixture.worktree);

        let outcome = runtime.prune().unwrap();
        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.removed[0].name, "auth");
        assert!(fixture.slots.list().unwrap().is_empty());
        assert!(fixture.leases.get(3000).unwrap().is_none());
        assert!(!worktree.exists());
    }
    #[test]
    fn db_names_are_rendered_and_deprovision_argv_is_persisted() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3200

[slot.db]
name_template = "app_{slot}"
schema_template = "slot_{slot}"
provision = []
deprovision = ["dropdb", "--if-exists", "{db}", "--worktree", "{worktree}"]
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        let outcome = runtime.up(&context, "auth", false).unwrap();

        let db = outcome.record.db.as_ref().expect("db recorded");
        assert_eq!(db.name, "app_auth");
        assert_eq!(db.schema.as_deref(), Some("slot_auth"));
        let deprovision = db.deprovision.as_ref().expect("deprovision recorded");
        assert_eq!(
            deprovision,
            &vec![
                "dropdb".to_string(),
                "--if-exists".to_string(),
                "app_auth".to_string(),
                "--worktree".to_string(),
                context.worktree.to_string_lossy().into_owned(),
            ]
        );
        // The environment carries both names for provision/deprovision.
        assert_eq!(db.name, outcome.record.env["PORTZILLA_DB"]);
        assert_eq!("slot_auth", outcome.record.env["PORTZILLA_DB_SCHEMA"]);
    }

    #[cfg(unix)]
    #[test]
    fn provision_receives_the_substituted_argv_and_inherits_the_environment() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3201

[slot.db]
name_template = "app_{slot}"
schema_template = "slot_{slot}"
provision = ["sh", "-c", 'printf "%s|%s|%s" "$PORTZILLA_DB" "$PORTZILLA_DB_SCHEMA" "$PORTZILLA_SLOT" > {worktree}/provision.txt']
deprovision = ["sh", "-c", 'printf done > {worktree}/deprovisioned.txt']
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);

        runtime.up(&context, "auth", false).unwrap();

        let written = fs::read_to_string(context.worktree.join("provision.txt")).unwrap();
        assert_eq!(written, "app_auth|slot_auth|auth");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_provision_command_rolls_the_slot_back() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot.ports]
web = 3202

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"

[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", 'echo provision-broke >&2; printf partial > {worktree}/provision-attempted; exit 3']
deprovision = ["sh", "-c", 'printf cleaned > {worktree}/deprovision-ran']
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);

        let error = runtime.up(&context, "auth", false).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("provision-broke"),
            "stderr surfaced: {message}"
        );
        assert!(
            message.contains("failed to provision"),
            "unexpected error: {message}"
        );
        assert!(fixture.leases.get(3202).unwrap().is_none(), "port released");
        assert!(
            fixture
                .slots
                .get(fixture.worktree.path(), "auth")
                .unwrap()
                .is_none(),
            "nothing persisted"
        );
        assert!(!context.worktree.join(".slot/auth/target").exists());
        assert!(!context.worktree.join(ENV_FILE_NAME).exists());
        // The failed provision is evidence the command ran; rollback then ran
        // deprovision so a half-created database does not survive.
        assert!(context.worktree.join("provision-attempted").exists());
        assert!(context.worktree.join("deprovision-ran").exists());
    }

    #[cfg(unix)]
    #[test]
    fn down_runs_deprovision_then_releases_ports_and_purges_dirs() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3203

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"

[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", 'printf db > {worktree}/db-created']
deprovision = ["sh", "-c", 'printf order > {worktree}/.slot/auth/target/order.txt; rm -f {worktree}/db-created']
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();
        let target = context.worktree.join(".slot/auth/target");
        assert!(context.worktree.join("db-created").exists());

        // Deprovision writes into the cache directory: it can only succeed if
        // purge has not run yet, so a successful `down` proves the order.
        let outcome = runtime.down(&context.worktree, "auth", false).unwrap();

        assert_eq!(outcome.released_ports, vec![3203]);
        assert_eq!(outcome.purged_dirs, vec![target.clone()]);
        assert!(!target.exists());
        assert!(!context.worktree.join("db-created").exists());
        assert!(fixture.leases.get(3203).unwrap().is_none());
        assert!(fixture.slots.list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn down_keeps_everything_when_deprovision_fails() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3204

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"

[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", 'printf db > {worktree}/db-created']
deprovision = ["sh", "-c", 'echo deprovision-broke >&2; exit 4']
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();

        let error = runtime.down(&context.worktree, "auth", false).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("deprovision-broke"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("--force"),
            "the retry hint must be visible: {message}"
        );

        // Nothing was torn down: the user can fix the command and retry.
        assert!(
            fixture
                .slots
                .get(fixture.worktree.path(), "auth")
                .unwrap()
                .is_some()
        );
        assert!(fixture.leases.get(3204).unwrap().is_some());
        assert!(context.worktree.join(".slot/auth/target").is_dir());
        assert!(context.worktree.join("db-created").exists());

        // The forced retry discards the broken step and cleans up.
        let forced = runtime.down(&context.worktree, "auth", true).unwrap();
        assert!(forced.forced);
        assert!(
            forced
                .warnings
                .iter()
                .any(|warning| warning.contains("deprovision failed"))
        );
        assert!(fixture.slots.list().unwrap().is_empty());
        assert!(fixture.leases.get(3204).unwrap().is_none());
        assert!(!context.worktree.join(".slot/auth/target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn down_force_cleans_up_even_when_deprovision_fails() {
        let fixture = fixture();
        let context = ctx(
            &fixture,
            r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3205

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/lost+found"

[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", "true"]
deprovision = ["sh", "-c", "exit 7"]
"#,
        );
        let runtime = rt(&fixture, &AlwaysAlive);
        runtime.up(&context, "auth", false).unwrap();
        assert!(context.worktree.join(".slot/auth/lost+found").is_dir());

        let outcome = runtime.down(&context.worktree, "auth", true).unwrap();

        assert!(outcome.forced);
        assert!(
            fixture
                .slots
                .get(fixture.worktree.path(), "auth")
                .unwrap()
                .is_none()
        );
        assert!(fixture.leases.get(3205).unwrap().is_none());
        assert!(!context.worktree.join(".slot/auth/lost+found").exists());
    }
}
