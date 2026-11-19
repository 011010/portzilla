mod audit;
mod claude_code;
mod codex;
mod conflict;
mod cursor;
mod gemini;
mod guard;
mod guard_cmd;
mod hook_common;
mod intent;
mod kimi;
mod lease;
mod mcp;
mod opencode;
mod slot;
mod slot_config;
mod slot_runtime;
mod store;
mod view;
mod watch;
mod windsurf;

use anyhow::{Context, Result};
use audit::{
    AuditActor, AuditEventKind, AuditEventType, AuditSource, HistoryPage, HistoryQuery,
    LeaseSnapshot, ProcessExitOutcome,
};
use clap::{Parser, Subcommand};
use intent::{Intent, IntentStore, key_for_file, locate_file};
use lease::{Lease, PidChecker, SystemPidChecker};
use serde::Serialize;
use slot::SlotRecord;
use slot::SlotStore;
use slot_config::SlotConfig;
use slot_runtime::{SlotContext, SlotRuntime};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use store::{ClaimOutcome, Store};
use view::{IntentView, LeaseView, to_claim_view, to_intent_view, to_view};

/// Maximum number of bytes the hook runners will read from stdin before
/// giving up. Hook payloads are JSON with a single command string; anything
/// larger than 1 MiB is either a programming error or an attempt to
/// exhaust portzilla's memory via a hostile harness. Exceeding the limit
/// is treated like a malformed payload (fail-open in the default mode,
/// fail-closed under `PORTZILLA_FAIL_CLOSED`).
const HOOK_STDIN_LIMIT: usize = 1 << 20;

/// Returns true if `PORTZILLA_FAIL_CLOSED` is set to `"1"` or `"true"`.
/// Under fail-closed, a portzilla-side failure (corrupt store, unreadable
/// stdin, oversized payload) flips the verdict from allow to deny instead
/// of the default fail-open behavior. Fail-open is the default because a
/// guard that blocks a legitimate command over a portzilla-side problem is
/// worse than no guard at all — this opt-in flips that tradeoff for users
/// who specifically prefer the "block when unverified" end of it.
fn fail_closed_mode() -> bool {
    std::env::var("PORTZILLA_FAIL_CLOSED").is_ok_and(|v| v == "1" || v == "true")
}

/// Reads hook stdin into a `String`, with a hard byte cap. Returns a
/// human-readable reason on failure rather than an `io::Error` so the
/// callers can render it directly into a fail-closed deny message.
fn read_hook_stdin(limit: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("failed to read stdin: {err}"))?;
    if bytes.len() > limit {
        return Err(format!("payload exceeds the {} byte limit", limit));
    }
    String::from_utf8(bytes).map_err(|err| format!("payload is not valid UTF-8: {err}"))
}

/// Builds the standard message shown when a portzilla-side failure flips
/// the verdict to deny under fail-closed mode. The `cause` is the
/// short reason (e.g. "corrupt state file", "payload exceeds the 1048576
/// byte limit"), and the body goes to the adapter that owns the deny shape.
fn fail_closed_reason(cause: &str) -> String {
    format!(
        "portzilla could not verify this command's safety ({cause}). PORTZILLA_FAIL_CLOSED is \
         enabled, so unverified commands are denied instead of allowed."
    )
}

/// Port/process lease coordinator for parallel AI coding-agent sessions.
#[derive(Parser)]
#[command(name = "portzilla", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Claim a local port, recording who owns it and why.
    Claim {
        /// The port to claim (1-65535). Port 0 is reserved by the OS and
        /// cannot be leased; clap rejects it with a clear error.
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Human-readable description of what this port is for.
        #[arg(long)]
        tag: String,
        /// PID of the owning process. Defaults to the parent process's PID.
        #[arg(long)]
        pid: Option<u32>,
        /// Optional session identifier grouping related leases.
        #[arg(long)]
        session: Option<String>,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List all recorded leases.
    Ls {
        /// Print machine-readable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Show the lease recorded for a single port.
    Who {
        /// The port to look up.
        port: u16,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Remove the lease recorded for a port.
    Release {
        /// The port to release.
        port: u16,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Remove every lease whose owning process is no longer alive.
    Prune {
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Query or clear the bounded event history.
    History {
        /// Clear all retained events instead of querying them.
        #[arg(value_enum)]
        action: Option<HistoryAction>,
        #[arg(long, conflicts_with = "action")]
        session: Option<String>,
        #[arg(long, conflicts_with = "action")]
        port: Option<u16>,
        #[arg(long, value_enum, conflicts_with = "action")]
        event: Option<AuditEventType>,
        #[arg(long, conflicts_with = "action")]
        before: Option<u64>,
        #[arg(long, conflicts_with = "action")]
        limit: Option<usize>,
        #[arg(long, conflicts_with = "action")]
        json: bool,
    },
    /// Periodically remove leases whose owning processes have exited.
    Watch {
        /// Seconds between lease-pruning cycles (default: 60).
        #[arg(
            long,
            value_name = "SECONDS",
            default_value_t = watch::DEFAULT_INTERVAL_SECS,
            value_parser = watch::parse_interval_secs
        )]
        interval: u64,
        /// Print machine-readable JSON cycle events.
        #[arg(long)]
        json: bool,
    },
    /// Run portzilla as a long-lived server process.
    Serve {
        /// Run the MCP (Model Context Protocol) server over stdio, exposing
        /// claim/who/ls/release/prune as MCP tools. Required (rather than a
        /// default mode) so `serve` can grow other modes later without an
        /// ambiguous "what did plain `serve` just do" default.
        #[arg(long)]
        mcp: bool,
    },
    /// Run portzilla as a kill-guard hook for an AI coding agent harness.
    Hook {
        #[command(subcommand)]
        harness: HookHarness,
    },
    /// Print setup instructions for integrating portzilla with a harness.
    Init {
        #[command(subcommand)]
        harness: InitHarness,
    },
    /// Run a command through the kill-guard directly: for harnesses with no
    /// hook mechanism, and for manual or scripted use. Denies (exit 2,
    /// command NOT executed) if the command targets a live lease owned by
    /// someone else; warns to stderr and executes otherwise.
    Guard {
        /// Session identifier for "this is my own lease" recognition.
        /// Falls back to the PORTZILLA_SESSION environment variable, then
        /// to no session at all (every live foreign-looking lease is then
        /// deny-worthy).
        #[arg(long)]
        session: Option<String>,
        /// The command to run, exactly as given after `--`. Executed
        /// directly (no shell) — for a pipe or compound command, wrap it as
        /// `-- sh -c '...'`.
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Claim a port and run a command with it: the lease is held by the
    /// child process while it runs, so `who` names the real server PID and
    /// `prune` reaps it when the child exits.
    Run {
        /// The port to claim (1-65535). On conflict the next free port is
        /// claimed instead and the child is told the actual port.
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Human-readable description of what this port is for.
        #[arg(long)]
        tag: String,
        /// Optional session identifier grouping related leases.
        #[arg(long)]
        session: Option<String>,
        /// The command to run, exactly as given after `--`. Executed
        /// directly (no shell) — for a pipe or compound command, wrap it as
        /// `-- sh -c '...'`.
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Inspect and manage file intents: which session is editing which file.
    Intent {
        #[command(subcommand)]
        command: IntentCommands,
    },
    /// Manage per-worktree environment slots: ports, cache directories, an
    /// environment file, and an optional database.
    Slot {
        #[command(subcommand)]
        command: SlotCommands,
    },
}

#[derive(Subcommand)]
enum IntentCommands {
    /// Claim a file for this session so another session's edits are checked
    /// against it.
    Claim {
        /// The file to claim, absolute or relative to the current directory.
        path: PathBuf,
        /// How long the claim stays binding, in seconds (default: 1800,
        /// maximum: 86400).
        #[arg(long)]
        ttl_secs: Option<u64>,
        /// Session id to claim under. Falls back to PORTZILLA_SESSION, then
        /// CLAUDE_CODE_SESSION_ID.
        #[arg(long)]
        session: Option<String>,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Show which session holds a file.
    Who {
        /// The file to look up.
        path: PathBuf,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List live file claims.
    Ls {
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Release this session's claim before it expires.
    Release {
        /// The file whose claim should be released.
        path: PathBuf,
        /// Session id releasing the claim. Falls back to PORTZILLA_SESSION,
        /// then CLAUDE_CODE_SESSION_ID.
        #[arg(long)]
        session: Option<String>,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum HistoryAction {
    Clear,
}

#[derive(Subcommand)]
enum SlotCommands {
    /// Allocate the slot's ports, create its cache directories, and write
    /// `.env.portzilla` in the worktree. Any failure rolls everything back.
    Up {
        /// Slot name, unique within the worktree.
        name: String,
        /// Config file to read instead of `<worktree>/portzilla.toml`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Tear down an existing slot with this name first.
        #[arg(long)]
        replace: bool,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Print the slot environment as `KEY=value` lines, ready for `eval` or
    /// direnv.
    Env {
        /// Slot name.
        name: String,
        /// Reload `[slot.env]` values from this config. Allocated values
        /// (ports, database names, cache paths) always come from the stored
        /// slot.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run a command with the slot environment injected. The child's exit code
    /// is forwarded.
    Run {
        /// Slot name.
        name: String,
        /// Reload `[slot.env]` values from this config before running.
        #[arg(long)]
        config: Option<PathBuf>,
        /// The command to run, exactly as given after `--`. Executed directly
        /// (no shell).
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// List slots in every worktree.
    Ls {
        /// Print machine-readable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Show one slot.
    Show {
        /// Slot name.
        name: String,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Run deprovision, release the ports, purge the cache directories when
    /// configured, and remove the record.
    Down {
        /// Slot name.
        name: String,
        /// Clean up even when deprovision fails.
        #[arg(long)]
        force: bool,
        /// Print machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Remove slots whose worktree no longer exists.
    Prune {
        /// Print machine-readable JSON instead of a human-readable summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HookHarness {
    /// Run as a Claude Code `PreToolUse` hook: reads the hook payload JSON
    /// from stdin, evaluates any Bash command against the lease registry,
    /// and writes the hook response JSON to stdout. By default, portzilla-side
    /// errors fail open; `PORTZILLA_FAIL_CLOSED=1` enables fail-closed behavior
    /// using Claude Code's hook contract.
    ClaudeCode,
    /// Run as a Cursor `beforeShellExecution` hook: reads the hook payload
    /// JSON from stdin, evaluates the command against the lease registry,
    /// and writes the hook response JSON to stdout. By default, portzilla-side
    /// errors fail open; `PORTZILLA_FAIL_CLOSED=1` enables fail-closed behavior
    /// using Cursor's hook contract.
    Cursor,
    /// Run as a Gemini CLI `BeforeTool` hook (scoped to the
    /// `run_shell_command` tool): reads the hook payload JSON from stdin
    /// and writes the hook response JSON to stdout. By default, portzilla-side
    /// errors fail open; `PORTZILLA_FAIL_CLOSED=1` enables fail-closed behavior
    /// using Gemini CLI's hook contract.
    Gemini,
    /// Run as a Codex CLI `PreToolUse` hook (scoped to the `Bash` tool):
    /// reads the hook payload JSON from stdin, evaluates the command
    /// against the lease registry, and writes the hook response JSON to
    /// stdout. By default, portzilla-side errors fail open;
    /// `PORTZILLA_FAIL_CLOSED=1` enables fail-closed behavior using Codex's
    /// hook contract.
    Codex,
    /// Run as a Kimi CLI `PreToolUse` hook (scoped to the `Shell` tool):
    /// reads the hook payload JSON from stdin, evaluates the command
    /// against the lease registry, and signals the verdict through Kimi's
    /// exit-code contract (exit 2 + stderr blocks, exit 0 allows). By default,
    /// portzilla-side errors fail open; `PORTZILLA_FAIL_CLOSED=1` enables
    /// fail-closed behavior using Kimi's exit-code contract.
    Kimi,
    /// Run as a Windsurf (Cascade) `pre_run_command` hook: reads the hook
    /// payload JSON from stdin, evaluates `tool_info.command_line` against
    /// the lease registry, and signals the verdict through Windsurf's
    /// exit-code contract (exit 2 + stderr blocks, exit 0 allows). By default,
    /// portzilla-side errors fail open; `PORTZILLA_FAIL_CLOSED=1` enables
    /// fail-closed behavior using Windsurf's exit-code contract.
    #[command(name = "windsurf")]
    Windsurf,
    /// Run the binary side of OpenCode's kill-guard: called by the
    /// `portzilla.js` plugin shim (printed by `portzilla init opencode`),
    /// reads the shim's verdict-request JSON from stdin, evaluates the
    /// command against the lease registry, and writes the verdict JSON to
    /// stdout. Always exits 0 — the verdict is the JSON, never the exit
    /// code. By default, portzilla-side errors produce an allow verdict;
    /// `PORTZILLA_FAIL_CLOSED=1` produces a deny verdict using OpenCode's
    /// action JSON protocol.
    #[command(name = "opencode")]
    OpenCode,
}

#[derive(Subcommand)]
enum InitHarness {
    /// Print the settings.json snippet that registers portzilla's
    /// kill-guard as a Claude Code `PreToolUse` hook on the `Bash` tool.
    ClaudeCode,
    /// Print the hooks.json snippet that registers portzilla's kill-guard
    /// as a Cursor `beforeShellExecution` hook.
    Cursor,
    /// Print the settings.json snippet that registers portzilla's
    /// kill-guard as a Gemini CLI `BeforeTool` hook on `run_shell_command`.
    Gemini,
    /// Print the hooks.json snippet that registers portzilla's kill-guard
    /// as a Codex CLI `PreToolUse` hook on the `Bash` tool.
    Codex,
    /// Print the config.toml snippet that registers portzilla's kill-guard
    /// as a Kimi CLI `PreToolUse` hook on the `Shell` tool.
    Kimi,
    /// Print the hooks.json snippet that registers portzilla's kill-guard
    /// as a Windsurf (Cascade) `pre_run_command` hook (in `.windsurf/`).
    #[command(name = "windsurf")]
    Windsurf,
    /// Print the full source of the `portzilla.js` plugin shim for
    /// OpenCode (its `tool.execute.before` hook shells out to
    /// `portzilla hook opencode`), plus where to save it.
    #[command(name = "opencode")]
    OpenCode,
    /// Print the `portzilla` agent skill (`SKILL.md`) for running dev
    /// servers under `portzilla run`: stdout is the skill file verbatim.
    Skill,
}

/// Exit code for "the requested lease does not exist" — distinct from the
/// generic exit code 1 used for unexpected/internal errors (I/O failures,
/// corrupt state, lock failures, etc.). A missing lease is an expected,
/// well-defined outcome, not a tool failure. The `intent` commands reuse it
/// for a file with no live claim.
const EXIT_NOT_FOUND: i32 = 2;

/// Bounds for `portzilla intent claim --ttl-secs`. Zero would create a claim
/// that is expired the moment it is written, and 24 hours is the longest a
/// crashed session should be able to hold a file before a peer can take it.
const MIN_INTENT_TTL_SECS: u64 = 1;
const MAX_INTENT_TTL_SECS: u64 = 86_400;

fn main() {
    match run() {
        Ok(()) => {}
        Err(RunError::NotFound(port)) => {
            eprintln!("no lease found for port {port}");
            std::process::exit(EXIT_NOT_FOUND);
        }
        Err(RunError::IntentNotFound(file)) => {
            eprintln!("no live file intent found for {file}");
            std::process::exit(EXIT_NOT_FOUND);
        }
        Err(RunError::SlotNotFound { name, worktree }) => {
            eprintln!(
                "no slot named {} is up in {}",
                sanitize_for_display(&name),
                sanitize_for_display(&worktree)
            );
            std::process::exit(EXIT_NOT_FOUND);
        }
        Err(RunError::Message(message)) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
        Err(RunError::Other(err)) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
    }
}

/// Distinguishes "lease not found" (exit code 2) from every other error
/// (exit code 1), so callers/scripts can tell "nothing to show" apart from
/// an actual failure.
enum RunError {
    NotFound(u16),
    /// A file has no live intent. Carries the path as the user spelled it so
    /// the message names what they asked about, not an opaque store key.
    IntentNotFound(String),
    /// A slot name is not up in the current worktree. Exit code 2, like a
    /// missing lease: the request was well-formed, the slot just is not there.
    SlotNotFound {
        name: String,
        worktree: String,
    },
    /// A usage error whose text is already the complete message (a missing
    /// session, a nonsensical TTL). Printed without the `error:` prefix so it
    /// reads as the instruction it is.
    Message(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for RunError {
    fn from(err: anyhow::Error) -> Self {
        RunError::Other(err)
    }
}

impl From<slot::SlotError> for RunError {
    fn from(err: slot::SlotError) -> Self {
        match err {
            slot::SlotError::NotFound { name, worktree } => RunError::SlotNotFound {
                name,
                worktree: worktree.display().to_string(),
            },
            other => RunError::Other(other.into()),
        }
    }
}

impl From<intent::IntentError> for RunError {
    fn from(err: intent::IntentError) -> Self {
        // A held-by-another or not-the-owner refusal carries the holder's
        // expiry. Say how long the block lasts instead of making the user
        // subtract timestamps.
        let Some(expires_at) = err.expires_at() else {
            return RunError::Other(err.into());
        };
        let remaining = expires_at.saturating_sub(lease::current_unix_timestamp());
        RunError::Other(anyhow::anyhow!("{err} (expires in {remaining}s)"))
    }
}

fn run() -> Result<(), RunError> {
    let cli = Cli::parse();

    match cli.command {
        // `serve` spins up its own async runtime and opens its own `Store`
        // internally (see `mcp::run_stdio`).
        Commands::Serve { mcp } => {
            if !mcp {
                return Err(RunError::Other(anyhow::anyhow!(
                    "serve requires a mode flag; currently only --mcp is supported (e.g. `portzilla \
                     serve --mcp`)"
                )));
            }
            let runtime = tokio::runtime::Runtime::new()
                .context("failed to start the async runtime for the MCP server")?;
            runtime.block_on(mcp::run_stdio())?;
        }

        Commands::Watch { interval, json } => {
            // Validate the data directory before entering the retrying loop:
            // a startup failure is a normal command error, not a cycle error.
            Store::open(None).context("failed to open store for watch")?;
            let runtime = tokio::runtime::Runtime::new()
                .context("failed to start the async runtime for the watch command")?;
            let result = runtime.block_on(watch::run_loop(None, interval, json));
            // A running spawn_blocking closure cannot be aborted. Do not wait
            // indefinitely for it after Ctrl-C; the process owns its lifetime.
            runtime.shutdown_timeout(std::time::Duration::ZERO);
            result?;
        }

        // Pure output, no store needed.
        Commands::Init { harness } => match harness {
            InitHarness::ClaudeCode => print_init_claude_code(),
            InitHarness::Cursor => print_init_cursor(),
            InitHarness::Gemini => print_init_gemini(),
            InitHarness::Codex => print_init_codex(),
            InitHarness::Kimi => print_init_kimi(),
            InitHarness::OpenCode => print_init_opencode(),
            InitHarness::Skill => print_init_skill(),
            InitHarness::Windsurf => print_init_windsurf(),
        },

        // Each hook handler manages its own store access with fail-open
        // semantics throughout — none of them may propagate an error via
        // `?`, since that would turn into a nonzero exit for what should
        // always be a silent, non-blocking hook response.
        Commands::Hook { harness } => match harness {
            HookHarness::ClaudeCode => run_hook_claude_code(),
            HookHarness::Cursor => run_hook_cursor(),
            HookHarness::Gemini => run_hook_gemini(),
            HookHarness::Codex => run_hook_codex(),
            HookHarness::Kimi => run_hook_kimi(),
            HookHarness::OpenCode => run_hook_opencode(),
            HookHarness::Windsurf => run_hook_windsurf(),
        },

        // `guard` either execs the given command (never returns on unix
        // success) or exits directly (deny, or a non-unix execution) — it
        // does not fall through to `Ok(())` below in the success path.
        Commands::Guard { session, command } => run_guard_cmd(session, command),

        // `run` spawns the child, transfers the lease to it, and waits for
        // it. A nonzero child status exits the process directly (see
        // `run_portzilla_run`); only a zero child status falls through to
        // `Ok(())` below.
        Commands::Run {
            port,
            tag,
            session,
            command,
        } => run_portzilla_run(port, tag, session, command)?,

        Commands::Claim {
            port,
            tag,
            pid,
            session,
            json,
        } => {
            let store = Store::open(None)?;
            let pid = pid.unwrap_or_else(default_pid);
            let actor_session = resolve_cli_actor_session(session.as_deref());
            let outcome = store.claim_with_actor(
                port,
                pid,
                tag,
                session,
                AuditActor::new(AuditSource::Cli, None, actor_session),
                &SystemPidChecker,
            )?;
            print_claim_outcome(&outcome, port, json);
        }
        Commands::Ls { json } => {
            let store = Store::open(None)?;
            let leases = store.list()?;
            print_leases(&leases, json);
        }
        Commands::Who { port, json } => {
            let store = Store::open(None)?;
            match store.get(port)? {
                Some(lease) => print_lease_view(&to_view(&lease, &SystemPidChecker), json),
                None => return Err(RunError::NotFound(port)),
            }
        }
        Commands::Release { port, json } => {
            let store = Store::open(None)?;
            match store.release(
                port,
                AuditActor::new(AuditSource::Cli, None, resolve_cli_actor_session(None)),
                &SystemPidChecker,
            )? {
                Some(outcome) => {
                    if outcome.lease.durable {
                        // A durable lease has no owning process, so the usual
                        // "owning pid is still alive" warning would be a lie.
                        eprintln!(
                            "released the durable slot reservation on port {} (tag: {})",
                            outcome.lease.port,
                            sanitize_for_display(&outcome.lease.tag)
                        );
                    } else if outcome.was_alive {
                        eprintln!(
                            "warning: released port {} whose owning pid {} is still alive",
                            outcome.lease.port, outcome.lease.pid
                        );
                    }
                    print_lease_view(&to_view(&outcome.lease, &SystemPidChecker), json);
                }
                None => return Err(RunError::NotFound(port)),
            }
        }
        Commands::Prune { json } => {
            let store = Store::open(None)?;
            let pruned = store.prune(
                AuditActor::new(AuditSource::Cli, None, resolve_cli_actor_session(None)),
                &SystemPidChecker,
            )?;
            print_pruned(&pruned, json);
        }
        Commands::History {
            action,
            session,
            port,
            event,
            before,
            limit,
            json,
        } => {
            let store = Store::open(None)?;
            if matches!(action, Some(HistoryAction::Clear)) {
                let removed = store.clear_history(AuditActor::new(
                    AuditSource::Cli,
                    None,
                    resolve_cli_actor_session(None),
                ))?;
                println!("cleared {removed} history events");
            } else {
                let query = HistoryQuery::try_new(
                    session,
                    port,
                    event,
                    before,
                    limit.unwrap_or(audit::DEFAULT_HISTORY_LIMIT),
                )?;
                let page = store.history(&query)?;
                print_history(&page, json);
            }
        }
        Commands::Intent { command } => run_intent_command(command)?,
        Commands::Slot { command } => run_slot_command(command)?,
    }
    Ok(())
}

/// Runs one `portzilla intent` subcommand.
///
/// `claim` and `release` need an attributable session; `who` and `ls` do not.
/// A path is resolved once into the store's opaque key, and `claim` also
/// records the located working copy so peers can run the conflict check
/// against the claim the CLI created.
fn run_intent_command(command: IntentCommands) -> Result<(), RunError> {
    let store = IntentStore::open(None)?;
    let now = lease::current_unix_timestamp();
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;

    match command {
        IntentCommands::Claim {
            path,
            ttl_secs,
            session,
            json,
        } => {
            let session = intent_session("claim", session.as_deref())?;
            let ttl = intent_ttl_secs(ttl_secs)?;
            let (key, location) = locate_file(&cwd, &path);
            let intent = store.claim(&key, &session, now, now + ttl, Some(location))?;
            print_intent_claim(&intent, ttl, json);
        }
        IntentCommands::Who { path, json } => match store.get(&key_for_file(&cwd, &path))? {
            Some(intent) if intent.is_live_at(now) => print_intent_who(&intent, now, json),
            _ => {
                return Err(RunError::IntentNotFound(sanitize_for_display(
                    &path.display().to_string(),
                )));
            }
        },
        IntentCommands::Ls { json } => {
            let mut intents = store.list(now)?;
            // Stable, script-friendly order independent of the store's
            // insertion order.
            intents.sort_by(|left, right| left.key.cmp(&right.key));
            print_intents(&intents, now, json);
        }
        IntentCommands::Release {
            path,
            session,
            json,
        } => {
            let session = intent_session("release", session.as_deref())?;
            let (key, location) = locate_file(&cwd, &path);
            match store.release(&key, &session)? {
                Some(intent) => print_intent_release(&intent, now, json),
                None => print_intent_absent_release(&location.file, &key, json),
            }
        }
    }
    Ok(())
}

/// Resolves the session a command must attribute its write to, or fails with
/// the exact instruction the user needs. `--session` wins; the environment
/// fallbacks are the same ones the lease commands use.
fn intent_session(command: &str, explicit: Option<&str>) -> Result<String, RunError> {
    resolve_cli_actor_session(explicit).ok_or_else(|| {
        RunError::Message(format!(
            "intent {command} needs a session: pass --session or set PORTZILLA_SESSION"
        ))
    })
}

/// Validates `--ttl-secs`, defaulting to the adapter's 30 minutes. Zero and
/// anything above 24 hours are refused before the store is touched.
fn intent_ttl_secs(requested: Option<u64>) -> Result<u64, RunError> {
    let ttl = requested.unwrap_or(intent::DEFAULT_TTL_SECS);
    if !(MIN_INTENT_TTL_SECS..=MAX_INTENT_TTL_SECS).contains(&ttl) {
        return Err(RunError::Message(format!(
            "--ttl-secs must be between {MIN_INTENT_TTL_SECS} and {MAX_INTENT_TTL_SECS} seconds, \
             got {ttl}"
        )));
    }
    Ok(ttl)
}

/// The best human label for an intent: the holder's working copy when the
/// record has one, otherwise the opaque key (which is all a v1 record stores).
fn intent_file_label(intent: &Intent) -> String {
    intent
        .location
        .as_ref()
        .map(|location| location.file.to_string_lossy().into_owned())
        .unwrap_or_else(|| intent.key.clone())
}

fn print_intents(intents: &[Intent], now: u64, json: bool) {
    if json {
        let views: Vec<IntentView> = intents
            .iter()
            .map(|intent| to_intent_view(intent, now))
            .collect();
        println!(
            "{}",
            serde_json::to_string(&views).expect("IntentView always serializes")
        );
        return;
    }
    if intents.is_empty() {
        println!("no live file intents");
        return;
    }
    println!("KEY SESSION AGE EXPIRES");
    for intent in intents {
        let expires_in = intent.expires_at.saturating_sub(now);
        println!(
            "{} {} {}s {}s",
            sanitize_for_display(&intent.key),
            sanitize_for_display(&intent.session),
            now.saturating_sub(intent.created_at),
            expires_in
        );
    }
}

fn print_intent_who(intent: &Intent, now: u64, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(&to_intent_view(intent, now))
                .expect("IntentView always serializes")
        );
        return;
    }
    println!(
        "{} is held by session {} (age {}s, expires in {}s)",
        sanitize_for_display(&intent_file_label(intent)),
        sanitize_for_display(&intent.session),
        now.saturating_sub(intent.created_at),
        intent.expires_at.saturating_sub(now)
    );
}

fn print_intent_claim(intent: &Intent, ttl: u64, json: bool) {
    if json {
        // The caller just wrote the claim, so `now` is before its expiry; use
        // the record itself to keep the view honest if the clock ticked.
        let now = lease::current_unix_timestamp();
        println!(
            "{}",
            serde_json::to_string(&to_intent_view(intent, now))
                .expect("IntentView always serializes")
        );
        return;
    }
    println!(
        "claimed {} for session {} (expires in {ttl}s)",
        sanitize_for_display(&intent_file_label(intent)),
        sanitize_for_display(&intent.session)
    );
}

fn print_intent_release(intent: &Intent, now: u64, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(&to_intent_view(intent, now))
                .expect("IntentView always serializes")
        );
        return;
    }
    println!(
        "released {} for session {}",
        sanitize_for_display(&intent_file_label(intent)),
        sanitize_for_display(&intent.session)
    );
}

/// Release is idempotent: a claim that is already absent is success. The JSON
/// form says so explicitly instead of inventing timestamps for a record that
/// never existed.
fn print_intent_absent_release(file: &std::path::Path, key: &str, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "released": false,
                "key": key,
                "file": file.to_string_lossy(),
            })
        );
        return;
    }
    println!(
        "no file intent for {} to release (already absent)",
        sanitize_for_display(&file.display().to_string())
    );
}

/// Runs one `portzilla slot` subcommand.
///
/// Slots are resolved, changed, and printed one command per arm so the
/// borrowed stores stay alive for the whole operation. `env` and `run` read
/// the stored record rather than the config: ports, database names, and cache
/// paths were decided at `up` time, and re-rendering them from a changed
/// config would describe a slot that is not the one on disk. `--config` only
/// reloads `[slot.env]` values.
fn run_slot_command(command: SlotCommands) -> Result<(), RunError> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;

    match command {
        SlotCommands::Up {
            name,
            config,
            replace,
            json,
        } => {
            let context = SlotContext::resolve(&cwd, config.as_deref())?;
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let outcome = runtime.up(&context, &name, replace)?;
            print_slot_warnings(&outcome.warnings);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&SlotUpView {
                        slot: SlotView::from_record(&outcome.record),
                        replaced: outcome.replaced,
                    })
                    .expect("SlotUpView always serializes")
                );
            } else {
                println!(
                    "slot {} is up in {}{}",
                    sanitize_for_display(&outcome.record.name),
                    sanitize_for_display(&outcome.record.worktree.display().to_string()),
                    if outcome.replaced { " (replaced)" } else { "" }
                );
                print_slot_detail(&outcome.record);
            }
        }
        SlotCommands::Env { name, config } => {
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let record = runtime.find(&cwd, &name)?;
            let mut env = record.env.clone();
            if let Some(path) = config.as_deref() {
                let config = SlotConfig::load(&record.worktree, Some(path))?;
                env.extend(config.env);
            }
            print!("{}", slot_runtime::render_env_file(&env));
        }
        SlotCommands::Run {
            name,
            config,
            command,
        } => {
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let record = runtime.find(&cwd, &name)?;
            let mut env = record.env.clone();
            if let Some(path) = config.as_deref() {
                let config = SlotConfig::load(&record.worktree, Some(path))?;
                env.extend(config.env);
            }
            let Some((program, arguments)) = command.split_first() else {
                return Err(RunError::Message(
                    "slot run requires a command to execute".to_string(),
                ));
            };
            let status = std::process::Command::new(program)
                .args(arguments)
                .envs(&env)
                .status()
                .with_context(|| format!("failed to spawn {program}"))?;
            match status.code() {
                Some(0) => {}
                _ => exit_with_child_status(status),
            }
        }
        SlotCommands::Ls { json } => {
            let slots = SlotStore::open(None)?;
            let mut records = slots.list()?;
            records.sort_by(|left, right| {
                left.worktree
                    .cmp(&right.worktree)
                    .then_with(|| left.name.cmp(&right.name))
            });
            print_slot_list(&records, json);
        }
        SlotCommands::Show { name, json } => {
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let record = runtime.find(&cwd, &name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&SlotView::from_record(&record))
                        .expect("SlotView always serializes")
                );
            } else {
                println!("name: {}", sanitize_for_display(&record.name));
                println!(
                    "worktree: {}",
                    sanitize_for_display(&record.worktree.display().to_string())
                );
                println!("repo: {}", sanitize_for_display(&record.repo_key));
                print_slot_detail(&record);
            }
        }
        SlotCommands::Down { name, force, json } => {
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let record = runtime.find(&cwd, &name)?;
            let outcome = runtime.down(&record.worktree, &name, force)?;
            print_slot_warnings(&outcome.warnings);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&SlotDownView {
                        slot: SlotView::from_record(&outcome.record),
                        released_ports: outcome.released_ports.clone(),
                        purged_dirs: outcome
                            .purged_dirs
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect(),
                        warnings: outcome.warnings.clone(),
                        forced: outcome.forced,
                    })
                    .expect("SlotDownView always serializes")
                );
            } else {
                print_slot_down(&outcome);
            }
        }
        SlotCommands::Prune { json } => {
            let leases = Store::open(None)?;
            let slots = SlotStore::open(None)?;
            let runtime = SlotRuntime::new(&leases, &slots, &SystemPidChecker);
            let outcome = runtime.prune()?;
            print_slot_warnings(&outcome.warnings);
            if json {
                let views: Vec<SlotView> =
                    outcome.removed.iter().map(SlotView::from_record).collect();
                println!(
                    "{}",
                    serde_json::to_string(&views).expect("SlotView always serializes")
                );
            } else if outcome.removed.is_empty() {
                println!("no slots to prune");
            } else {
                for record in &outcome.removed {
                    println!(
                        "pruned slot {} in {}",
                        sanitize_for_display(&record.name),
                        sanitize_for_display(&record.worktree.display().to_string())
                    );
                }
            }
        }
    }
    Ok(())
}

fn print_slot_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
}

/// JSON shape of a slot, shared by `up`, `ls`, `show`, `down`, and `prune`.
#[derive(Serialize)]
struct SlotView {
    name: String,
    worktree: String,
    repo_key: String,
    ports: BTreeMap<String, u16>,
    db: Option<SlotDbView>,
    cache_dirs: Vec<String>,
    env_file: String,
    created_at: u64,
    age_secs: u64,
}

#[derive(Serialize)]
struct SlotDbView {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
}

#[derive(Serialize)]
struct SlotUpView {
    #[serde(flatten)]
    slot: SlotView,
    replaced: bool,
}

#[derive(Serialize)]
struct SlotDownView {
    #[serde(flatten)]
    slot: SlotView,
    released_ports: Vec<u16>,
    purged_dirs: Vec<String>,
    warnings: Vec<String>,
    forced: bool,
}

impl SlotView {
    fn from_record(record: &SlotRecord) -> Self {
        Self {
            name: record.name.clone(),
            worktree: record.worktree.to_string_lossy().into_owned(),
            repo_key: record.repo_key.clone(),
            ports: record.ports.clone(),
            db: record.db.as_ref().map(|db| SlotDbView {
                name: db.name.clone(),
                schema: db.schema.clone(),
            }),
            cache_dirs: record
                .cache_dirs
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            env_file: record.env_file.to_string_lossy().into_owned(),
            created_at: record.created_at,
            age_secs: lease::current_unix_timestamp().saturating_sub(record.created_at),
        }
    }
}

fn slot_ports_label(ports: &BTreeMap<String, u16>) -> String {
    ports
        .iter()
        .map(|(name, port)| format!("{name}={port}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn slot_db_label(record: &SlotRecord) -> String {
    match &record.db {
        None => "-".to_string(),
        Some(db) => match &db.schema {
            Some(schema) => format!("{} ({schema})", db.name),
            None => db.name.clone(),
        },
    }
}

fn print_slot_detail(record: &SlotRecord) {
    println!("ports: {}", slot_ports_label(&record.ports));
    println!("db: {}", sanitize_for_display(&slot_db_label(record)));
    for dir in &record.cache_dirs {
        println!(
            "cache: {}",
            sanitize_for_display(&dir.display().to_string())
        );
    }
    println!(
        "env file: {}",
        sanitize_for_display(&record.env_file.display().to_string())
    );
    println!(
        "age: {}s",
        lease::current_unix_timestamp().saturating_sub(record.created_at)
    );
}

fn print_slot_list(records: &[SlotRecord], json: bool) {
    if json {
        let views: Vec<SlotView> = records.iter().map(SlotView::from_record).collect();
        println!(
            "{}",
            serde_json::to_string(&views).expect("SlotView always serializes")
        );
        return;
    }
    if records.is_empty() {
        println!("no slots");
        return;
    }
    println!(
        "{:<16} {:<40} {:<24} {:<20} AGE",
        "NAME", "WORKTREE", "PORTS", "DB"
    );
    for record in records {
        println!(
            "{:<16} {:<40} {:<24} {:<20} {}s",
            sanitize_for_display(&record.name),
            sanitize_for_display(&record.worktree.display().to_string()),
            slot_ports_label(&record.ports),
            sanitize_for_display(&slot_db_label(record)),
            lease::current_unix_timestamp().saturating_sub(record.created_at)
        );
    }
}

fn print_slot_down(outcome: &slot_runtime::DownOutcome) {
    let mut details = Vec::new();
    if !outcome.released_ports.is_empty() {
        details.push(format!(
            "released ports {}",
            outcome
                .released_ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !outcome.purged_dirs.is_empty() {
        details.push(format!(
            "purged {} cache director{}",
            outcome.purged_dirs.len(),
            if outcome.purged_dirs.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join("; "))
    };
    println!(
        "slot {} is down{suffix}",
        sanitize_for_display(&outcome.record.name)
    );
}

/// Runs the Claude Code hook (`PreToolUse` or `PostToolUse`): reads the hook
/// payload from stdin, evaluates it, and writes the response to
/// stdout/stderr.
///
/// This function is the fail-open boundary described in `claude_code.rs`'s
/// module docs, made concrete: reading stdin, opening the stores, listing
/// leases, and even a panic inside the guard/adapter logic are all caught
/// here and turned into "print nothing, note it on stderr" rather than ever
/// propagating an error out of `run()` — a nonzero exit here would risk
/// alarming or blocking the user over a portzilla-side problem that has
/// nothing to do with whether their command is actually safe to run.
fn run_hook_claude_code() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                println!(
                    "{}",
                    claude_code::fail_closed_response(&fail_closed_reason(&cause))
                );
                return;
            }
            eprintln!("portzilla hook claude-code: {cause}, failing open (allow)");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                println!(
                    "{}",
                    claude_code::fail_closed_response(&fail_closed_reason(&format!("{err:#}")))
                );
                return;
            }
            eprintln!(
                "portzilla hook claude-code: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    // The file-intent store lives next to the lease store. A `PreToolUse` for
    // `Write`/`Edit` needs it; if it cannot be opened, the adapter applies the
    // same fail-open/fail-closed policy as any other portzilla-side failure,
    // and `PostToolUse` stays silent because it may never deny. The detailed
    // cause goes to stderr, which Claude Code only writes to its debug log on
    // exit 0.
    let intents = match IntentStore::open(None) {
        Ok(store) => Some(store),
        Err(err) => {
            eprintln!("portzilla hook claude-code: failed to open the file-intent store: {err:#}");
            None
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        claude_code::handle_with_policy(
            &raw_input,
            &leases,
            &SystemPidChecker,
            intents.as_ref(),
            fail_closed,
        )
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::ClaudeCode);
            if let Some(json) = outcome.stdout_json {
                println!("{json}");
            }
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
        }
        Err(_) => {
            if fail_closed {
                println!(
                    "{}",
                    claude_code::fail_closed_response(&fail_closed_reason(
                        "internal error while evaluating the command",
                    ))
                );
                return;
            }
            eprintln!(
                "portzilla hook claude-code: internal error while evaluating the command, failing \
                 open (allow)"
            );
        }
    }
}

/// Runs the Cursor `beforeShellExecution` hook. Same fail-open boundary as
/// [`run_hook_claude_code`], adapted to Cursor's shape: it always prints an
/// explicit JSON response (Cursor's own reference examples never rely on
/// "empty stdout means allow" the way Claude Code's do).
fn run_hook_cursor() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                println!(
                    "{}",
                    cursor::fail_closed_response(&fail_closed_reason(&cause))
                );
                return;
            }
            eprintln!("portzilla hook cursor: {cause}, failing open (allow)");
            println!("{{\"permission\":\"allow\"}}");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                println!(
                    "{}",
                    cursor::fail_closed_response(&fail_closed_reason(&format!("{err:#}")))
                );
                return;
            }
            eprintln!(
                "portzilla hook cursor: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cursor::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::Cursor);
            println!("{}", outcome.stdout_json);
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
        }
        Err(_) => {
            if fail_closed {
                println!(
                    "{}",
                    cursor::fail_closed_response(&fail_closed_reason(
                        "internal error while evaluating the command",
                    ))
                );
                return;
            }
            eprintln!(
                "portzilla hook cursor: internal error while evaluating the command, failing open (allow)"
            );
            println!("{{\"permission\":\"allow\"}}");
        }
    }
}

/// Runs the Gemini CLI `BeforeTool` hook. Same fail-open boundary as
/// [`run_hook_claude_code`].
fn run_hook_gemini() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                println!(
                    "{}",
                    gemini::fail_closed_response(&fail_closed_reason(&cause))
                );
                return;
            }
            eprintln!("portzilla hook gemini: {cause}, failing open (allow)");
            println!("{{}}");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                println!(
                    "{}",
                    gemini::fail_closed_response(&fail_closed_reason(&format!("{err:#}")))
                );
                return;
            }
            eprintln!(
                "portzilla hook gemini: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gemini::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::Gemini);
            println!("{}", outcome.stdout_json);
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
        }
        Err(_) => {
            if fail_closed {
                println!(
                    "{}",
                    gemini::fail_closed_response(&fail_closed_reason(
                        "internal error while evaluating the command",
                    ))
                );
                return;
            }
            eprintln!(
                "portzilla hook gemini: internal error while evaluating the command, failing open (allow)"
            );
            println!("{{}}");
        }
    }
}

/// Runs the Codex CLI `PreToolUse` hook. Same fail-open boundary as
/// [`run_hook_claude_code`]: Codex treats exit 0 with empty stdout as
/// "continue" (allow), and also honors "exit 2 + stderr" as a deny — so a
/// nonzero exit here must never happen over a portzilla-side problem.
fn run_hook_codex() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                println!(
                    "{}",
                    codex::fail_closed_response(&fail_closed_reason(&cause))
                );
                return;
            }
            eprintln!("portzilla hook codex: {cause}, failing open (allow)");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                println!(
                    "{}",
                    codex::fail_closed_response(&fail_closed_reason(&format!("{err:#}")))
                );
                return;
            }
            eprintln!(
                "portzilla hook codex: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        codex::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::Codex);
            if let Some(json) = outcome.stdout_json {
                println!("{json}");
            }
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
        }
        Err(_) => {
            if fail_closed {
                println!(
                    "{}",
                    codex::fail_closed_response(&fail_closed_reason(
                        "internal error while evaluating the command",
                    ))
                );
                return;
            }
            eprintln!(
                "portzilla hook codex: internal error while evaluating the command, failing \
                 open (allow)"
            );
        }
    }
}

/// Runs the Kimi CLI `PreToolUse` hook. Same fail-open boundary as the
/// other hook runners, but the verdict is signaled through Kimi's
/// exit-code contract instead of a stdout JSON response: exit 2 + stderr
/// blocks (and feeds the reason back to the model), exit 0 allows. The
/// exit code only ever comes from the adapter's own decision — a
/// portzilla-side failure always resolves to exit 0 — so this function is
/// the only hook runner whose success path can exit nonzero.
fn run_hook_kimi() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                eprintln!("{}", fail_closed_reason(&cause));
                std::process::exit(2);
            }
            eprintln!("portzilla hook kimi: {cause}, failing open (allow)");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                eprintln!("{}", fail_closed_reason(&format!("{err:#}")));
                std::process::exit(2);
            }
            eprintln!(
                "portzilla hook kimi: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kimi::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::Kimi);
            if let Some(text) = outcome.stdout_text {
                println!("{text}");
            }
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
            if outcome.exit_code != 0 {
                std::process::exit(outcome.exit_code);
            }
        }
        Err(_) => {
            if fail_closed {
                eprintln!(
                    "{}",
                    fail_closed_reason("internal error while evaluating the command")
                );
                std::process::exit(2);
            }
            eprintln!(
                "portzilla hook kimi: internal error while evaluating the command, failing \
                 open (allow)"
            );
        }
    }
}

/// Runs the Windsurf (Cascade) `pre_run_command` kill-guard. Same
/// exit-code-driven fail-open boundary as `run_hook_kimi`: exit 2 + stderr
/// is Windsurf's block signal, exit 0 allows, and any portzilla-side failure
/// resolves to exit 0 (Windsurf documents every exit code except 2 as
/// allow, which composes with portzilla's fail-open principle).
fn run_hook_windsurf() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                eprintln!("{}", fail_closed_reason(&cause));
                std::process::exit(2);
            }
            eprintln!("portzilla hook windsurf: {cause}, failing open (allow)");
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                eprintln!("{}", fail_closed_reason(&format!("{err:#}")));
                std::process::exit(2);
            }
            eprintln!(
                "portzilla hook windsurf: failed to read the lease store, failing open (allow): \
                 {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        windsurf::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::Windsurf);
            if let Some(text) = outcome.stdout_text {
                println!("{text}");
            }
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
            if outcome.exit_code != 0 {
                std::process::exit(outcome.exit_code);
            }
        }
        Err(_) => {
            if fail_closed {
                eprintln!(
                    "{}",
                    fail_closed_reason("internal error while evaluating the command")
                );
                std::process::exit(2);
            }
            eprintln!(
                "portzilla hook windsurf: internal error while evaluating the command, failing \
                 open (allow)"
            );
        }
    }
}

/// Runs the OpenCode kill-guard binary side, called by the `portzilla.js`
/// plugin shim. Same fail-open boundary as the other hook runners, with
/// one difference: stdout ALWAYS carries the verdict JSON (the shim parses
/// the verdict from stdout — silence must never mean anything), and the
/// exit code is always 0 (the shim only reads the JSON).
fn run_hook_opencode() {
    let fail_closed = fail_closed_mode();
    let raw_input = match read_hook_stdin(HOOK_STDIN_LIMIT) {
        Ok(input) => input,
        Err(cause) => {
            if fail_closed {
                println!(
                    "{}",
                    opencode::fail_closed_response(&fail_closed_reason(&cause))
                );
                return;
            }
            eprintln!("portzilla hook opencode: {cause}, failing open (allow)");
            println!(r#"{{"action":"allow"}}"#);
            return;
        }
    };

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            if fail_closed {
                println!(
                    "{}",
                    opencode::fail_closed_response(&fail_closed_reason(&format!("{err:#}")))
                );
                return;
            }
            eprintln!(
                "portzilla hook opencode: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        opencode::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
            persist_hook_evidence(outcome.audit, audit::AuditHarness::OpenCode);
            println!("{}", outcome.stdout_json);
            if let Some(note) = outcome.stderr_note {
                eprintln!("{note}");
            }
        }
        Err(_) => {
            if fail_closed {
                println!(
                    "{}",
                    opencode::fail_closed_response(&fail_closed_reason(
                        "internal error while evaluating the command",
                    ))
                );
                return;
            }
            eprintln!(
                "portzilla hook opencode: internal error while evaluating the command, failing \
                 open (allow)"
            );
            println!(r#"{{"action":"allow"}}"#);
        }
    }
}

/// Runs `portzilla guard --session <S> -- <command...>`: resolves session
/// identity, evaluates the joined command, and either denies (exit 2, no
/// execution), warns then executes, or executes silently. Never returns on
/// the execute paths — see [`execute`].
fn run_guard_cmd(session_flag: Option<String>, command: Vec<String>) {
    let fail_closed = fail_closed_mode();
    let self_session = session_flag
        .or_else(|| std::env::var("PORTZILLA_SESSION").ok())
        .filter(|s| !s.is_empty());
    let command_display = guard_cmd::join_command(&command);

    // Fail-open: a store problem is not a reason to block a command a
    // human or script explicitly asked to run. Under `PORTZILLA_FAIL_CLOSED`,
    // a store we can't read flips to deny instead.
    let (store, leases) = match Store::open(None) {
        Ok(store) => match store.list() {
            Ok(leases) => (Some(store), leases),
            Err(err) => {
                if fail_closed {
                    eprintln!(
                        "portzilla guard: blocked — could not verify lease safety \
                         and PORTZILLA_FAIL_CLOSED is set (failing closed): {err:#}"
                    );
                    std::process::exit(2);
                }
                eprintln!(
                    "portzilla guard: failed to read the lease store, failing open (execute): {err:#}"
                );
                (None, Vec::new())
            }
        },
        Err(err) => {
            if fail_closed {
                eprintln!(
                    "portzilla guard: blocked — could not verify lease safety \
                     and PORTZILLA_FAIL_CLOSED is set (failing closed): {err:#}"
                );
                std::process::exit(2);
            }
            eprintln!(
                "portzilla guard: failed to read the lease store, failing open (execute): {err:#}"
            );
            (None, Vec::new())
        }
    };

    let (action, evidence) = guard_cmd::decide_with_evidence(
        &command_display,
        &leases,
        None,
        self_session.as_deref(),
        &SystemPidChecker,
    );
    if let Some(evidence) = evidence {
        persist_guard_evidence(store.as_ref(), evidence, self_session.as_deref());
    }
    match action {
        guard_cmd::GuardAction::Deny { explanation } => {
            eprintln!("portzilla guard: blocked — {explanation}");
            // Exit code 2 here is `guard`'s own choice, unrelated to (and
            // only coincidentally the same value as) EXIT_NOT_FOUND used by
            // `who`/`release` — they are different subcommands with
            // independent exit-code contracts. Worth a second look if exit
            // codes are ever consolidated into one shared scheme.
            std::process::exit(2);
        }
        guard_cmd::GuardAction::WarnThenExecute { explanation } => {
            eprintln!("portzilla guard: warning — {explanation}");
            execute(&command);
        }
        guard_cmd::GuardAction::Execute => execute(&command),
    }
}

fn persist_guard_evidence(
    store: Option<&Store>,
    evidence: guard::GuardAuditDraft,
    actor_session: Option<&str>,
) {
    let Some(store) = store else { return };
    if let Err(err) = store.record_guard_evidence(
        AuditActor::new(
            AuditSource::Guard,
            Some(audit::AuditHarness::Generic),
            actor_session.map(str::to_owned),
        ),
        evidence.evidence,
    ) {
        eprintln!("portzilla guard: warning: failed to record guard event: {err:#}");
    }
}

fn persist_hook_evidence(evidence: Option<guard::GuardAuditDraft>, harness: audit::AuditHarness) {
    let Some(evidence) = evidence else { return };
    let actor = AuditActor::new(AuditSource::Guard, Some(harness), evidence.actor_session);
    match Store::open(None).and_then(|store| store.record_guard_evidence(actor, evidence.evidence))
    {
        Ok(()) => {}
        Err(err) => eprintln!("portzilla hook: warning: failed to record guard event: {err:#}"),
    }
}

fn resolve_cli_actor_session(explicit: Option<&str>) -> Option<String> {
    resolve_cli_actor_session_from(
        explicit,
        std::env::var("PORTZILLA_SESSION").ok().as_deref(),
        std::env::var("CLAUDE_CODE_SESSION_ID").ok().as_deref(),
    )
}

fn resolve_cli_actor_session_from(
    explicit: Option<&str>,
    portzilla_session: Option<&str>,
    claude_session: Option<&str>,
) -> Option<String> {
    explicit
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            portzilla_session
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| claude_session.filter(|v| !v.is_empty()).map(str::to_owned))
}

/// Maps a failure to start `program` to the POSIX-conventional exit code a
/// shell would use for the same failure: 127 when the program could not be
/// found at all, 126 for every other reason it couldn't be started (not
/// executable, permission denied, etc.).
fn exec_failure_exit_code(err: &std::io::Error) -> i32 {
    if err.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

/// Executes `args` (program + its own arguments) directly — no shell, no
/// reinterpretation. On Unix this replaces the current process image via
/// `exec`, so `portzilla guard` never has to itself track or propagate an
/// exit code: the exec'd process becomes the same PID and its exit code is
/// what the caller sees. `exec` only returns on failure. On non-Unix
/// platforms (no `exec`), spawns a child, waits for it, and exits with its
/// status code.
#[cfg(unix)]
fn execute(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let Some((program, rest)) = args.split_first() else {
        eprintln!("portzilla guard: no command given to execute");
        std::process::exit(1);
    };
    let err = std::process::Command::new(program).args(rest).exec();
    let code = exec_failure_exit_code(&err);
    eprintln!("portzilla guard: failed to execute {program}: {err}");
    std::process::exit(code);
}

#[cfg(not(unix))]
fn execute(args: &[String]) -> ! {
    let Some((program, rest)) = args.split_first() else {
        eprintln!("portzilla guard: no command given to execute");
        std::process::exit(1);
    };
    match std::process::Command::new(program).args(rest).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            let code = exec_failure_exit_code(&err);
            eprintln!("portzilla guard: failed to execute {program}: {err}");
            std::process::exit(code);
        }
    }
}

/// Runs `portzilla run <port> --tag <tag> [--session <s>] -- <command...>`:
/// claims the requested port for this wrapper process, spawns the command
/// directly (no shell) with `PORTZILLA_PORT` set to the actual (possibly
/// reassigned) port, transfers the live lease to the spawned child, and
/// waits for it, propagating its exit status.
///
/// A zero child status returns `Ok(())`; a nonzero (or signaled) child
/// exits this process with the same code (or 1 when there is no code to
/// propagate) so `run` is transparent in scripts. Error paths before the
/// wait return `Err` normally and never release the wrapper lease — it is
/// left for `prune` to reap once this short-lived wrapper exits.
fn run_portzilla_run(
    port: u16,
    tag: String,
    session: Option<String>,
    command: Vec<String>,
) -> Result<(), RunError> {
    // Normalize here as well as in `Store::claim` so the child environment
    // matches the stored lease: empty means absent, never `PORTZILLA_SESSION=""`.
    let session = session.filter(|s| !s.is_empty());
    let actor_session = resolve_cli_actor_session(session.as_deref());
    let store = Store::open(None)?;
    let wrapper_pid = std::process::id();
    let outcome = store.claim_with_actor(
        port,
        wrapper_pid,
        tag,
        session.clone(),
        AuditActor::new(AuditSource::Run, None, actor_session.clone()),
        &SystemPidChecker,
    )?;
    let claimed_lease = outcome.lease.clone();
    let run_actor = AuditActor::new(AuditSource::Run, None, actor_session);
    let assigned = outcome.lease.port;

    // The wrapper lease must carry a verified start time before anything is
    // spawned: without it the later transfer has no trustworthy wrapper
    // identity to match against.
    let wrapper_start_time = SystemPidChecker
        .process_start_time(wrapper_pid)
        .context("could not resolve the start time of the run wrapper process")?;
    if outcome.lease.process_start_time != Some(wrapper_start_time) {
        return Err(RunError::Other(anyhow::anyhow!(
            "run wrapper lease on port {assigned} has no verified process identity; refusing to spawn"
        )));
    }

    // Human progress only, stderr only: stdout belongs to the child.
    if outcome.reassigned {
        eprintln!("port {port} is busy; running on port {assigned} instead");
    } else {
        eprintln!("running on port {assigned}");
    }

    let Some((program, rest)) = command.split_first() else {
        return Err(RunError::Other(anyhow::anyhow!(
            "run requires a command to execute"
        )));
    };
    let mut child_cmd = std::process::Command::new(program);
    child_cmd.args(rest);
    child_cmd.env("PORTZILLA_PORT", assigned.to_string());
    if let Some(session) = session.as_deref() {
        child_cmd.env("PORTZILLA_SESSION", session);
    } else {
        child_cmd.env_remove("PORTZILLA_SESSION");
    }
    let mut child = child_cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    // Transfer BEFORE waiting, retrying while the child is still alive: a
    // just-spawned (or just-exited zombie) child is not always immediately
    // visible to the PID checker, so a single attempt races with exec/exit.
    // A child that already exited before the transfer ran already ran with
    // the right environment, so reap it and propagate its status instead of
    // failing the run. Only a child that stays alive yet unresolvable past
    // the deadline is a genuine failure: stop and reap it, then report —
    // never touching a lease we did not verify as ours.
    let child_pid = child.id();
    let transfer_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let transferred_lease = loop {
        match store.transfer(
            assigned,
            wrapper_pid,
            wrapper_start_time,
            child_pid,
            run_actor.clone(),
            &SystemPidChecker,
        ) {
            Ok(lease) => break lease,
            Err(err) => {
                if let Some(status) = child
                    .try_wait()
                    .context("failed to poll the run child after a failed lease transfer")?
                {
                    if let Err(record_err) = store.record_process_exit(
                        run_actor.clone(),
                        &claimed_lease,
                        process_exit_outcome(&status),
                    ) {
                        eprintln!(
                            "portzilla run: warning: failed to record process exit: {record_err:#}"
                        );
                    }
                    exit_with_child_status(status);
                }
                if std::time::Instant::now() >= transfer_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::Other(err.context(format!(
                        "failed to transfer the lease on port {assigned}"
                    ))));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    };

    let status = child.wait().context("failed to wait for the run child")?;
    if let Err(err) =
        store.record_process_exit(run_actor, &transferred_lease, process_exit_outcome(&status))
    {
        eprintln!("portzilla run: warning: failed to record process exit: {err:#}");
    }
    match status.code() {
        Some(0) => Ok(()),
        Some(_) => exit_with_child_status(status),
        None => exit_with_child_status(status),
    }
}

fn process_exit_outcome(status: &std::process::ExitStatus) -> ProcessExitOutcome {
    if let Some(code) = status.code() {
        return ProcessExitOutcome::Code(code);
    }
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(status) {
        return ProcessExitOutcome::Signal(signal);
    }
    ProcessExitOutcome::Unknown
}

/// Exits this process with the child's status: the same code when the child
/// exited normally, 1 when the child was signaled and there is no code to
/// propagate.
fn exit_with_child_status(status: std::process::ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1));
}

fn print_init_cursor() {
    eprintln!("Add the following to your Cursor hooks config (.cursor/hooks.json for a project,");
    eprintln!("or ~/.cursor/hooks.json for your user) to register portzilla's kill-guard as a");
    eprintln!("beforeShellExecution hook:");
    eprintln!();
    println!("{}", cursor::HOOKS_SNIPPET);
    eprintln!();
    eprintln!("If you already have hooks.json, merge the \"beforeShellExecution\" entry above");
    eprintln!("into your existing hooks array instead of overwriting the file.");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!(
        "Note: Cursor does not currently expose the conversation id to the shell commands it"
    );
    eprintln!(
        "runs, only to the hook payload itself, so claims made from a Cursor session cannot be"
    );
    eprintln!("tagged with a matching --session. This hook still protects against killing another");
    eprintln!("session's live process; it just can't yet recognize a lease as your own.");
}

fn print_init_gemini() {
    eprintln!("Add the following to your Gemini CLI settings (.gemini/settings.json for a");
    eprintln!("project, or ~/.gemini/settings.json for your user) to register portzilla's");
    eprintln!("kill-guard as a BeforeTool hook scoped to the shell tool:");
    eprintln!();
    println!("{}", gemini::SETTINGS_SNIPPET);
    eprintln!();
    eprintln!("If you already have a \"hooks\" key in settings.json, merge the \"BeforeTool\"");
    eprintln!("entry above into your existing hooks instead of overwriting the file.");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!(
        "Note: Gemini CLI's run_shell_command tool only sets GEMINI_CLI=1 in the commands it"
    );
    eprintln!(
        "runs, not a session id, so claims made from a Gemini CLI session cannot be tagged with"
    );
    eprintln!(
        "a matching --session. This hook still protects against killing another session's live"
    );
    eprintln!("process; it just can't yet recognize a lease as your own.");
}

fn print_init_opencode() {
    eprintln!("portzilla's guard for OpenCode is a plugin shim, because OpenCode hooks run");
    eprintln!("in-process as JavaScript/TypeScript plugin modules — it cannot run `portzilla`");
    eprintln!("directly as the hook the way Claude Code/Codex/Kimi can. Save the file below as:");
    eprintln!();
    eprintln!("  .opencode/plugin/portzilla.js        (project level)");
    eprintln!("  ~/.config/opencode/plugin/portzilla.js   (user level)");
    eprintln!();
    eprintln!(
        "Then QUIT AND RESTART opencode — plugins are loaded once at startup, not hot-reloaded."
    );
    eprintln!();
    println!("{}", opencode::PLUGIN_SNIPPET);
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the plugin's guard checks to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!("How it works:");
    eprintln!(
        "  - tool.execute.before: intercepts write/edit and shell calls, asks `portzilla hook"
    );
    eprintln!("    opencode` for a verdict, and throws on deny before anything runs — the reason");
    eprintln!("    reaches the model as a tool error.");
    eprintln!(
        "  - write/edit: the edited file is claimed as a file intent. When another live session"
    );
    eprintln!(
        "    holds it, the pending edit is checked: a real conflict is denied, while an edit that"
    );
    eprintln!("    merges cleanly is allowed with a warning naming the holder.");
    eprintln!(
        "  - shell: a command that would kill a process leased to another session is denied."
    );
    eprintln!(
        "  - warn verdicts from a file edit reach the model: the reason is appended to the tool"
    );
    eprintln!(
        "    result by a second hook, tool.execute.after. A shell warn (a kill by process name,"
    );
    eprintln!("    which cannot be resolved to a lease) goes to OpenCode's log instead.");
    eprintln!("    Neither warning blocks the call.");
    eprintln!();
    eprintln!(
        "Fail-open: if the plugin can't load, can't spawn portzilla, or hits the 5s timeout,"
    );
    eprintln!("the tool call is allowed — a portzilla problem never blocks your session.");
}

fn print_init_codex() {
    eprintln!("Add the following to your Codex hooks config (.codex/hooks.json for a project,");
    eprintln!("or ~/.codex/hooks.json for your user) to register portzilla's kill-guard as a");
    eprintln!("PreToolUse hook on the Bash tool:");
    eprintln!();
    println!("{}", codex::HOOKS_SNIPPET);
    eprintln!();
    eprintln!("If you already have hooks.json, merge the \"PreToolUse\" entry above into your");
    eprintln!("existing hooks array instead of overwriting the file.");
    eprintln!();
    eprintln!("Note: project-level hooks are only loaded after the project .codex/ layer is");
    eprintln!("trusted; Codex will ask you to review/trust new hooks (see its /hooks command).");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!(
        "Note: Codex does not currently expose the session id to the shell commands it runs,"
    );
    eprintln!(
        "only to the hook payload itself, so claims made from a Codex session cannot be tagged"
    );
    eprintln!(
        "with a matching --session. This hook still protects against killing another session's"
    );
    eprintln!("live process; it just can't yet recognize a lease as your own.");
}

fn print_init_kimi() {
    eprintln!("Add the following to your Kimi CLI config (~/.kimi/config.toml) to register");
    eprintln!("portzilla's kill-guard as a PreToolUse hook scoped to the Shell tool:");
    eprintln!();
    println!("{}", kimi::CONFIG_SNIPPET);
    eprintln!();
    eprintln!("If you already have a [[hooks]] array in config.toml, append the entry above to it");
    eprintln!("instead of overwriting the file. `timeout` is Kimi's hook timeout in seconds");
    eprintln!("(default 30); portzilla's own stdin cap is 1 MiB.");
    eprintln!();
    eprintln!("Note: only user-level registration (~/.kimi/config.toml) is verified; Kimi's");
    eprintln!("project-level hook configuration is not positively documented, so this snippet");
    eprintln!("targets the user-level file.");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!(
        "Note: Kimi CLI does not currently expose the session id to the shell commands it runs,"
    );
    eprintln!(
        "only to the hook payload itself, so claims made from a Kimi session cannot be tagged"
    );
    eprintln!(
        "with a matching --session. This hook still protects against killing another session's"
    );
    eprintln!("live process; it just can't yet recognize a lease as your own.");
    eprintln!();
    eprintln!(
        "Note: Kimi CLI's hooks system is Beta, and Kimi CLI is transitioning to Kimi Code CLI"
    );
    eprintln!("as its successor project — re-verify this integration when adopting the successor.");
}

fn print_init_windsurf() {
    eprintln!("Add the following to your `.windsurf/hooks.json` (workspace) to register");
    eprintln!("portzilla's kill-guard as a pre_run_command hook (blocks dangerous commands like");
    eprintln!("kills of other sessions' processes):");
    eprintln!();
    println!("{}", windsurf::CONFIG_SNIPPET);
    eprintln!();
    eprintln!("Alternatively, place the same file at ~/.codeium/windsurf/hooks.json (user level),");
    eprintln!("or at a system-level path (see the Cascade Hooks docs). If you already have a");
    eprintln!("hooks.json, merge the \"pre_run_command\" entry into the existing \"hooks\" object");
    eprintln!("instead of overwriting the file.");
    eprintln!();
    eprintln!("`show_output: true` prints portzilla's deny/warning text in the Cascade UI (a deny");
    eprintln!(
        "reaches the agent regardless via stderr; warnings are human-visible ONLY — Windsurf"
    );
    eprintln!("has no non-blocking channel the model sees, so a warn never blocks the command).");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
    eprintln!();
    eprintln!(
        "Note: Windsurf does not currently expose the trajectory id to the shell commands it"
    );
    eprintln!(
        "runs, only to the hook payload itself, so claims made from a Cascade session cannot"
    );
    eprintln!(
        "be tagged with a matching --session. This hook still protects against killing another"
    );
    eprintln!("session's live process; it just can't yet recognize a lease as your own.");
    eprintln!();
    eprintln!("Note: Cascade hooks do not load or run while a workspace is open in Restricted");
    eprintln!("Mode — the guard is absent there by design.");
}

/// Prints the `portzilla` agent skill verbatim: stdout is byte-for-byte
/// `skills/portzilla/SKILL.md`, embedded at compile time with
/// `include_str!` so there is no runtime file lookup. This enables
/// `mkdir -p .opencode/skills/portzilla && portzilla init skill >
/// .opencode/skills/portzilla/SKILL.md` to reproduce the file exactly —
/// hence `print!`, not `println!`: no extra trailing newline.
fn print_init_skill() {
    print!("{}", include_str!("../skills/portzilla/SKILL.md"));
}

fn print_init_claude_code() {
    eprintln!(
        "Add the following to your Claude Code settings (.claude/settings.json for a project,"
    );
    eprintln!("or ~/.claude/settings.json for your user) to register portzilla's kill-guard on");
    eprintln!("Bash and its file-intent guard on Write and Edit:");
    eprintln!();
    println!("{}", claude_code::SETTINGS_SNIPPET);
    eprintln!();
    eprintln!("If you already have a \"hooks\" key in settings.json, merge the \"PreToolUse\" and");
    eprintln!("\"PostToolUse\" entries above into your existing hooks instead of overwriting the");
    eprintln!("file.");
    eprintln!();
    eprintln!(
        "If you registered an earlier portzilla release, its \"PreToolUse\" matcher is Bash only."
    );
    eprintln!(
        "Replace that entry with the \"Bash|Write|Edit\" entry above and add the \"PostToolUse\""
    );
    eprintln!("entry. Without the update you keep the kill-guard but get no file-intent");
    eprintln!("protection.");
    eprintln!();
    eprintln!(
        "A clean edit is allowed without skipping the permission prompt: portzilla emits nothing,"
    );
    eprintln!(
        "so Claude Code's normal permission flow still runs. Only a proven conflict, or an edit"
    );
    eprintln!("portzilla cannot prove clean, is denied.");
    eprintln!();
    eprintln!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    eprintln!("Verify with: portzilla --version");
}

/// Resolves the default PID to attribute a claim to: the parent process
/// (the shell or agent invoking `portzilla`). Falls back to this process's
/// own PID if the parent cannot be determined.
fn default_pid() -> u32 {
    use sysinfo::{ProcessesToUpdate, get_current_pid};

    let Ok(current) = get_current_pid() else {
        return std::process::id();
    };
    let Some(mut system) = crate::lease::fresh_process_table() else {
        return std::process::id();
    };
    system.refresh_processes(ProcessesToUpdate::Some(&[current]), true);
    system
        .process(current)
        .and_then(|process| process.parent())
        .map(|pid| pid.as_u32())
        .unwrap_or_else(std::process::id)
}

/// Replaces control characters (C0 controls including ESC, C1 controls, and
/// DEL) with a single space before a string is written to human-readable
/// output. Tags and sessions are arbitrary user-supplied text: left
/// unsanitized, embedded newlines/carriage-returns/escape sequences let a
/// claim forge fake table rows or terminal control sequences in `ls`/`who`/
/// `release`/`prune` output. JSON output is unaffected (JSON already escapes
/// control characters) and is not passed through this function.
pub(crate) fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn print_history(page: &HistoryPage, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(page).expect("HistoryPage always serializes")
        );
        return;
    }
    if page.events.is_empty() {
        println!("no history events");
        return;
    }
    println!("SEQ EVENT SOURCE HARNESS ACTOR_SESSION LEASE_SESSION PORT PID SUMMARY");
    let now = lease::current_unix_timestamp();
    for event in &page.events {
        let snapshot = history_snapshot(&event.kind);
        let lease_session = snapshot
            .and_then(|lease| lease.session.as_deref())
            .map(sanitize_for_display)
            .unwrap_or_else(|| "(none)".into());
        let port =
            history_port(&event.kind, snapshot).map_or_else(|| "-".into(), |port| port.to_string());
        let pid = snapshot.map_or_else(|| "-".into(), |lease| lease.pid.to_string());
        let actor_session = event
            .actor
            .session
            .as_deref()
            .map(sanitize_for_display)
            .unwrap_or_else(|| "(none)".into());
        let source = serde_json::to_string(&event.actor.source)
            .unwrap_or_else(|_| "unknown".into())
            .trim_matches('"')
            .to_string();
        let harness = event
            .actor
            .harness
            .and_then(|value| serde_json::to_string(&value).ok())
            .map(|value| value.trim_matches('"').to_string())
            .unwrap_or_else(|| "-".into());
        let summary = serde_json::to_string(&event.kind)
            .unwrap_or_else(|_| "{}".into())
            .replace(['\n', '\r', '\t'], " ");
        println!(
            "{} {} {} {} {} {} {} {} age={}s {}",
            event.sequence,
            event.kind.event_type().as_str(),
            source,
            harness,
            actor_session,
            lease_session,
            port,
            pid,
            now.saturating_sub(event.occurred_at),
            summary
        );
    }
}

fn history_snapshot(kind: &AuditEventKind) -> Option<&LeaseSnapshot> {
    match kind {
        AuditEventKind::LeaseClaimed { lease, .. }
        | AuditEventKind::LeaseTransferred { lease, .. }
        | AuditEventKind::LeaseReleased { lease, .. }
        | AuditEventKind::LeasePruned { lease }
        | AuditEventKind::ProcessExited { lease, .. }
        | AuditEventKind::GuardDenied { lease, .. } => Some(lease),
        AuditEventKind::GuardWarned { .. } | AuditEventKind::HistoryCleared { .. } => None,
    }
}

fn history_port(kind: &AuditEventKind, snapshot: Option<&LeaseSnapshot>) -> Option<u16> {
    match kind {
        AuditEventKind::LeaseClaimed { .. } => snapshot.map(|lease| lease.port),
        AuditEventKind::GuardWarned { target, .. } => match target {
            audit::GuardTarget::Port { port } => Some(*port),
            _ => None,
        },
        _ => snapshot.map(|lease| lease.port),
    }
}

fn print_claim_outcome(outcome: &ClaimOutcome, requested_port: u16, json: bool) {
    if json {
        let view = to_claim_view(outcome, requested_port);
        println!(
            "{}",
            serde_json::to_string(&view).expect("ClaimView always serializes")
        );
    } else if outcome.reassigned {
        let cause = match outcome.reassignment_reason {
            Some(store::ReassignmentReason::LeaseConflict) => "lease conflict",
            Some(store::ReassignmentReason::OsOccupied) => "OS occupied",
            None => "port conflict",
        };
        println!(
            "port {requested_port} is busy ({cause}); claimed port {} instead for pid {} (tag: {})",
            outcome.lease.port,
            outcome.lease.pid,
            sanitize_for_display(&outcome.lease.tag)
        );
    } else {
        println!(
            "claimed port {} for pid {} (tag: {})",
            outcome.lease.port,
            outcome.lease.pid,
            sanitize_for_display(&outcome.lease.tag)
        );
    }
}

fn print_leases(leases: &[Lease], json: bool) {
    let views: Vec<LeaseView> = leases
        .iter()
        .map(|lease| to_view(lease, &SystemPidChecker))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string(&views).expect("LeaseView always serializes")
        );
        return;
    }

    println!(
        "{:<7} {:<8} {:<6} {:<10} TAG",
        "PORT", "PID", "STATUS", "AGE"
    );
    for view in &views {
        print_lease_row(view);
    }
}

fn print_lease_row(view: &LeaseView) {
    let status = if view.alive { "alive" } else { "dead" };
    println!(
        "{:<7} {:<8} {:<6} {:<10} {}",
        view.port,
        view.pid,
        status,
        format!("{}s", view.age_secs),
        sanitize_for_display(&view.tag)
    );
}

/// Prints a single lease, as used by `who` and `release`. Human mode shows
/// every field (including session, which the `ls` table omits for width);
/// JSON mode prints the same flat object shape as one `ls --json` entry.
fn print_lease_view(view: &LeaseView, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(view).expect("LeaseView always serializes")
        );
        return;
    }

    let status = if view.alive { "alive" } else { "dead" };
    let session = view
        .session
        .as_deref()
        .map(sanitize_for_display)
        .unwrap_or_else(|| "(none)".to_string());
    println!("port: {}", view.port);
    println!("pid: {}", view.pid);
    println!("tag: {}", sanitize_for_display(&view.tag));
    println!("session: {session}");
    println!("age: {}s", view.age_secs);
    println!("status: {status}");
}

fn print_pruned(pruned: &[Lease], json: bool) {
    let views: Vec<LeaseView> = pruned
        .iter()
        .map(|lease| to_view(lease, &SystemPidChecker))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string(&views).expect("LeaseView always serializes")
        );
        return;
    }

    if views.is_empty() {
        println!("no dead leases to prune");
        return;
    }
    for view in &views {
        println!(
            "pruned port {} (pid {}, tag: {})",
            view.port,
            view.pid,
            sanitize_for_display(&view.tag)
        );
    }
}

#[cfg(test)]
mod actor_tests {
    use super::resolve_cli_actor_session_from;

    #[test]
    fn process_exit_outcome_classifies_exit_code() {
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .unwrap();
        assert_eq!(
            super::process_exit_outcome(&status),
            super::ProcessExitOutcome::Code(7)
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_exit_outcome_classifies_unix_signal() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(
            super::process_exit_outcome(&status),
            super::ProcessExitOutcome::Signal(15)
        );
    }

    #[test]
    fn claim_actor_prefers_explicit_session_over_ambient() {
        assert_eq!(
            resolve_cli_actor_session_from(Some("explicit"), Some("portzilla"), Some("claude")),
            Some("explicit".to_string())
        );
    }

    #[test]
    fn claim_actor_uses_portzilla_session_without_changing_lease_session() {
        assert_eq!(
            resolve_cli_actor_session_from(None, Some("portzilla"), Some("claude")),
            Some("portzilla".to_string())
        );
    }

    #[test]
    fn claim_actor_falls_back_to_claude_code_session_id() {
        assert_eq!(
            resolve_cli_actor_session_from(None, None, Some("claude")),
            Some("claude".to_string())
        );
    }

    #[test]
    fn claim_actor_ignores_empty_portzilla_session_before_claude_fallback() {
        assert_eq!(
            resolve_cli_actor_session_from(None, Some(""), Some("claude")),
            Some("claude".to_string())
        );
    }

    #[test]
    fn claimed_history_rows_display_the_assigned_port() {
        let lease = super::LeaseSnapshot::from(&super::Lease::new(3001, 42, "tag", None));
        let kind = super::AuditEventKind::LeaseClaimed {
            requested_port: 3000,
            lease,
            disposition: super::audit::ClaimDisposition::ReassignedLeaseConflict,
            prior_lease: None,
            replaced_lease: None,
        };

        assert_eq!(
            super::history_port(&kind, super::history_snapshot(&kind)),
            Some(3001)
        );
    }
}
