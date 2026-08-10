mod claude_code;
mod cursor;
mod gemini;
mod guard;
mod guard_cmd;
mod lease;
mod mcp;
mod store;
mod view;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lease::{Lease, SystemPidChecker};
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
}

#[derive(Subcommand)]
enum HookHarness {
    /// Run as a Claude Code `PreToolUse` hook: reads the hook payload JSON
    /// from stdin, evaluates any Bash command against the lease registry,
    /// and writes the hook response JSON to stdout. Never blocks or fails
    /// the tool call because of a portzilla-side error (fails open).
    ClaudeCode,
    /// Run as a Cursor `beforeShellExecution` hook: reads the hook payload
    /// JSON from stdin, evaluates the command against the lease registry,
    /// and writes the hook response JSON to stdout. Never blocks or fails
    /// the command because of a portzilla-side error (fails open).
    Cursor,
    /// Run as a Gemini CLI `BeforeTool` hook (scoped to the
    /// `run_shell_command` tool): reads the hook payload JSON from stdin
    /// and writes the hook response JSON to stdout. Never blocks or fails
    /// the tool call because of a portzilla-side error (fails open).
    Gemini,
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

        // Pure output, no store needed.
        Commands::Init { harness } => match harness {
            InitHarness::ClaudeCode => print_init_claude_code(),
            InitHarness::Cursor => print_init_cursor(),
            InitHarness::Gemini => print_init_gemini(),
        },

        // Each hook handler manages its own store access with fail-open
        // semantics throughout — none of them may propagate an error via
        // `?`, since that would turn into a nonzero exit for what should
        // always be a silent, non-blocking hook response.
        Commands::Hook { harness } => match harness {
            HookHarness::ClaudeCode => run_hook_claude_code(),
            HookHarness::Cursor => run_hook_cursor(),
            HookHarness::Gemini => run_hook_gemini(),
        },

        // `guard` either execs the given command (never returns on unix
        // success) or exits directly (deny, or a non-unix execution) — it
        // does not fall through to `Ok(())` below in the success path.
        Commands::Guard { session, command } => run_guard_cmd(session, command),

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
fn sanitize_for_display(s: &str) -> String {
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
        println!(
            "port {requested_port} is busy; claimed port {} instead for pid {} (tag: {})",
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
