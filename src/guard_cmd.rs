//! Universal `portzilla guard -- <command...>` wrapper.
//!
//! For harnesses with no hook mechanism (Aider, ad hoc scripts) and for
//! humans who just want the kill-guard in front of a command manually: runs
//! [`crate::guard::check`] against the joined command string, then either
//! blocks (deny), warns and proceeds (warn), or proceeds silently (allow).
//! Like the hook adapters, this module is a thin translation layer — all
//! detection/lease-resolution logic stays in `guard`.
//!
//! # Session resolution
//!
//! `--session <S>` if given, else the `PORTZILLA_SESSION` environment
//! variable, else no session at all (every live foreign-looking lease is
//! then deny-worthy). There is no `self_pid` here either, for the same
//! structural reason as the hook adapters: the command hasn't run yet, so
//! there is no PID to offer.
//!
//! # Fail-open
//!
//! A lease-store failure (corrupt state, I/O error, lock failure) warns to
//! stderr and executes the command anyway — the guard failing is not a
//! reason to block a command a human or script explicitly asked to run.
//! This mirrors the hook adapters' fail-open principle (see
//! `src/guard.rs`'s module doc), adapted to a foreground CLI: there is no
//! silent "no decision" here (there is a real human/script waiting on this
//! process), so failures are surfaced on stderr rather than swallowed
//! entirely, but they never block execution.

use crate::guard::{self, MAX_SHELL_UNWRAP_DEPTH, Verdict, find_shell_c_payload, shell_words};
use crate::lease::{Lease, PidChecker};

/// What `main.rs` should do after resolving a verdict for the command.
#[derive(Debug, PartialEq)]
pub enum GuardAction {
    /// Do not run the command. `explanation` goes to stderr; the caller
    /// must exit nonzero (2) without executing anything.
    Deny { explanation: String },
    /// Print `explanation` to stderr, then run the command anyway.
    WarnThenExecute { explanation: String },
    /// Run the command with no message.
    Execute,
}

/// Builds the string [`guard::check`] analyzes from `args` (the command and
/// its arguments, exactly as given after `--`).
///
/// The default is a plain space-join. There is one deliberate special case:
/// a leading shell invocation of the form `sh -c '<payload>'` (or
/// `bash`/`zsh`/`dash` in place of `sh`, with any run of dash-prefixed
/// flags — combined or separate, e.g. `-lc`, `-x -c`, `--norc -c` — before
/// the payload) is unwrapped to the raw payload, recursively, up to
/// [`MAX_SHELL_UNWRAP_DEPTH`] levels. This is the only way to route a real
/// pipe or compound command (`lsof ... | xargs kill`) through `portzilla
/// guard -- ...` at all, since `exec` runs the given program directly with
/// no shell of its own to interpret a literal `|` in the arguments. Naively
/// joining `sh -c <payload>` with spaces silently defeats detection:
/// `guard::check` requires a kill verb to be the FIRST word of its own
/// pipeline segment (a deliberate anti-false-positive design — see
/// `src/guard.rs`), and in the naively-joined string `sh`/`-c` occupy that
/// position instead of the real verb inside the payload. Unwrapping here
/// is the same normalization the core now applies to the joined command
/// string it eventually analyzes, so detection never depends on which
/// adapter produced the string.
///
/// # Known gaps (disclosed, not silent)
///
/// This is a targeted unwrap for the `sh -c`-family shape specifically, not
/// a shell parser — same spirit as `src/guard.rs`'s own documented
/// approximation, and with similar boundaries:
/// - Only `sh`, `bash`, `zsh`, `dash` (by basename) are recognized as shell
///   invocations. `ksh`, `fish`, `env sh -c ...`, `python3 -c`, PowerShell's
///   `-Command`, etc. are not unwrapped.
/// - No command substitution, variable expansion, backslash escapes, or
///   ANSI-C quoting (`$'...'`) — a payload like `sh -c "$(echo kill 1234)"`
///   is analyzed as the literal text `$(echo kill 1234)`, not the command
///   substitution's result, so a kill hidden behind substitution is not
///   detected.
/// - The quote handling in [`shell_words`] understands single and double
///   quotes (including one nested inside the other, which is what makes the
///   common `sh -c 'sh -c "kill 1234"'` nesting pattern work), but not
///   escaped quotes within the same quote type or other shell escaping
///   rules.
/// - Only the flag cluster immediately before the payload token is
///   inspected for a `c`; a flag that takes its own separate argument
///   (other than the payload itself) is not modeled, so an invocation like
///   `sh -o pipefail -c '<payload>'` is not unwrapped (`-o` here isn't a
///   short cluster ending in `c`, and `pipefail` — its argument — is not
///   itself flag-shaped, so the loop stops before reaching `-c`).
pub fn join_command(args: &[String]) -> String {
    unwrap_shell_c(args, MAX_SHELL_UNWRAP_DEPTH).unwrap_or_else(|| args.join(" "))
}

/// Recursively unwraps a leading `sh -c`-family invocation in `args` down to
/// the innermost payload, bounded by `depth`. Returns `None` if `args`
/// isn't such an invocation at all (the top-level caller then falls back to
/// a plain join).
fn unwrap_shell_c(args: &[String], depth: u32) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let payload = find_shell_c_payload(args)?;
    let inner_tokens = shell_words(&payload);
    Some(unwrap_shell_c(&inner_tokens, depth - 1).unwrap_or(payload))
}

/// Decides what to do with a command about to be run, given the current
/// lease registry and the caller's session/PID identity (see the module
/// doc comment for how those are resolved before this is called).
pub fn decide(
    command_display: &str,
    leases: &[Lease],
    self_pid: Option<u32>,
    self_session: Option<&str>,
    checker: &dyn PidChecker,
) -> GuardAction {
    match guard::check(command_display, leases, self_pid, self_session, checker) {
        Verdict::Allow => GuardAction::Execute,
        Verdict::Deny { explanation, .. } => GuardAction::Deny { explanation },
        Verdict::Warn { explanation } => GuardAction::WarnThenExecute { explanation },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Lease;
    use crate::lease::test_support::{AlwaysAlive, AlwaysDead};

    fn lease(port: u16, pid: u32, tag: &str) -> Lease {
        Lease::new(port, pid, tag, None)
    }

    #[test]
    fn join_command_joins_args_with_spaces() {
        let args = vec!["npm".to_string(), "run".to_string(), "dev".to_string()];
        assert_eq!(join_command(&args), "npm run dev");
    }

    #[test]
    fn join_command_on_a_single_arg_returns_it_unchanged() {
        let args = vec!["ls".to_string()];
        assert_eq!(join_command(&args), "ls");
    }

    #[test]
    fn join_command_on_empty_args_returns_empty_string() {
        let args: Vec<String> = vec![];
        assert_eq!(join_command(&args), "");
    }

    #[test]
    fn join_command_unwraps_sh_dash_c_to_the_raw_payload() {
        // `sh -c '<payload>'` arrives as three argv elements: ["sh", "-c",
        // "<payload>"]. Naively joining with spaces produces "sh -c
        // <payload>", which defeats guard::check's segment-first-word
        // detection for anything piped inside the payload (see the
        // decide_analyzes_a_joined_sh_c_payload_and_still_denies test for
        // why). The payload alone must be handed to guard::check instead.
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "lsof -ti:3000 | xargs kill".to_string(),
        ];
        assert_eq!(join_command(&args), "lsof -ti:3000 | xargs kill");
    }

    #[test]
    fn join_command_unwraps_bash_dash_c_too() {
        let args = vec![
            "bash".to_string(),
            "-c".to_string(),
            "kill 1234".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_does_not_unwrap_a_plain_command_that_happens_to_have_three_args() {
        let args = vec!["echo".to_string(), "-c".to_string(), "hello".to_string()];
        assert_eq!(join_command(&args), "echo -c hello");
    }

    // ---- generalized unwrap: combined/separate flags, nesting, boundaries ----
    // (fixes CRITICAL: the exact-3-argv-element match previously missed all of
    // these, letting a kill through undetected)

    #[test]
    fn join_command_unwraps_combined_short_flags_ending_in_c() {
        let args = vec!["sh".to_string(), "-lc".to_string(), "kill 1234".to_string()];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_unwraps_combined_short_flags_eic() {
        let args = vec![
            "bash".to_string(),
            "-eic".to_string(),
            "kill 1234".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_unwraps_separate_dash_x_dash_c() {
        let args = vec![
            "sh".to_string(),
            "-x".to_string(),
            "-c".to_string(),
            "kill 1234".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_unwraps_long_flag_then_dash_c() {
        // `--norc` must NOT be mistaken for a `-c` flag just because its name
        // contains the letter 'c' — only `-c` (or a short cluster containing
        // `c`, e.g. `-lc`) counts. The real `-c` here is the separate token
        // right before the payload.
        let args = vec![
            "bash".to_string(),
            "--norc".to_string(),
            "-c".to_string(),
            "kill 1234".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_unwraps_nested_sh_c() {
        // `sh -c "sh -c 'kill 1234'"`: the outer payload is itself a shell
        // invocation. Recursion must reach the innermost real command.
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sh -c 'kill 1234'".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_unwraps_doubly_nested_sh_c() {
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sh -c 'sh -c \"kill 1234\"'".to_string(),
        ];
        assert_eq!(join_command(&args), "kill 1234");
    }

    #[test]
    fn join_command_sh_c_with_a_non_kill_payload_still_returns_the_payload() {
        let args = vec!["sh".to_string(), "-c".to_string(), "echo hello".to_string()];
        assert_eq!(join_command(&args), "echo hello");
    }

    #[test]
    fn join_command_sh_c_missing_payload_does_not_panic_and_falls_back_to_plain_join() {
        let args = vec!["sh".to_string(), "-c".to_string()];
        assert_eq!(join_command(&args), "sh -c");
    }

    #[test]
    fn join_command_bare_sh_does_not_panic_and_falls_back_to_plain_join() {
        let args = vec!["sh".to_string()];
        assert_eq!(join_command(&args), "sh");
    }

    #[test]
    fn join_command_never_panics_on_adversarial_shell_like_input() {
        let adversarial: Vec<Vec<String>> = vec![
            vec![],
            vec!["sh".to_string()],
            vec!["sh".to_string(), "-c".to_string()],
            vec!["sh".to_string(), "-".to_string(), "x".to_string()],
            vec!["sh".to_string(), "--".to_string(), "-c".to_string()],
            vec!["sh".to_string(), "-c".to_string(), "sh -c".to_string()],
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "sh -c 'sh -c 'sh -c".to_string(),
            ],
            {
                // A deeply (pathologically) nested chain must not blow the
                // stack or hang — bounded recursion should just stop.
                let mut payload = "kill 1234".to_string();
                for _ in 0..50 {
                    payload = format!("sh -c '{payload}'");
                }
                vec!["sh".to_string(), "-c".to_string(), payload]
            },
        ];
        for args in adversarial {
            let _ = join_command(&args);
        }
    }

    #[test]
    fn decide_denies_kill_reached_via_nested_sh_c() {
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sh -c 'kill 1234'".to_string(),
        ];
        let joined = join_command(&args);
        let leases = vec![lease(3000, 1234, "dev-server")];
        let action = decide(&joined, &leases, None, None, &AlwaysAlive);
        assert!(
            matches!(action, GuardAction::Deny { .. }),
            "joined command was: {joined}"
        );
    }

    #[test]
    fn decide_denies_kill_of_a_foreign_live_lease() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let action = decide("kill 1234", &leases, None, None, &AlwaysAlive);
        match action {
            GuardAction::Deny { explanation } => {
                assert!(explanation.contains("3000"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn decide_executes_on_a_dead_lease() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            decide("kill 1234", &leases, None, None, &AlwaysDead),
            GuardAction::Execute
        );
    }

    #[test]
    fn decide_executes_on_a_non_kill_command() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            decide("git status", &leases, None, None, &AlwaysAlive),
            GuardAction::Execute
        );
    }

    #[test]
    fn decide_warns_then_executes_on_an_unresolvable_process_name() {
        let leases: Vec<Lease> = vec![];
        let action = decide("pkill node", &leases, None, None, &AlwaysAlive);
        match action {
            GuardAction::WarnThenExecute { explanation } => assert!(explanation.contains("node")),
            other => panic!("expected WarnThenExecute, got {other:?}"),
        }
    }

    #[test]
    fn decide_allows_own_session_lease() {
        let leases = vec![Lease::new(
            3000,
            1234,
            "dev-server",
            Some("sess-mine".to_string()),
        )];
        assert_eq!(
            decide("kill 1234", &leases, None, Some("sess-mine"), &AlwaysAlive),
            GuardAction::Execute
        );
    }

    #[test]
    fn decide_denies_foreign_session_lease() {
        let leases = vec![Lease::new(
            3000,
            1234,
            "dev-server",
            Some("sess-theirs".to_string()),
        )];
        let action = decide("kill 1234", &leases, None, Some("sess-mine"), &AlwaysAlive);
        assert!(matches!(action, GuardAction::Deny { .. }));
    }

    #[test]
    fn decide_analyzes_a_joined_sh_c_payload_and_still_denies() {
        // `portzilla guard -- sh -c 'lsof -ti:3000 | xargs kill'`: the args
        // after `--` are ["sh", "-c", "lsof -ti:3000 | xargs kill"]. Joined
        // with spaces, the pipe is still present in the joined string, so
        // guard::check's segment-based lsof/xargs correlation still fires.
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "lsof -ti:3000 | xargs kill".to_string(),
        ];
        let joined = join_command(&args);
        let leases = vec![lease(3000, 1234, "dev-server")];
        let action = decide(&joined, &leases, None, None, &AlwaysAlive);
        assert!(
            matches!(action, GuardAction::Deny { .. }),
            "joined command was: {joined}"
        );
    }
}
