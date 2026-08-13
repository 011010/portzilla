//! Kimi CLI adapter for the kill-guard.
//!
//! Translates a Kimi CLI `PreToolUse` hook payload into a call to the
//! harness-agnostic [`crate::guard`], and translates the resulting
//! [`crate::guard::Verdict`] back into the exit-code/stream contract Kimi
//! expects. Thin by design — see `src/guard.rs`'s module doc for why.
//!
//! Schema verified against the current Kimi CLI hooks documentation
//! (<https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/hooks.md>,
//! cross-checked against `src/kimi_cli/hooks/runner.py` in the same repo) —
//! see the doc comments below for exactly which fields and behavior that
//! verification covers.
//!
//! # A different response contract: exit codes, not stdout JSON
//!
//! Kimi's `PreToolUse` contract is deliberately Claude Code-flavored on the
//! *input* side (stdin JSON with `session_id`, `cwd`, `hook_event_name`,
//! `tool_name`, `tool_input.command`) but exit-code driven on the *output*
//! side:
//!
//! - exit 0 → allow; **non-empty stdout is added to the model's context**
//!   (the non-blocking, model-visible warn channel this adapter uses for
//!   [`Verdict::Warn`], printed as plain text — a JSON `hookSpecificOutput`
//!   blob on stdout would instead be parsed as a structured decision)
//! - exit 2 → block; stderr is fed back to the model as a correction (this
//!   adapter's [`Verdict::Deny`] path)
//! - any other exit code → allow; stderr is logged only
//!
//! A structured JSON decision (`hookSpecificOutput.permissionDecision`) on
//! exit 0 is also documented, but this adapter deliberately uses the
//! exit-2/stderr path for denies: the exit code is the unambiguous channel,
//! with no JSON parse between portzilla's verdict and Kimi's runner.
//!
//! # Ownership caveat (documented, not silently dropped)
//!
//! `self_session` is set from the hook payload's `session_id`, which IS a
//! documented input field. But per the Kimi CLI env-vars reference
//! (`docs/en/configuration/env-vars.md`) and the shell tool source, no
//! variable exposes that session id to the shell commands the agent itself
//! runs — so a claim made from inside a Kimi session cannot currently be
//! tagged with the session id this hook receives. Foreign-lease protection
//! only, no own-lease recognition. See the README's Kill guard section.
//!
//! # Beta status
//!
//! Kimi's hooks system is documented as Beta ("implementation details and
//! configuration definitions may change"), and Kimi CLI itself is
//! transitioning to a successor project (Kimi Code CLI). This adapter is
//! built against the currently documented contract.
//!
//! # Fail-open, concretely
//!
//! Kimi's own runner is documented fail-open (timeouts, crashes, and any
//! non-2 exit all allow), which composes with portzilla's fail-open
//! principle: every portzilla-side failure here resolves to exit 0 with
//! empty stdout plus a stderr diagnostic — never a block.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure
//! flips from "allow + note" to exit 2 with the reason on stderr (Kimi's
//! own block channel) instead.

use crate::guard::{self, Verdict};
use crate::lease::{Lease, PidChecker};
use serde::Deserialize;

/// Exit code used for a deny. Kimi's contract: exit 2 blocks the action
/// and feeds stderr back to the model as a correction.
const EXIT_BLOCK: i32 = 2;

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_text` to stdout if present (plain text —
/// on exit 0 Kimi adds it to the model's context), print `stderr_note` to
/// stderr if present, and exit with `exit_code`. Unlike the JSON-shaped
/// adapters, the exit code is load-bearing here: 0 allows, 2 blocks.
pub struct HookOutcome {
    pub stdout_text: Option<String>,
    pub stderr_note: Option<String>,
    pub exit_code: i32,
}

impl HookOutcome {
    fn allow_silent() -> Self {
        Self {
            stdout_text: None,
            stderr_note: None,
            exit_code: 0,
        }
    }

    fn allow_with_note(note: impl Into<String>) -> Self {
        Self {
            stdout_text: None,
            stderr_note: Some(note.into()),
            exit_code: 0,
        }
    }

    /// Allow, but put `warning` in front of the model: on exit 0 Kimi adds
    /// non-empty stdout to the model's context, which is the documented
    /// non-blocking model-visible channel.
    fn allow_with_model_visible_warning(warning: String) -> Self {
        Self {
            stdout_text: Some(warning),
            stderr_note: None,
            exit_code: 0,
        }
    }

    fn deny_with_reason(reason: String) -> Self {
        Self {
            stdout_text: None,
            stderr_note: Some(reason),
            exit_code: EXIT_BLOCK,
        }
    }
}

/// The subset of the Kimi CLI `PreToolUse` hook input JSON this adapter
/// reads. Verified fields (per the hooks doc's payload example):
/// `session_id`, `cwd`, `hook_event_name`, `tool_name`, `tool_input`
/// (with `command` for the `Shell` tool), `tool_call_id`. This adapter
/// needs `tool_name`, `tool_input.command`, and `session_id`; every other
/// field is ignored via `#[serde(default)]` tolerance rather than declared,
/// so payload fields we don't use (or future additions) never cause a
/// parse failure.
#[derive(Debug, Deserialize)]
struct PreToolUseInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<ToolInput>,
}

#[derive(Debug, Deserialize)]
struct ToolInput {
    #[serde(default)]
    command: Option<String>,
}

/// The exact tool name Kimi CLI uses for shell execution, per the hooks
/// doc's own examples (`matcher = "Shell"` intercepting shell commands).
const SHELL_TOOL_NAME: &str = "Shell";

/// Fail-open wrapper kept for parity with the other adapters' in-process
/// unit tests; `main.rs` calls [`handle_with_policy`] so it can pass the
/// `PORTZILLA_FAIL_CLOSED` policy.
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, false)
}

/// Handles one `PreToolUse` hook invocation: parses `raw_input`, and if
/// it's a [`SHELL_TOOL_NAME`] tool call, runs its command through
/// [`guard::check`] against `leases`.
///
/// `self_pid` is always `None` when calling into `guard`: `PreToolUse`
/// fires *before* the tool runs, so there is no process yet whose PID this
/// adapter could pass as "the process about to run this." Ownership
/// therefore resolves entirely through `self_session` — see the module doc
/// comment for why that only ever protects against foreign kills today.
///
/// When `fail_closed` is `true`, every portzilla-side failure (unparseable
/// JSON, missing command, …) returns exit 2 with the reason on stderr
/// instead of allow + stderr note.
pub fn handle_with_policy(
    raw_input: &str,
    leases: &[Lease],
    checker: &dyn PidChecker,
    fail_closed: bool,
) -> HookOutcome {
    let input: PreToolUseInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome::deny_with_reason(
                    "portzilla: could not parse hook input JSON".to_string(),
                );
            }
            return HookOutcome::allow_with_note(format!(
                "portzilla hook kimi: could not parse hook input JSON, failing open (allow): {err}"
            ));
        }
    };

    if input.tool_name.as_deref() != Some(SHELL_TOOL_NAME) {
        // Not a Shell call — nothing for the kill-guard to check. Silent,
        // since this is the common case when the matcher isn't scoped and
        // not worth a diagnostic line.
        return HookOutcome::allow_silent();
    }

    let self_session = input.session_id;

    let Some(command) = input.tool_input.and_then(|t| t.command) else {
        if fail_closed {
            return HookOutcome::deny_with_reason(
                "portzilla: Shell tool call had no tool_input.command".to_string(),
            );
        }
        return HookOutcome::allow_with_note(
            "portzilla hook kimi: Shell tool call had no tool_input.command, failing open (allow)"
                .to_string(),
        );
    };

    match guard::check(&command, leases, None, self_session.as_deref(), checker) {
        Verdict::Allow => HookOutcome::allow_silent(),
        Verdict::Deny { explanation, .. } => HookOutcome::deny_with_reason(explanation),
        Verdict::Warn { explanation } => {
            HookOutcome::allow_with_model_visible_warning(format!("portzilla: {explanation}"))
        }
    }
}

/// The `~/.kimi/config.toml` snippet printed by `portzilla init kimi`,
/// registering this hook on `PreToolUse` scoped to the `Shell` tool.
/// Format verified against the hooks doc's configuration example
/// (`[[hooks]]` array with `event`, `matcher` (regex), `command`,
/// `timeout`). Note: only user-level registration via
/// `~/.kimi/config.toml` is documented; project-level hook config is not
/// positively verified, so the snippet targets the user-level file.
pub const CONFIG_SNIPPET: &str = r#"[[hooks]]
event = "PreToolUse"
matcher = "Shell"
command = "portzilla hook kimi"
timeout = 10"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Lease;
    use crate::lease::test_support::{AlwaysAlive, AlwaysDead};

    fn lease(port: u16, pid: u32, tag: &str) -> Lease {
        Lease::new(port, pid, tag, None)
    }

    fn lease_with_session(port: u16, pid: u32, tag: &str, session: &str) -> Lease {
        Lease::new(port, pid, tag, Some(session.to_string()))
    }

    fn shell_input(command: &str) -> String {
        shell_input_with_session(command, "abc123")
    }

    fn shell_input_with_session(command: &str, session_id: &str) -> String {
        serde_json::json!({
            "session_id": session_id,
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_name": "Shell",
            "tool_input": { "command": command },
            "tool_call_id": "call_01"
        })
        .to_string()
    }

    #[test]
    fn deny_on_foreign_live_lease_exits_2_with_stderr_reason() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysAlive);

        assert_eq!(outcome.exit_code, 2, "deny must exit 2 (Kimi's block)");
        assert!(
            outcome.stdout_text.is_none(),
            "deny must not print to stdout"
        );
        let reason = outcome
            .stderr_note
            .expect("deny must carry a stderr reason");
        assert!(reason.contains("3000"));
        assert!(reason.contains("dev-server"));
    }

    #[test]
    fn allow_on_kill_of_unleased_pid_exits_0_silent() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("kill 9999"), &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_non_kill_command_exits_0_silent() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("git status"), &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_dead_lease_exits_0_silent() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysDead);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
    }

    #[test]
    fn non_shell_tool_is_a_silent_no_op() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "session_id": "abc123",
            "hook_event_name": "PreToolUse",
            "tool_name": "ReadFile",
            "tool_input": { "path": "/tmp/x" }
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn malformed_json_fails_open_exit_0_with_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle("{ not valid json", &leases, &AlwaysAlive);

        assert_eq!(outcome.exit_code, 0, "malformed input must fail open");
        assert!(outcome.stdout_text.is_none());
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open")),
            "malformed input should still leave a diagnostic trail"
        );
    }

    #[test]
    fn shell_call_missing_command_fails_open_with_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "session_id": "abc123",
            "hook_event_name": "PreToolUse",
            "tool_name": "Shell",
            "tool_input": {}
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn empty_stdin_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle("", &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn warn_on_unresolvable_process_name_exits_0_with_plain_text_stdout() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("pkill node"), &leases, &AlwaysAlive);

        assert_eq!(outcome.exit_code, 0, "warn must not block");
        assert!(outcome.stderr_note.is_none());
        let warning = outcome
            .stdout_text
            .expect("warn must print plain text to stdout (added to model context)");
        assert!(warning.contains("node"));
        // Plain text, NOT JSON: a JSON hookSpecificOutput blob on stdout
        // would be parsed as a structured decision instead of added to the
        // model's context as a warning.
        assert!(
            serde_json::from_str::<serde_json::Value>(&warning).is_err(),
            "warn output must be plain text, not parseable JSON"
        );
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        // The lease was claimed under the SAME session that's now trying to
        // kill it. With self_pid always None from this adapter, session is
        // the ONLY way ownership can ever resolve here, so this must Allow.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-mine")];
        let outcome = handle(
            &shell_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(outcome.exit_code, 0, "own-session kill must be allowed");
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn denies_kill_of_a_lease_owned_by_a_different_session() {
        let leases = vec![lease_with_session(
            3000,
            1234,
            "dev-server",
            "session-theirs",
        )];
        let outcome = handle(
            &shell_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(outcome.exit_code, 2, "foreign-session kill must be denied");
    }

    #[test]
    fn fail_closed_policy_denies_on_malformed_json() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle_with_policy("{ not valid json", &leases, &AlwaysAlive, true);

        assert_eq!(outcome.exit_code, 2, "fail-closed must block");
        assert!(outcome.stdout_text.is_none());
        let reason = outcome
            .stderr_note
            .expect("fail-closed must carry a reason");
        assert!(reason.contains("could not parse hook input JSON"));
    }

    #[test]
    fn config_snippet_registers_pretooluse_on_the_shell_matcher() {
        // No TOML parser in dev-deps: assert on the documented fields
        // directly. The snippet is verified against the hooks doc's own
        // `[[hooks]]` configuration example.
        assert!(CONFIG_SNIPPET.contains("[[hooks]]"));
        assert!(CONFIG_SNIPPET.contains(r#"event = "PreToolUse""#));
        assert!(CONFIG_SNIPPET.contains(r#"matcher = "Shell""#));
        assert!(CONFIG_SNIPPET.contains(r#"command = "portzilla hook kimi""#));
        assert!(CONFIG_SNIPPET.contains("timeout"));
    }
}
