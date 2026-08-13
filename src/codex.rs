//! Codex CLI adapter for the kill-guard.
//!
//! Translates a Codex CLI `PreToolUse` hook payload into a call to the
//! harness-agnostic [`crate::guard`], and translates the resulting
//! [`crate::guard::Verdict`] back into the JSON shape Codex expects on
//! stdout. Thin by design — see `src/guard.rs`'s module doc for why.
//!
//! Schema verified against the current Codex hooks reference
//! (<https://developers.openai.com/codex/hooks>, GA since May 2026 per
//! <https://developers.openai.com/codex/whats-new>) — see the doc comments
//! below for exactly which fields and behavior that verification covers.
//! Codex's hook contract is deliberately close to Claude Code's: same
//! stdin-JSON transport, same `hookSpecificOutput.permissionDecision`
//! response shape, same exit-0-with-empty-stdout allow semantics.
//!
//! # Ownership caveat (documented, not silently dropped)
//!
//! `self_session` is set from the hook payload's `session_id`, which IS a
//! documented base input field for every hook event including `PreToolUse`.
//! But per the Codex environment-variables reference
//! (<https://developers.openai.com/codex/config-file/environment-variables>),
//! no documented variable exposes that session id to the shell commands the
//! agent itself runs (as opposed to the hook script's process). So, same as
//! Cursor and Gemini CLI: a claim made from inside a Codex session cannot
//! currently be tagged with the session id this hook receives —
//! foreign-lease protection only, no own-lease recognition. See the
//! README's Kill guard section.
//!
//! # Coverage gap: `exec_command`
//!
//! The hooks reference documents that "Unified exec (`exec_command`)" calls
//! are also observable by `PreToolUse`/`PostToolUse` hooks, but does not
//! document which matcher string or `tool_input` shape those calls carry
//! (only "`Bash` and `apply_patch` use `tool_input.command`"). This adapter
//! therefore scopes itself to the verified `Bash` tool name only; whether
//! `exec_command` kills reach this hook (and with what payload shape) is an
//! open verification item, documented rather than assumed.
//!
//! # Fail-open, concretely
//!
//! Per `guard`'s fail-open principle: this adapter treats every failure
//! mode (unparseable JSON, missing fields, an unreadable lease store, even
//! an internal panic — see `main.rs`'s `catch_unwind` around the call into
//! this module) the same way — print nothing to stdout and exit 0. Per the
//! Codex hook contract, exit 0 with no output means "success/continue,"
//! i.e. allow. A diagnostic goes to stderr for troubleshooting.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure
//! flips from "allow + note" to "deny + reason" instead, via
//! [`fail_closed_response`] and the same `permissionDecisionReason` field
//! Codex forwards to the model on a deny.

use crate::guard::{self, Verdict};
use crate::lease::{Lease, PidChecker};
use serde::{Deserialize, Serialize};

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_json` to stdout if present (nothing if
/// `None` — see the fail-open note above), and print `stderr_note` to
/// stderr if present. Both are independent; either, both, or neither may be
/// set. Exit code is always 0 — the caller must never turn this into a
/// nonzero exit for a `PreToolUse` hook, since Codex also honors
/// "exit 2 + stderr" as a deny and we must not trip that accidentally.
pub struct HookOutcome {
    pub stdout_json: Option<String>,
    pub stderr_note: Option<String>,
}

impl HookOutcome {
    fn allow_silent() -> Self {
        Self {
            stdout_json: None,
            stderr_note: None,
        }
    }

    fn allow_with_note(note: impl Into<String>) -> Self {
        Self {
            stdout_json: None,
            stderr_note: Some(note.into()),
        }
    }

    fn deny_with_reason(reason: String) -> Self {
        Self {
            stdout_json: Some(fail_closed_response(&reason)),
            stderr_note: None,
        }
    }
}

/// Builds the Codex `PreToolUse` deny JSON for a portzilla-side failure
/// under fail-closed mode. The reason is sent to the agent via
/// `permissionDecisionReason`, the field Codex forwards to the model when
/// a tool call is denied. Kept here (rather than in `main.rs`) so the
/// adapter owns the exact byte shape Codex expects on stdout.
pub fn fail_closed_response(reason: &str) -> String {
    let response = HookResponse {
        system_message: None,
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PreToolUse",
            permission_decision: Some("deny"),
            permission_decision_reason: Some(reason.to_string()),
            additional_context: None,
        },
    };
    serde_json::to_string(&response).expect("HookResponse always serializes")
}

/// The subset of the Codex `PreToolUse` hook input JSON this adapter reads.
/// Verified fields (per the hooks reference's common-input table):
/// `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `model`,
/// `permission_mode`; `PreToolUse` adds `tool_name`, `tool_use_id`, and
/// `tool_input` (with `command` for `Bash`). This adapter needs
/// `tool_name`, `tool_input.command`, and `session_id`; every other field
/// is ignored via `#[serde(default)]` tolerance rather than declared, so
/// payload fields we don't use (or future additions) never cause a parse
/// failure.
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

/// Output JSON shape, verified against the hooks reference's `PreToolUse`
/// output section:
/// ```json
/// {
///   "hookSpecificOutput": {
///     "hookEventName": "PreToolUse",
///     "permissionDecision": "deny",
///     "permissionDecisionReason": "..."
///   }
/// }
/// ```
/// plus the documented `additionalContext` (non-blocking, "added as extra
/// developer context" — i.e. model-visible) and top-level `systemMessage`
/// ("surfaced as a warning in the UI or event stream" — human-visible)
/// fields, used together for [`Verdict::Warn`].
#[derive(Debug, Serialize, PartialEq)]
struct HookResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "systemMessage")]
    system_message: Option<String>,
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Serialize, PartialEq)]
struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "permissionDecision")]
    permission_decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "additionalContext")]
    additional_context: Option<String>,
}

/// Fail-open wrapper kept for parity with the other adapters' in-process
/// unit tests; `main.rs` calls [`handle_with_policy`] so it can pass the
/// `PORTZILLA_FAIL_CLOSED` policy.
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, false)
}

/// Handles one `PreToolUse` hook invocation: parses `raw_input`, and if
/// it's a `Bash` tool call, runs its command through [`guard::check`]
/// against `leases`.
///
/// `self_pid` is always `None` when calling into `guard`: `PreToolUse`
/// fires *before* the tool runs, so there is no process yet whose PID this
/// adapter could pass as "the process about to run this." Ownership
/// therefore resolves entirely through `self_session` — see the module doc
/// comment for why that only ever protects against foreign kills today.
///
/// When `fail_closed` is `true`, every portzilla-side failure (unparseable
/// JSON, missing command, …) returns a deny shape on stdout instead of
/// allow + stderr note.
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
                    "could not parse hook input JSON".to_string(),
                );
            }
            return HookOutcome::allow_with_note(format!(
                "portzilla hook codex: could not parse hook input JSON, failing open (allow): {err}"
            ));
        }
    };

    if input.tool_name.as_deref() != Some("Bash") {
        // Not a Bash call — nothing for the kill-guard to check. Silent,
        // since this is the overwhelmingly common case (every non-Bash
        // tool use fires this hook too, if the matcher isn't scoped) and
        // not worth a diagnostic line. See the module doc for the
        // documented `exec_command` coverage gap.
        return HookOutcome::allow_silent();
    }

    let self_session = input.session_id;

    let Some(command) = input.tool_input.and_then(|t| t.command) else {
        if fail_closed {
            return HookOutcome::deny_with_reason(
                "Bash tool call had no tool_input.command".to_string(),
            );
        }
        return HookOutcome::allow_with_note(
            "portzilla hook codex: Bash tool call had no tool_input.command, failing open (allow)"
                .to_string(),
        );
    };

    match guard::check(&command, leases, None, self_session.as_deref(), checker) {
        Verdict::Allow => HookOutcome::allow_silent(),
        Verdict::Deny { explanation, .. } => {
            let response = HookResponse {
                system_message: None,
                hook_specific_output: HookSpecificOutput {
                    hook_event_name: "PreToolUse",
                    permission_decision: Some("deny"),
                    permission_decision_reason: Some(explanation),
                    additional_context: None,
                },
            };
            HookOutcome {
                stdout_json: Some(
                    serde_json::to_string(&response).expect("HookResponse always serializes"),
                ),
                stderr_note: None,
            }
        }
        Verdict::Warn { explanation } => {
            let response = HookResponse {
                system_message: Some(explanation.clone()),
                hook_specific_output: HookSpecificOutput {
                    hook_event_name: "PreToolUse",
                    permission_decision: None,
                    permission_decision_reason: None,
                    additional_context: Some(explanation),
                },
            };
            HookOutcome {
                stdout_json: Some(
                    serde_json::to_string(&response).expect("HookResponse always serializes"),
                ),
                stderr_note: None,
            }
        }
    }
}

/// The `.codex/hooks.json` snippet printed by `portzilla init codex`,
/// registering this hook on `PreToolUse` for the `Bash` tool. Format
/// verified against the hooks reference's registration section: hooks are
/// discovered as `hooks.json` files next to active config layers
/// (`~/.codex/hooks.json` user-level, `<repo>/.codex/hooks.json`
/// project-level), shaped as event → matcher group (regex on tool name) →
/// handlers (`type: "command"`, `command`).
pub const HOOKS_SNIPPET: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "portzilla hook codex"
          }
        ]
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

    fn bash_input(command: &str) -> String {
        bash_input_with_session(command, "abc123")
    }

    fn bash_input_with_session(command: &str, session_id: &str) -> String {
        serde_json::json!({
            "session_id": session_id,
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_use_id": "call_01",
            "permission_mode": "default"
        })
        .to_string()
    }

    #[test]
    fn deny_on_foreign_live_lease_emits_permission_decision_deny() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&bash_input("kill 1234"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json: serde_json::Value =
            serde_json::from_str(&outcome.stdout_json.expect("deny must produce stdout JSON"))
                .unwrap();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("permissionDecisionReason must be a string");
        assert!(reason.contains("3000"));
        assert!(reason.contains("dev-server"));
        // Deny must not also carry a systemMessage/additionalContext.
        assert!(json.get("systemMessage").is_none());
        assert!(
            json["hookSpecificOutput"]
                .get("additionalContext")
                .is_none()
        );
    }

    #[test]
    fn allow_on_kill_of_unleased_pid_emits_nothing() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&bash_input("kill 9999"), &leases, &AlwaysAlive);
        assert!(outcome.stdout_json.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_non_kill_command_emits_nothing() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&bash_input("git status"), &leases, &AlwaysAlive);
        assert!(outcome.stdout_json.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allow_on_dead_lease_emits_nothing() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&bash_input("kill 1234"), &leases, &AlwaysDead);
        assert!(outcome.stdout_json.is_none());
    }

    #[test]
    fn non_bash_tool_is_a_silent_no_op() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "session_id": "abc123",
            "hook_event_name": "PreToolUse",
            "tool_name": "apply_patch",
            "tool_input": { "command": "*** Begin Patch" }
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert!(outcome.stdout_json.is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn malformed_json_fails_open_with_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle("{ not valid json", &leases, &AlwaysAlive);

        assert!(
            outcome.stdout_json.is_none(),
            "malformed input must fail open (no blocking output)"
        );
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open")),
            "malformed input should still leave a diagnostic trail"
        );
    }

    #[test]
    fn bash_call_missing_command_fails_open_with_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "session_id": "abc123",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {}
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert!(outcome.stdout_json.is_none());
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn empty_stdin_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle("", &leases, &AlwaysAlive);
        assert!(outcome.stdout_json.is_none());
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn warn_on_unresolvable_process_name_emits_additional_context_and_system_message() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&bash_input("pkill node"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json: serde_json::Value =
            serde_json::from_str(&outcome.stdout_json.expect("warn must produce stdout JSON"))
                .unwrap();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(
            json["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        let additional_context = json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext must be a string");
        assert!(additional_context.contains("node"));
        let system_message = json["systemMessage"]
            .as_str()
            .expect("systemMessage must be a string");
        assert!(system_message.contains("node"));
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        // The lease was claimed under the SAME session that's now trying to
        // kill it. With self_pid always None from this adapter, session is
        // the ONLY way ownership can ever resolve here, so this must Allow.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-mine")];
        let outcome = handle(
            &bash_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert!(
            outcome.stdout_json.is_none(),
            "own-session kill must be allowed (silent)"
        );
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
            &bash_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        let json: serde_json::Value = serde_json::from_str(
            &outcome
                .stdout_json
                .expect("foreign-session kill must be denied"),
        )
        .unwrap();
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn fail_closed_policy_denies_on_malformed_json() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle_with_policy("{ not valid json", &leases, &AlwaysAlive, true);

        assert!(outcome.stderr_note.is_none());
        let json: serde_json::Value = serde_json::from_str(
            &outcome
                .stdout_json
                .expect("fail-closed must produce a deny shape"),
        )
        .unwrap();
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            json["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .expect("reason must be a string")
                .contains("could not parse hook input JSON")
        );
    }

    #[test]
    fn hooks_snippet_registers_pretooluse_on_the_bash_matcher() {
        let value: serde_json::Value =
            serde_json::from_str(HOOKS_SNIPPET).expect("HOOKS_SNIPPET must itself be valid JSON");
        let hook_entry = &value["hooks"]["PreToolUse"][0];
        assert_eq!(hook_entry["matcher"], "Bash");
        assert_eq!(hook_entry["hooks"][0]["type"], "command");
        assert_eq!(hook_entry["hooks"][0]["command"], "portzilla hook codex");
    }
}
