//! Windsurf (Cascade) adapter for the kill-guard.
//!
//! Translates a Windsurf `pre_run_command` hook payload into a call to the
//! harness-agnostic [`crate::guard`], and translates the resulting
//! [`crate::guard::Verdict`] back into the exit-code contract Windsurf
//! expects. Thin by design — see `src/guard.rs`'s module doc for why.
//!
//! Schema verified against the current Cascade Hooks documentation
//! (<https://docs.windsurf.com/windsurf/cascade/hooks>, now De
//! Desktop) — see the doc comments below for exactly which fields and
//! behavior that verification covers.
//!
//! # An exit-code response contract, no stdout JSON
//!
//! Windsurf's `pre_run_command` hook contract is exit-code driven on the
//! *output* side — there is no JSON response protocol at all:
//!
//! - exit 0 → allow
//! - exit 2 → block; the Cascade agent sees the message on stderr (this
//!   adapter's [`Verdict::Deny`] path)
//! - any other exit code → allow (Windsurf's documented fail-open)
//!
//! # No model-visible warn channel
//!
//! `pre_run_command` has no non-blocking channel the model sees. Windsurf's
//! `show_output` config flag only prints hook output to the *user-facing*
//! Cascade UI, never to the model. [`Verdict::Warn`] therefore rides stderr
//! on exit 0 here: it never blocks, and a human watching Cascade (with
//! `show_output: true`) sees it. Same documented tradeoff as Gemini CLI's
//! adapter.
//!
//! # Ownership caveat (documented, not silently dropped)
//!
//! `self_session` is set from the hook payload's `trajectory_id` (the
//! conversation id), which IS a documented input field. But per the
//! Cascade Hooks docs, no environment variable exposes that id to the shell
//! commands Cascade itself spawns — so a claim made from inside a Cascade
//! session cannot currently be tagged with the trajectory id this hook
//! receives. Foreign-lease protection only, no own-lease recognition. See
//! the README's Kill guard section.
//!
//! # Restricted Mode
//!
//! Cascades hooks do not load or run while a workspace is open in Restricted
//! Mode — the guard is simply absent there, which is Windsurf's own decision.
//!
//! # Fail-open, concretely
//!
//! Windsurf is documented fail-open (every exit code except 2 allows), which
//! composes with portzilla's fail-open principle: every portzilla-side
//! failure here resolves to exit 0 with empty stdout plus a stderr
//! diagnostic — never a block.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure
//! flips from "allow + note" to exit 2 with the reason on stderr (Windsurf's
//! own block channel) instead.

use crate::guard::{self, Verdict};
use crate::lease::{Lease, PidChecker};
use serde::Deserialize;

/// Exit code used for a deny. Windsurf's contract: exit 2 blocks the
/// command and the Cascade agent sees stderr.
const EXIT_BLOCK: i32 = 2;

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_text` to stdout if present, print
/// `stderr_note` to stderr if present, and exit with `exit_code`. Like
/// Kimi's adapter (same exit-code-driven contract), the exit code is
/// load-bearing: 0 allows, 2 blocks.
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

    /// Allow, but leave `note` on stderr (a human with `show_output: true`
    /// sees it; the model does not — no model-visible warn channel exists).
    fn allow_human_visible_note(note: impl Into<String>) -> Self {
        Self {
            stdout_text: None,
            stderr_note: Some(note.into()),
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

/// The subset of the Windsurf `pre_run_command` hook input JSON this adapter
/// reads. Verified fields (per the hooks doc's input example):
/// `agent_action_name`, `trajectory_id`, `execution_id`, `timestamp`,
/// `tool_info` (with `command_line` and `cwd`). Every other field is ignored
/// via `#[serde(default)]` tolerance rather than declared, so payload fields
/// we don't use (or future additions) never cause a parse failure.
#[derive(Debug, Deserialize)]
struct PreRunCommandInput {
    #[serde(default)]
    agent_action_name: Option<String>,
    #[serde(default)]
    trajectory_id: Option<String>,
    #[serde(default)]
    tool_info: Option<ToolInfo>,
}

#[derive(Debug, Deserialize)]
struct ToolInfo {
    #[serde(default)]
    command_line: Option<String>,
}

/// The exact hook event Windsurf fires before executing a terminal command,
/// per the hooks doc — the adapter is scoped to it in the config snippet.
const PRE_RUN_COMMAND_EVENT: &str = "pre_run_command";

/// Fail-open wrapper kept for parity with the other adapters' in-process
/// unit tests; `main.rs` calls [`handle_with_policy`] so it can pass the
/// `PORTZILLA_FAIL_CLOSED` policy.
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, false)
}

/// Handles one `pre_run_command` hook invocation: parses `raw_input`, and
/// if it's a [`PRE_RUN_COMMAND_EVENT`], runs `tool_info.command_line`
/// through [`guard::check`] against `leases`.
///
/// `self_pid` is always `None` when calling into `guard`: the hook fires
/// *before* the command runs, so there is no process yet whose PID this
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
    let input: PreRunCommandInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome::deny_with_reason(
                    "portzilla: could not parse hook input JSON".to_string(),
                );
            }
            return HookOutcome::allow_human_visible_note(format!(
                "portzilla hook windsurf: could not parse hook input JSON, failing open (allow): \
                 {err}"
            ));
        }
    };

    if input.agent_action_name.as_deref() != Some(PRE_RUN_COMMAND_EVENT) {
        // Not a command the kill-guard checks — silent, since the snippet
        // already scopes the hook to `pre_run_command` and this is the
        // expected state for any other event.
        return HookOutcome::allow_silent();
    }

    let self_session = input.trajectory_id;

    let Some(command) = input.tool_info.and_then(|t| t.command_line) else {
        if fail_closed {
            return HookOutcome::deny_with_reason(
                "portzilla: pre_run_command payload had no tool_info.command_line".to_string(),
            );
        }
        return HookOutcome::allow_human_visible_note(
            "portzilla hook windsurf: pre_run_command payload had no tool_info.command_line, \
             failing open (allow)"
                .to_string(),
        );
    };

    match guard::check(&command, leases, None, self_session.as_deref(), checker) {
        Verdict::Allow => HookOutcome::allow_silent(),
        Verdict::Deny { explanation, .. } => HookOutcome::deny_with_reason(explanation),
        Verdict::Warn { explanation } => {
            HookOutcome::allow_human_visible_note(format!("portzilla: {explanation}"))
        }
    }
}

/// The `hooks.json` snippet printed by `portzilla init windsurf`,
/// registering this hook on `pre_run_command`. Format verified against the
/// hooks doc's configuration example (`hooks` map keyed by event, value an
/// array of `{ "command", "show_output" }` objects). Workspace-level
/// (`.windsurf/hooks.json`) is the version-controlled choice; the doc also
/// documents user-level (`~/.codeium/windsurf/hooks.json`) and system-level
/// paths.
pub const CONFIG_SNIPPET: &str = r#"{
  "hooks": {
    "pre_run_command": [
      {
        "command": "portzilla hook windsurf",
        "show_output": true
      }
    ]
  }
}"#;

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

    fn command_input(command: &str) -> String {
        command_input_with_session(command, "trajectory-abc123")
    }

    fn command_input_with_session(command: &str, trajectory_id: &str) -> String {
        serde_json::json!({
            "agent_action_name": "pre_run_command",
            "trajectory_id": trajectory_id,
            "execution_id": "exec_01",
            "timestamp": "2026-01-01T00:00:00Z",
            "tool_info": {
                "command_line": command,
                "cwd": "/home/user/project"
            }
        })
        .to_string()
    }

    #[test]
    fn deny_on_foreign_live_lease_exits_2_with_stderr_reason() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&command_input("kill 1234"), &leases, &AlwaysAlive);

        assert_eq!(outcome.exit_code, 2, "deny must exit 2 (Windsurf's block)");
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
        let outcome = handle(&command_input("kill 9999"), &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_non_kill_command_exits_0_silent() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&command_input("git status"), &leases, &AlwaysAlive);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_dead_lease_exits_0_silent() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&command_input("kill 1234"), &leases, &AlwaysDead);
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout_text.is_none());
    }

    #[test]
    fn non_run_command_event_is_a_silent_no_op() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "agent_action_name": "pre_read_code",
            "trajectory_id": "abc123",
            "tool_info": { "file_path": "/tmp/x" }
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
    fn run_command_missing_command_line_fails_open_with_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "agent_action_name": "pre_run_command",
            "trajectory_id": "abc123",
            "tool_info": { "cwd": "/tmp" }
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
    fn warn_on_unresolvable_process_name_exits_0_with_stderr_warning() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&command_input("pkill node"), &leases, &AlwaysAlive);

        assert_eq!(outcome.exit_code, 0, "warn must not block");
        assert!(outcome.stdout_text.is_none());
        let warning = outcome
            .stderr_note
            .expect("warn must leave a stderr note (human-visible, not model-visible)");
        assert!(warning.contains("node"));
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        // The lease was claimed under the SAME trajectory id that's now
        // trying to kill it. With self_pid always None from this adapter,
        // session is the ONLY way ownership can ever resolve here, so this
        // must Allow.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "shared")];
        let outcome = handle(
            &command_input_with_session("kill 1234", "shared"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(outcome.exit_code, 0, "own-session kill must be allowed");
        assert!(outcome.stdout_text.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn denies_kill_of_a_lease_owned_by_a_different_session() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "theirs")];
        let outcome = handle(
            &command_input_with_session("kill 1234", "mine"),
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
    fn config_snippet_registers_pre_run_command_hook() {
        // No JSON parser assertions needed beyond the documented fields: the
        // snippet is verified against the hooks doc's config example.
        assert!(CONFIG_SNIPPET.contains("\"pre_run_command\""));
        assert!(CONFIG_SNIPPET.contains("\"command\": \"portzilla hook windsurf\""));
        assert!(CONFIG_SNIPPET.contains("\"show_output\""));
    }
}
