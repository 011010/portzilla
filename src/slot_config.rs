//! `portzilla.toml`: the per-worktree recipe for an environment slot.
//!
//! A slot bundles four kinds of per-worktree resources: ports, cache
//! directories, environment variables, and an optional database. The file is
//! read once, validated completely, and only then does anything run — a typo
//! in a provision command must fail before a port is claimed or a directory is
//! created, not halfway through `slot up`.
//!
//! # Placeholders
//!
//! Templates are intentionally literal: a `{token}` is substituted only when
//! the field's allow-list names that token, and anything else is an error that
//! names the offending token. There is no shell here, so there are no quoting
//! or escaping rules to get wrong.
//!
//! - cache paths: `{name}`
//! - `[slot.db] name_template` and `schema_template`: `{slot}`
//! - `provision` / `deprovision` argv elements: `{slot}`, `{db}`, `{schema}`,
//!   `{worktree}`
//!
//! A `{` with no matching `}` is left alone: it is far more likely to be part
//! of a shell snippet (`awk '{print $1}'`) than a mistyped placeholder.
//!
//! # Provenance for `slot down`
//!
//! This module never persists a rendered command. `slot up` renders the
//! deprovision argv and stores it in the slot record, so teardown does not
//! depend on the config file still existing (or still being identical) when
//! the worktree is cleaned up.

use crate::intent::normalize_path;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// File name looked up at the worktree root when `--config` is absent.
pub const CONFIG_FILE_NAME: &str = "portzilla.toml";

/// Environment variable names this module reserves for values it computes
/// itself. Allowing `[slot.env] PORTZILLA_PORT = "..."` would let a config
/// silently overwrite the port `slot up` just allocated.
const RESERVED_ENV_PREFIX: &str = "PORTZILLA_";

/// Maximum length of a slot name, in characters. Slot names become lease tags,
/// directory names through `{name}`, and database names through `{slot}`;
/// keeping them short and boring keeps every one of those safe.
pub const MAX_SLOT_NAME_CHARS: usize = 64;

/// The parsed and validated contents of one `portzilla.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotConfig {
    /// Remove the recorded cache directories on `slot down`.
    pub purge_cache_on_down: bool,
    /// Port name -> preferred base port. Names are stable identifiers used in
    /// the lease tag and in the `PORTZILLA_PORT_<NAME>` variable.
    pub ports: BTreeMap<String, u16>,
    /// Environment variable -> path template, relative to the worktree unless
    /// the template is absolute.
    pub cache: BTreeMap<String, String>,
    /// Extra environment variables exported by `slot env` and `slot run`.
    pub env: BTreeMap<String, String>,
    /// Optional database recipe.
    pub db: Option<DbConfig>,
}

/// The `[slot.db]` table. Every command is an argv array, never a shell
/// string; write `["sh", "-c", "..."]` if a shell is really wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbConfig {
    pub name_template: String,
    pub schema_template: Option<String>,
    pub provision: Vec<String>,
    pub deprovision: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    slot: RawSlot,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    #[serde(default)]
    purge_cache_on_down: bool,
    #[serde(default)]
    ports: BTreeMap<String, u16>,
    #[serde(default)]
    cache: BTreeMap<String, String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    db: Option<RawDb>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDb {
    name_template: String,
    #[serde(default)]
    schema_template: Option<String>,
    provision: Vec<String>,
    #[serde(default)]
    deprovision: Option<Vec<String>>,
}

impl SlotConfig {
    /// Reads and validates `portzilla.toml` for `worktree`.
    ///
    /// `explicit` is the `--config` path; otherwise `<worktree>/portzilla.toml`
    /// is used. Every check here is side-effect free, so a rejected config
    /// never leaves a directory, a lease, or a database behind.
    pub fn load(worktree: &Path, explicit: Option<&Path>) -> Result<Self> {
        let path = explicit.map_or_else(|| worktree.join(CONFIG_FILE_NAME), Path::to_path_buf);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no slot config found at {}; create {CONFIG_FILE_NAME} at the worktree root \
                     or pass --config <path>",
                    path.display()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read slot config at {}", path.display()));
            }
        };
        let file: FileConfig = toml::from_str(&text).with_context(|| {
            format!(
                "slot config at {} is not valid TOML (expected a [slot] table)",
                path.display()
            )
        })?;
        Self::from_raw(file.slot)
            .with_context(|| format!("slot config at {} is invalid", path.display()))
    }

    fn from_raw(raw: RawSlot) -> Result<Self> {
        for name in raw.ports.keys() {
            validate_port_name(name)?;
        }
        for (name, path) in &raw.cache {
            validate_env_key(name, "cache")?;
            validate_env_value(path, &format!("cache path for {name}"))?;
            validate_template(path, &["name"], &format!("cache path for {name}"))?;
        }
        for (name, value) in &raw.env {
            validate_env_key(name, "slot.env")?;
            validate_env_value(value, &format!("slot.env value for {name}"))?;
        }
        // `[slot.cache]` overrides `[slot.env]` on conflict (the resolved path
        // is what a build tool must use). Both are reserved-prefix checked, so
        // a collision between them is documented in SLOTS.md rather than an
        // error: the cache entry is simply the one that wins.
        let db = raw.db.map(DbConfig::from_raw).transpose()?;
        Ok(Self {
            purge_cache_on_down: raw.purge_cache_on_down,
            ports: raw.ports,
            cache: raw.cache,
            env: raw.env,
            db,
        })
    }

    /// Renders a cache path for `slot_name` and resolves it against
    /// `worktree`. The result is lexically normalized but need not exist.
    pub fn cache_path(&self, worktree: &Path, slot_name: &str, template: &str) -> PathBuf {
        let rendered = substitute(template, &[("name", slot_name)]);
        let path = Path::new(&rendered);
        if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&worktree.join(path))
        }
    }

    /// Renders the database name for `slot_name`, or `None` without a
    /// `[slot.db]` table.
    pub fn db_name(&self, slot_name: &str) -> Option<String> {
        self.db
            .as_ref()
            .map(|db| substitute(&db.name_template, &[("slot", slot_name)]))
    }

    /// Renders the schema name for `slot_name`, when configured.
    pub fn db_schema(&self, slot_name: &str) -> Option<String> {
        self.db.as_ref().and_then(|db| {
            db.schema_template
                .as_ref()
                .map(|template| substitute(template, &[("slot", slot_name)]))
        })
    }
}

impl DbConfig {
    fn from_raw(raw: RawDb) -> Result<Self> {
        if raw.name_template.trim().is_empty() {
            bail!("slot.db.name_template must not be empty");
        }
        validate_template(&raw.name_template, &["slot"], "slot.db.name_template")?;
        if let Some(schema) = &raw.schema_template {
            if schema.trim().is_empty() {
                bail!("slot.db.schema_template must not be empty when present");
            }
            validate_template(schema, &["slot"], "slot.db.schema_template")?;
        }
        for (index, argument) in raw.provision.iter().enumerate() {
            validate_command_argument(
                argument,
                &format!("slot.db.provision[{index}]"),
                raw.schema_template.is_some(),
            )?;
        }
        if let Some(deprovision) = &raw.deprovision {
            for (index, argument) in deprovision.iter().enumerate() {
                validate_command_argument(
                    argument,
                    &format!("slot.db.deprovision[{index}]"),
                    raw.schema_template.is_some(),
                )?;
            }
        }
        Ok(Self {
            name_template: raw.name_template,
            schema_template: raw.schema_template,
            provision: raw.provision,
            deprovision: raw.deprovision,
        })
    }
}

/// Rejects slot names that would be ambiguous in a lease tag, unsafe as a
/// directory name through `{name}`, or unsafe as a database name through
/// `{slot}`. Dots are refused too, so `..` can never appear in a rendered
/// cache path.
pub fn validate_slot_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("slot name must not be empty");
    }
    if name.chars().count() > MAX_SLOT_NAME_CHARS {
        bail!(
            "invalid slot name: at most {MAX_SLOT_NAME_CHARS} characters, got {}",
            name.chars().count()
        );
    }
    let mut characters = name.chars();
    let first = characters.next().expect("non-empty name");
    if !first.is_ascii_alphanumeric() {
        bail!("invalid slot name {name:?}: must start with an ASCII letter or digit");
    }
    if let Some(character) =
        characters.find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        bail!(
            "invalid slot name {name:?}: character {character:?} is not allowed \
             (use ASCII letters, digits, '_' or '-')"
        );
    }
    Ok(())
}

fn validate_port_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("slot.ports contains an empty port name");
    }
    let mut characters = name.chars();
    let first = characters.next().expect("non-empty port name");
    if !first.is_ascii_alphabetic() {
        bail!("invalid port name {name:?}: must start with an ASCII letter");
    }
    if let Some(character) =
        characters.find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        bail!(
            "invalid port name {name:?}: character {character:?} is not allowed \
             (use ASCII letters, digits, '_' or '-')"
        );
    }
    Ok(())
}

/// Validates an environment key. Empty keys, keys with `=`, and keys that are
/// not shell identifiers would produce an env file that cannot be sourced.
fn validate_env_key(key: &str, table: &str) -> Result<()> {
    let mut characters = key.chars();
    let valid = characters
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!(
            "invalid {table} key {key:?}: use an ASCII shell identifier ([A-Za-z_][A-Za-z0-9_]*)"
        );
    }
    if key.starts_with(RESERVED_ENV_PREFIX) {
        bail!(
            "{table} key {key:?} uses the reserved {RESERVED_ENV_PREFIX} prefix; \
             portzilla computes those values itself"
        );
    }
    Ok(())
}

/// Values are written one per line into `.env.portzilla`; a control character
/// would break the line-oriented format (and `eval`). Spaces are fine — the
/// writer quotes them.
fn validate_env_value(value: &str, field: &str) -> Result<()> {
    if value.chars().any(|character| character.is_control()) {
        bail!("{field} contains a control character; use a literal value without newlines");
    }
    Ok(())
}

fn validate_command_argument(argument: &str, field: &str, has_schema: bool) -> Result<()> {
    if argument.is_empty() {
        bail!("{field} must not be empty");
    }
    validate_env_value(argument, field)?;
    validate_template(argument, &["slot", "db", "schema", "worktree"], field)?;
    if !has_schema && references_token(argument, "schema") {
        bail!("{field} uses {{schema}} but slot.db.schema_template is not set");
    }
    Ok(())
}

/// Rejects any identifier-shaped `{token}` not in `allowed`, naming the
/// offending token and the field. Called at load time so no command runs with
/// an unresolved placeholder.
///
/// Shell snippets are the reason this is not a blanket "every brace pair must
/// be a placeholder" rule. `["sh", "-c", "awk '{print $1}'"]`,
/// `psql -c 'SELECT ${PGUSER}'` and similar are legitimate argv commands;
/// their braces are data, not templates. A token is a placeholder candidate
/// only when it is a bare identifier (`[A-Za-z_][A-Za-z0-9_]*`) and not
/// preceded by `$` (shell parameter expansion). Everything else is left
/// untouched, and [`substitute`] applies the same rule.
fn validate_template(template: &str, allowed: &[&str], field: &str) -> Result<()> {
    let mut rest = template;
    let mut consumed = 0;
    while let Some(start) = rest.find('{') {
        let absolute_start = consumed + start;
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // An unterminated `{` is a literal, not a broken placeholder.
            break;
        };
        let token = &after[..end];
        let shell_parameter =
            absolute_start > 0 && template.as_bytes().get(absolute_start - 1) == Some(&b'$');
        if !shell_parameter && is_identifier(token) && !allowed.contains(&token) {
            let allowed_list = allowed
                .iter()
                .map(|token| format!("{{{token}}}"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "{field} uses unknown placeholder {{{token}}}; allowed placeholders: {allowed_list}"
            );
        }
        consumed = absolute_start + 1 + end + 1;
        rest = &template[consumed..];
    }
    Ok(())
}

/// True for a bare `[A-Za-z_][A-Za-z0-9_]*` token, the only shape treated as a
/// placeholder candidate.
fn is_identifier(token: &str) -> bool {
    let mut characters = token.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_');
    valid_start && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn references_token(template: &str, token: &str) -> bool {
    substitute(template, &[(token, "\u{0}")]).contains('\u{0}')
}

/// Replaces every identifier-shaped `{key}` in `template` with its value.
/// Non-identifier braces (`${PGUSER}`, `awk '{print $1}'`) are data and are
/// re-emitted unchanged; see [`validate_template`].
pub fn substitute(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let token = &after[..end];
        match values.iter().find(|(key, _)| *key == token) {
            Some((_, value)) if is_identifier(token) => output.push_str(value),
            _ => {
                output.push('{');
                output.push_str(token);
                output.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
}

/// A set of `(env var, resolved path)` pairs, with the cache env names kept
/// stable for comparisons and purge bookkeeping.
pub fn cache_entries(
    config: &SlotConfig,
    worktree: &Path,
    slot_name: &str,
) -> Vec<(String, PathBuf)> {
    config
        .cache
        .iter()
        .map(|(env, template)| {
            (
                env.clone(),
                config.cache_path(worktree, slot_name, template),
            )
        })
        .collect()
}

/// Returns the distinct resolved cache directories, sorted, so the store and
/// the purger see one entry per real path even when two env vars point at it.
pub fn distinct_cache_dirs(entries: &[(String, PathBuf)]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    for (_, path) in entries {
        seen.insert(path.clone());
    }
    seen.into_iter().collect()
}

/// Test-only: writes `toml` as the worktree's `portzilla.toml` and loads it.
/// Keeps runtime tests focused on effects rather than on file plumbing.
#[cfg(test)]
pub(crate) fn load_for_tests(worktree: &Path, toml: &str) -> SlotConfig {
    let path = worktree.join(CONFIG_FILE_NAME);
    fs::write(&path, toml).expect("write test config");
    SlotConfig::load(worktree, Some(&path)).expect("test config must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempConfig {
        _dir: tempfile::TempDir,
        worktree: PathBuf,
        path: PathBuf,
    }

    fn temp_config(contents: &str) -> TempConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, contents).unwrap();
        TempConfig {
            worktree: dir.path().to_path_buf(),
            path,
            _dir: dir,
        }
    }

    const FULL_CONFIG: &str = r#"
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3000
api = 3100

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
XDG_CACHE_HOME = ".slot/{name}/cache"

[slot.env]
RUST_LOG = "debug"

[slot.db]
name_template = "app_{slot}"
schema_template = "slot_{slot}"
provision = ["createdb", "{db}"]
deprovision = ["dropdb", "--if-exists", "{db}"]
"#;

    #[test]
    fn parses_ports_cache_env_and_db() {
        let temp = temp_config(FULL_CONFIG);
        let config = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap();

        assert!(config.purge_cache_on_down);
        assert_eq!(config.ports.get("web"), Some(&3000));
        assert_eq!(config.ports.get("api"), Some(&3100));
        assert_eq!(
            config.cache.get("CARGO_TARGET_DIR").map(String::as_str),
            Some(".slot/{name}/target")
        );
        assert_eq!(
            config.env.get("RUST_LOG").map(String::as_str),
            Some("debug")
        );

        let db = config.db.as_ref().expect("db table");
        assert_eq!(db.name_template, "app_{slot}");
        assert_eq!(db.schema_template.as_deref(), Some("slot_{slot}"));
        assert_eq!(db.provision, ["createdb", "{db}"]);
        assert_eq!(
            db.deprovision.as_deref(),
            Some(
                &[
                    "dropdb".to_string(),
                    "--if-exists".to_string(),
                    "{db}".to_string()
                ][..]
            )
        );

        assert_eq!(config.db_name("auth").as_deref(), Some("app_auth"));
        assert_eq!(config.db_schema("auth").as_deref(), Some("slot_auth"));
    }

    #[test]
    fn a_missing_file_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = SlotConfig::load(dir.path(), None).unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains("no slot config found at"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("portzilla.toml"),
            "the message must name the expected file: {message}"
        );
        assert!(
            message.contains(&dir.path().display().to_string()),
            "the message must name the expected path: {message}"
        );
    }

    #[test]
    fn an_unknown_placeholder_is_rejected_before_anything_runs() {
        let temp = temp_config(
            r#"
[slot.db]
name_template = "app_{slot}"
provision = ["createdb", "{nope}"]
"#,
        );
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();
        let message = format!("{err:#}");

        assert!(
            message.contains("{nope}"),
            "the error must name the offending token: {message}"
        );
        assert!(
            message.contains("provision[1]"),
            "the error must name the field: {message}"
        );
    }

    #[test]
    fn relative_cache_paths_resolve_against_the_worktree() {
        let temp = temp_config(FULL_CONFIG);
        let config = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap();

        let path = config.cache_path(&temp.worktree, "auth", ".slot/{name}/target");
        assert_eq!(path, temp.worktree.join(".slot/auth/target"));

        let absolute = config.cache_path(&temp.worktree, "auth", "/tmp/elsewhere/{name}");
        assert_eq!(absolute, PathBuf::from("/tmp/elsewhere/auth"));

        let entries = cache_entries(&config, &temp.worktree, "auth");
        assert_eq!(entries.len(), 2);
        assert_eq!(distinct_cache_dirs(&entries).len(), 2);
    }

    #[test]
    fn provision_is_argv_not_a_shell_string() {
        let temp = temp_config(
            r#"
[slot.db]
name_template = "app_{slot}"
provision = "createdb {db}"
"#,
        );
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();
        let message = format!("{err:#}");

        assert!(
            message.contains("not valid TOML"),
            "a string provision must fail to deserialize: {message}"
        );
    }

    #[test]
    fn a_schema_placeholder_without_a_schema_template_is_rejected() {
        let temp = temp_config(
            r#"
[slot.db]
name_template = "app_{slot}"
provision = ["createdb", "--schema={schema}", "{db}"]
"#,
        );
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();

        assert!(
            format!("{err:#}").contains("{schema} but slot.db.schema_template is not set"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn reserved_and_malformed_environment_keys_are_rejected() {
        for key in ["PORTZILLA_PORT", "not-a-shell-name", "1LEADING"] {
            let temp = temp_config(&format!("[slot.env]\n{key} = \"value\"\n"));
            let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();
            assert!(
                format!("{err:#}").contains(&format!("{key:?}")),
                "the error must name the key {key:?}: {err:#}"
            );
        }
    }

    #[test]
    fn a_control_character_in_a_value_is_rejected() {
        let temp = temp_config("[slot.env]\nMULTILINE = \"a\\nb\"\n");
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();

        assert!(
            format!("{err:#}").contains("control character"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn an_unterminated_brace_is_a_literal_not_an_unknown_placeholder() {
        let temp = temp_config(
            r#"
[slot.db]
name_template = "app_{slot}"
provision = ["sh", "-c", "awk '{print $1}' && echo ${PGUSER}"]
"#,
        );
        let config =
            SlotConfig::load(&temp.worktree, Some(&temp.path)).expect("shell braces are allowed");

        let rendered = substitute(
            &config.db.unwrap().provision[2],
            &[("slot", "auth"), ("db", "app_auth")],
        );
        assert_eq!(rendered, "awk '{print $1}' && echo ${PGUSER}");
    }

    #[test]
    fn a_config_without_a_slot_table_is_rejected() {
        let temp = temp_config("[something_else]\nvalue = 1\n");
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();

        assert!(
            format!("{err:#}").contains("not valid TOML"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn unknown_fields_are_rejected_so_a_typo_cannot_be_silently_ignored() {
        let temp = temp_config("[slot]\npurge_cache_on_dow = true\n");
        let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();

        assert!(
            format!("{err:#}").contains("purge_cache_on_dow"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn port_names_must_be_stable_identifiers() {
        for name in ["", "1web", "web/3000", "web port"] {
            let temp = temp_config(&format!("[slot.ports]\n\"{name}\" = 3000\n"));
            let err = SlotConfig::load(&temp.worktree, Some(&temp.path)).unwrap_err();
            assert!(
                format!("{err:#}").contains("port name"),
                "unexpected error for {name:?}: {err:#}"
            );
        }
    }
}
