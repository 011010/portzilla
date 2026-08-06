mod claude_code;
mod guard;
mod lease;
mod mcp;
mod store;
mod view;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lease::{Lease, SystemPidChecker};
use std::io::Read;
use store::{ClaimOutcome, Store};
use view::{to_claim_view, to_view, LeaseView};

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
        /// The port to claim.
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
}

#[derive(Subcommand)]
enum HookHarness {
    /// Run as a Claude Code `PreToolUse` hook: reads the hook payload JSON
    /// from stdin, evaluates any Bash command against the lease registry,
    /// and writes the hook response JSON to stdout. Never blocks or fails
    /// the tool call because of a portzilla-side error (fails open).
    ClaudeCode,
}

#[derive(Subcommand)]
enum InitHarness {
    /// Print the settings.json snippet that registers portzilla's
    /// kill-guard as a Claude Code `PreToolUse` hook on the `Bash` tool.
    ClaudeCode,
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

    // `serve` spins up its own async runtime and opens its own `Store`
    // internally (see `mcp::run_stdio`); handle it before the rest of the
    // (fully synchronous) commands, which share one `store` below.
    if let Commands::Serve { mcp } = &cli.command {
        let mcp = *mcp;
        if !mcp {
            return Err(RunError::Other(anyhow::anyhow!(
                "serve requires a mode flag; currently only --mcp is supported (e.g. `portzilla serve --mcp`)"
            )));
        }
        let runtime = tokio::runtime::Runtime::new()
            .context("failed to start the async runtime for the MCP server")?;
        runtime.block_on(mcp::run_stdio())?;
        return Ok(());
    }

    // `init claude-code` is pure output, no store needed.
    if let Commands::Init { harness } = &cli.command {
        match harness {
            InitHarness::ClaudeCode => print_init_claude_code(),
        }
        return Ok(());
    }

    // `hook claude-code` manages its own store access with fail-open
    // semantics throughout (see `run_hook_claude_code`) — it must never
    // propagate an error via `?`, since that would turn into a nonzero exit
    // for what should always be a silent, non-blocking hook response.
    if let Commands::Hook { harness } = &cli.command {
        match harness {
            HookHarness::ClaudeCode => run_hook_claude_code(),
        }
        return Ok(());
    }

    let store = Store::open(None)?;

    match cli.command {
        Commands::Serve { .. } => unreachable!("handled above"),
        Commands::Init { .. } => unreachable!("handled above"),
        Commands::Hook { .. } => unreachable!("handled above"),
        Commands::Claim {
            port,
            tag,
            pid,
            session,
            json,
        } => {
            let pid = pid.unwrap_or_else(default_pid);
            let outcome = store.claim(port, pid, tag, session, &SystemPidChecker)?;
            print_claim_outcome(&outcome, port, json);
        }
        Commands::Ls { json } => {
            let leases = store.list()?;
            print_leases(&leases, json);
        }
        Commands::Who { port, json } => match store.get(port)? {
            Some(lease) => print_lease_view(&to_view(&lease, &SystemPidChecker), json),
            None => return Err(RunError::NotFound(port)),
        },
        Commands::Release { port, json } => match store.release(port, &SystemPidChecker)? {
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
        },
        Commands::Prune { json } => {
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
    let mut raw_input = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut raw_input) {
        eprintln!("portzilla hook claude-code: failed to read stdin, failing open (allow): {err:#}");
        return;
    }

    let leases = match Store::open(None).and_then(|store| store.list()) {
        Ok(leases) => leases,
        Err(err) => {
            eprintln!(
                "portzilla hook claude-code: failed to read the lease store, failing open (allow): {err:#}"
            );
            Vec::new()
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        claude_code::handle(&raw_input, &leases, &SystemPidChecker)
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
            eprintln!(
                "portzilla hook claude-code: internal error while evaluating the command, failing \
                 open (allow)"
            );
        }
    }
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
    use sysinfo::{get_current_pid, ProcessesToUpdate, System};

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
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
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
    let views: Vec<LeaseView> = leases.iter().map(|lease| to_view(lease, &SystemPidChecker)).collect();
    if json {
        println!(
            "{}",
            serde_json::to_string(&views).expect("LeaseView always serializes")
        );
        return;
    }

    println!("{:<7} {:<8} {:<6} {:<10} TAG", "PORT", "PID", "STATUS", "AGE");
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
    let views: Vec<LeaseView> = pruned.iter().map(|lease| to_view(lease, &SystemPidChecker)).collect();
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
