mod claude_code;
mod codex;
mod cursor;
mod gemini;
mod guard;
mod guard_cmd;
mod hook_common;
mod kimi;
mod lease;
mod mcp;
mod opencode;
mod store;
mod view;
mod watch;
mod windsurf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lease::{Lease, PidChecker, SystemPidChecker};
use std::io::Read;
use store::{ClaimOutcome, Store};
use view::{LeaseView, to_claim_view, to_view};

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
/// well-defined outcome, not a tool failure.
const EXIT_NOT_FOUND: i32 = 2;

fn main() {
    match run() {
        Ok(()) => {}
        Err(RunError::NotFound(port)) => {
            eprintln!("no lease found for port {port}");
            std::process::exit(EXIT_NOT_FOUND);
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
    Other(anyhow::Error),
}

impl From<anyhow::Error> for RunError {
    fn from(err: anyhow::Error) -> Self {
        RunError::Other(err)
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
            let outcome = store.claim(port, pid, tag, session, &SystemPidChecker)?;
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
            match store.release(port, &SystemPidChecker)? {
                Some(outcome) => {
                    if outcome.was_alive {
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
            let pruned = store.prune(&SystemPidChecker)?;
            print_pruned(&pruned, json);
        }
    }
    Ok(())
}

/// Runs the Claude Code `PreToolUse` hook: reads the hook payload from
/// stdin, evaluates it, and writes the response to stdout/stderr.
///
/// This function is the fail-open boundary described in `claude_code.rs`'s
/// module docs, made concrete: reading stdin, opening the store, listing
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

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        claude_code::handle_with_policy(&raw_input, &leases, &SystemPidChecker, fail_closed)
    }));

    match result {
        Ok(outcome) => {
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
    let self_session = session_flag.or_else(|| std::env::var("PORTZILLA_SESSION").ok());
    let command_display = guard_cmd::join_command(&command);

    // Fail-open: a store problem is not a reason to block a command a
    // human or script explicitly asked to run. Under `PORTZILLA_FAIL_CLOSED`,
    // a store we can't read flips to deny instead.
    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
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
            Vec::new()
        }
    };

    let action = guard_cmd::decide(
        &command_display,
        &leases,
        None,
        self_session.as_deref(),
        &SystemPidChecker,
    );
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
    let store = Store::open(None)?;
    let wrapper_pid = std::process::id();
    let outcome = store.claim(port, wrapper_pid, tag, session.clone(), &SystemPidChecker)?;
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
    loop {
        match store.transfer(
            assigned,
            wrapper_pid,
            wrapper_start_time,
            child_pid,
            &SystemPidChecker,
        ) {
            Ok(_) => break,
            Err(err) => {
                if let Some(status) = child
                    .try_wait()
                    .context("failed to poll the run child after a failed lease transfer")?
                {
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
    }

    let status = child.wait().context("failed to wait for the run child")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(_) => exit_with_child_status(status),
        None => exit_with_child_status(status),
    }
}

/// Exits this process with the child's status: the same code when the child
/// exited normally, 1 when the child was signaled and there is no code to
/// propagate.
fn exit_with_child_status(status: std::process::ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1));
}

fn print_init_cursor() {
    println!("Add the following to your Cursor hooks config (.cursor/hooks.json for a project,");
    println!("or ~/.cursor/hooks.json for your user) to register portzilla's kill-guard as a");
    println!("beforeShellExecution hook:");
    println!();
    println!("{}", cursor::HOOKS_SNIPPET);
    println!();
    println!("If you already have hooks.json, merge the \"beforeShellExecution\" entry above");
    println!("into your existing hooks array instead of overwriting the file.");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!("Note: Cursor does not currently expose the conversation id to the shell commands it");
    println!(
        "runs, only to the hook payload itself, so claims made from a Cursor session cannot be"
    );
    println!("tagged with a matching --session. This hook still protects against killing another");
    println!("session's live process; it just can't yet recognize a lease as your own.");
}

fn print_init_gemini() {
    println!("Add the following to your Gemini CLI settings (.gemini/settings.json for a");
    println!("project, or ~/.gemini/settings.json for your user) to register portzilla's");
    println!("kill-guard as a BeforeTool hook scoped to the shell tool:");
    println!();
    println!("{}", gemini::SETTINGS_SNIPPET);
    println!();
    println!("If you already have a \"hooks\" key in settings.json, merge the \"BeforeTool\"");
    println!("entry above into your existing hooks instead of overwriting the file.");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!("Note: Gemini CLI's run_shell_command tool only sets GEMINI_CLI=1 in the commands it");
    println!(
        "runs, not a session id, so claims made from a Gemini CLI session cannot be tagged with"
    );
    println!(
        "a matching --session. This hook still protects against killing another session's live"
    );
    println!("process; it just can't yet recognize a lease as your own.");
}

fn print_init_opencode() {
    println!("portzilla's kill-guard for OpenCode is a plugin shim, because OpenCode hooks run");
    println!("in-process as JavaScript/TypeScript plugin modules — it cannot run `portzilla`");
    println!("directly as the hook the way Claude Code/Codex/Kimi can. Save the file below as:");
    println!();
    println!("  .opencode/plugin/portzilla.js        (project level)");
    println!("  ~/.config/opencode/plugin/portzilla.js   (user level)");
    println!();
    println!(
        "Then QUIT AND RESTART opencode — plugins are loaded once at startup, not hot-reloaded."
    );
    println!();
    println!("{}", opencode::PLUGIN_SNIPPET);
    println!();
    println!("portzilla must be on PATH as `portzilla` for the plugin's guard checks to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!("How it works:");
    println!(
        "  - tool.execute.before: intercepts bash calls, asks `portzilla hook opencode` for a"
    );
    println!("    verdict; a deny throws, and the reason reaches the model as a tool error.");
    println!(
        "  - tool.execute.after: Warn verdicts are appended to the tool result the model reads"
    );
    println!("    (OpenCode has no non-blocking model-visible warn channel before execution).");
    println!("  - shell.env: injects PORTZILLA_SESSION into every bash subprocess, so claims made");
    println!("    from OpenCode sessions can tag their leases and kill-guard recognizes them as");
    println!(
        "    your own — the only non-Claude harness where own-lease recognition works end to end."
    );
    println!(
        "    Claim like: portzilla claim 3000 --tag \"vite dev\" --session \"$PORTZILLA_SESSION\""
    );
    println!();
    println!("Fail-open: if the plugin can't load, can't spawn portzilla, or hits the 5s timeout,");
    println!("the command is allowed — a portzilla problem never blocks your session.");
}

fn print_init_codex() {
    println!("Add the following to your Codex hooks config (.codex/hooks.json for a project,");
    println!("or ~/.codex/hooks.json for your user) to register portzilla's kill-guard as a");
    println!("PreToolUse hook on the Bash tool:");
    println!();
    println!("{}", codex::HOOKS_SNIPPET);
    println!();
    println!("If you already have hooks.json, merge the \"PreToolUse\" entry above into your");
    println!("existing hooks array instead of overwriting the file.");
    println!();
    println!("Note: project-level hooks are only loaded after the project .codex/ layer is");
    println!("trusted; Codex will ask you to review/trust new hooks (see its /hooks command).");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!("Note: Codex does not currently expose the session id to the shell commands it runs,");
    println!(
        "only to the hook payload itself, so claims made from a Codex session cannot be tagged"
    );
    println!(
        "with a matching --session. This hook still protects against killing another session's"
    );
    println!("live process; it just can't yet recognize a lease as your own.");
}

fn print_init_kimi() {
    println!("Add the following to your Kimi CLI config (~/.kimi/config.toml) to register");
    println!("portzilla's kill-guard as a PreToolUse hook scoped to the Shell tool:");
    println!();
    println!("{}", kimi::CONFIG_SNIPPET);
    println!();
    println!("If you already have a [[hooks]] array in config.toml, append the entry above to it");
    println!("instead of overwriting the file. `timeout` is Kimi's hook timeout in seconds");
    println!("(default 30); portzilla's own stdin cap is 1 MiB.");
    println!();
    println!("Note: only user-level registration (~/.kimi/config.toml) is verified; Kimi's");
    println!("project-level hook configuration is not positively documented, so this snippet");
    println!("targets the user-level file.");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!(
        "Note: Kimi CLI does not currently expose the session id to the shell commands it runs,"
    );
    println!(
        "only to the hook payload itself, so claims made from a Kimi session cannot be tagged"
    );
    println!(
        "with a matching --session. This hook still protects against killing another session's"
    );
    println!("live process; it just can't yet recognize a lease as your own.");
    println!();
    println!(
        "Note: Kimi CLI's hooks system is Beta, and Kimi CLI is transitioning to Kimi Code CLI"
    );
    println!("as its successor project — re-verify this integration when adopting the successor.");
}

fn print_init_windsurf() {
    println!("Add the following to your `.windsurf/hooks.json` (workspace) to register");
    println!("portzilla's kill-guard as a pre_run_command hook (blocks dangerous commands like");
    println!("kills of other sessions' processes):");
    println!();
    println!("{}", windsurf::CONFIG_SNIPPET);
    println!();
    println!("Alternatively, place the same file at ~/.codeium/windsurf/hooks.json (user level),");
    println!("or at a system-level path (see the Cascade Hooks docs). If you already have a");
    println!("hooks.json, merge the \"pre_run_command\" entry into the existing \"hooks\" object");
    println!("instead of overwriting the file.");
    println!();
    println!("`show_output: true` prints portzilla's deny/warning text in the Cascade UI (a deny");
    println!("reaches the agent regardless via stderr; warnings are human-visible ONLY — Windsurf");
    println!("has no non-blocking channel the model sees, so a warn never blocks the command).");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
    println!();
    println!("Note: Windsurf does not currently expose the trajectory id to the shell commands it");
    println!("runs, only to the hook payload itself, so claims made from a Cascade session cannot");
    println!(
        "be tagged with a matching --session. This hook still protects against killing another"
    );
    println!("session's live process; it just can't yet recognize a lease as your own.");
    println!();
    println!("Note: Cascade hooks do not load or run while a workspace is open in Restricted");
    println!("Mode — the guard is absent there by design.");
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
    println!(
        "Add the following to your Claude Code settings (.claude/settings.json for a project,"
    );
    println!("or ~/.claude/settings.json for your user) to register portzilla's kill-guard as a");
    println!("PreToolUse hook on the Bash tool:");
    println!();
    println!("{}", claude_code::SETTINGS_SNIPPET);
    println!();
    println!(
        "If you already have a \"hooks\" key in settings.json, merge the \"PreToolUse\" entry"
    );
    println!("above into your existing hooks instead of overwriting the file.");
    println!();
    println!("portzilla must be on PATH as `portzilla` for the hook command above to run.");
    println!("Verify with: portzilla --version");
}

/// Resolves the default PID to attribute a claim to: the parent process
/// (the shell or agent invoking `portzilla`). Falls back to this process's
/// own PID if the parent cannot be determined.
fn default_pid() -> u32 {
    use sysinfo::{ProcessesToUpdate, System, get_current_pid};

    let Ok(current) = get_current_pid() else {
        return std::process::id();
    };
    let mut system = System::new();
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
