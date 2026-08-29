//! Cursor adapter for the kill-guard.
//!
//! Owns parsing of Cursor's `beforeShellExecution` hook payload and rendering
//! of the resulting [`crate::guard::Verdict`] into the JSON shape Cursor
//! expects on stdout. `hook_common::evaluate` delegates normalized command
//! evaluation to the harness-agnostic guard core; payload parsing and wire
//! rendering stay local to this adapter. Thin by design — see `src/guard.rs`'s
//! module doc for why.
//!
//! Schema verified against the current Cursor hooks documentation
//! (<https://cursor.com/docs/agent/hooks>, "Common schema" and
//! "beforeShellExecution" sections) — see the doc comments below for
//! exactly which fields and behavior that verification covers.
//!
//! # Ownership caveat (documented, not silently dropped)
//!
//! `self_session` is set from the hook payload's `conversation_id`. The
//! docs' own "Environment Variables" section for hooks
//! (<https://cursor.com/docs/agent/hooks#environment-variables>) is
//! explicitly scoped to variables the HOOK SCRIPT receives (`CURSOR_PROJECT_DIR`,
//! `CURSOR_VERSION`, `CURSOR_USER_EMAIL`, `CURSOR_TRANSCRIPT_PATH`,
//! `CURSOR_CODE_REMOTE`, `CLAUDE_PROJECT_DIR`) — none of them carry
//! `conversation_id`, and no other documented mechanism exposes it to the
//! shell commands Cursor's agent actually executes. This means a claim made
//! from inside a Cursor session cannot currently be tagged with the
//! conversation id that would later let `hook_common::evaluate` recognize it
//! as "your own" — Cursor gets foreign-lease protection (deny) but not
//! own-lease recognition (allow), until Cursor documents such a variable.
//! See the README's Kill guard section for the user-facing version of this.
//!
//! # Fail-open, concretely
//!
//! Per `guard`'s fail-open principle (see `src/guard.rs`): any failure here
//! (unparseable payload, missing fields, an unreadable lease store, an
//! internal panic — see `main.rs`'s `catch_unwind`) resolves to
//! `{"permission": "allow"}` plus a stderr diagnostic, never a crash or a
//! deny. The docs confirm Cursor's own hook runner already fails open by
//! default on hook failure (crash/timeout/invalid JSON) unless
//! `failClosed: true` is set on the hook definition — this adapter's
//! internal fail-open handling is defense in depth on top of that, not a
//! substitute for it (we never ask users to set `failClosed`).

use crate::guard::Verdict;
use crate::hook_common::{EvaluationRequest, evaluate};
use crate::lease::{Lease, PidChecker};
use serde::{Deserialize, Serialize};

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation. Unlike the Claude Code adapter, Cursor's own reference
/// examples always emit an explicit JSON response (rather than relying on
/// "empty stdout means allow"), so `stdout_json` here is never optional.
pub struct HookOutcome {
    pub stdout_json: String,
    pub stderr_note: Option<String>,
}

/// The subset of Cursor's `beforeShellExecution` hook input JSON this
/// adapter reads. Verified fields: the "Common schema" input (all hooks)
/// includes `conversation_id`, `generation_id`, `model`, `model_id`,
/// `model_params`, `hook_event_name`, `cursor_version`, `workspace_roots`,
/// `user_email`, `transcript_path`; the `beforeShellExecution`-specific
/// input adds `command`, `cwd`, `sandbox`. This adapter only needs
/// `command` and `conversation_id`; every other field is ignored via
/// `#[serde(default)]` tolerance rather than declared.
#[derive(Debug, Deserialize)]
struct BeforeShellExecutionInput {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
}

/// Output JSON shape, verified against the docs' own `beforeShellExecution`
/// / `beforeMCPExecution` schema block:
/// ```json
/// {
///   "permission": "allow" | "deny" | "ask",
///   "user_message": "<message shown in client>",
///   "agent_message": "<message sent to agent>"
/// }
/// ```
/// Unlike some other Cursor hook events (whose docs annotate `user_message`/
/// `agent_message` as "shown/sent ... when denied"), `beforeShellExecution`'s
/// own schema block carries no such qualifier.
///
/// That is as far as the verification goes, and it's worth being precise
/// about what it does and doesn't establish: the ABSENCE of a "when denied"
/// qualifier is not the same as documentation POSITIVELY CONFIRMING
/// `agent_message` is delivered to the agent on a `permission: "allow"`
/// response — it just means nothing in the docs rules that out for this
/// event specifically. [`Verdict::Warn`] is implemented on that basis (see
/// `handle` below), on the reasoning that it's the best-documented option
/// available and harmless if wrong (a dropped `agent_message` on `allow`
/// degrades to the command just running with no warning shown anywhere but
/// `user_message`, not a safety regression). This still needs a live Cursor
/// smoke test to positively confirm delivery — not done as part of this
/// change.
#[derive(Debug, Serialize, PartialEq)]
struct HookResponse {
    permission: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_message: Option<String>,
}

/// Handles one `beforeShellExecution` hook invocation: parses `raw_input`
/// and runs the command through [`crate::hook_common::evaluate`] against
/// `leases`.
///
/// `self_pid` is always `None`: `beforeShellExecution` fires before the
/// command runs, so there is no process yet whose PID this adapter could
/// offer. `self_session` comes from `conversation_id` — see the module doc
/// comment for why that only ever protects against foreign kills, and
/// cannot currently recognize a claim as "your own" (Cursor doesn't expose
/// `conversation_id` to the agent's shell commands).
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, false)
}

/// Same as [`handle`] but with the explicit fail-closed policy. When
/// `fail_closed` is `true`, every portzilla-side failure (unparseable JSON,
/// missing command, …) returns a deny shape on stdout instead of allow +
/// stderr note.
pub fn handle_with_policy(
    raw_input: &str,
    leases: &[Lease],
    checker: &dyn PidChecker,
    fail_closed: bool,
) -> HookOutcome {
    let input: BeforeShellExecutionInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome {
                    stdout_json: fail_closed_response("could not parse hook input JSON"),
                    stderr_note: None,
                };
            }
            return HookOutcome {
                stdout_json: allow_response(),
                stderr_note: Some(format!(
                    "portzilla hook cursor: could not parse hook input JSON, failing open (allow): {err}"
                )),
            };
        }
    };

    let self_session = input.conversation_id;

    let Some(command) = input.command else {
        if fail_closed {
            return HookOutcome {
                stdout_json: fail_closed_response("hook input had no command"),
                stderr_note: None,
            };
        }
        return HookOutcome {
            stdout_json: allow_response(),
            stderr_note: Some(
                "portzilla hook cursor: hook input had no command, failing open (allow)"
                    .to_string(),
            ),
        };
    };

    let response = match evaluate(EvaluationRequest {
        command: &command,
        session: self_session.as_deref(),
        leases,
        checker,
    }) {
        Verdict::Allow => HookResponse {
            permission: "allow",
            user_message: None,
            agent_message: None,
        },
        Verdict::Deny {
            port,
            tag,
            explanation,
            ..
        } => HookResponse {
            permission: "deny",
            user_message: Some(format!(
                "portzilla: blocked — port {port} (\"{tag}\") is owned by another session"
            )),
            agent_message: Some(explanation),
        },
        Verdict::Warn { explanation } => HookResponse {
            permission: "allow",
            user_message: Some("portzilla: could not verify this command is safe".to_string()),
            agent_message: Some(explanation),
        },
    };

    HookOutcome {
        stdout_json: serde_json::to_string(&response).expect("HookResponse always serializes"),
        stderr_note: None,
    }
}

fn allow_response() -> String {
    serde_json::to_string(&HookResponse {
        permission: "allow",
        user_message: None,
        agent_message: None,
    })
    .expect("HookResponse always serializes")
}

/// Builds the Cursor `beforeShellExecution` deny JSON for a portzilla-side
/// failure under fail-closed mode. Same shape as a normal deny, with a
/// short user-visible `user_message` and the longer reason in
/// `agent_message` (which the docs describe as "sent to agent"). Kept here
/// so the adapter owns the exact byte shape Cursor expects on stdout.
pub fn fail_closed_response(reason: &str) -> String {
    let response = HookResponse {
        permission: "deny",
        user_message: Some(format!(
            "portzilla: blocked — could not verify this command's safety ({reason})."
        )),
        agent_message: Some(reason.to_string()),
    };
    serde_json::to_string(&response).expect("HookResponse always serializes")
}

/// The `.cursor/hooks.json` snippet printed by `portzilla init cursor`,
/// registering this hook on `beforeShellExecution`. Format verified against
/// the docs' own worked example (`version: 1`, `hooks.beforeShellExecution`
/// array of `{ command: ... }` entries).
pub const HOOKS_SNIPPET: &str = r#"{
  "version": 1,
  "hooks": {
    "beforeShellExecution": [
      {
        "command": "portzilla hook cursor"
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

    fn shell_input(command: &str) -> String {
        shell_input_with_conversation(command, "conv-abc")
    }

    fn shell_input_with_conversation(command: &str, conversation_id: &str) -> String {
        serde_json::json!({
            "command": command,
            "cwd": "/home/user/project",
            "sandbox": false,
            "conversation_id": conversation_id,
            "generation_id": "gen-1",
            "model": "test-model",
            "workspace_roots": ["/home/user/project"],
            "user_email": "user@example.com"
        })
        .to_string()
    }

    fn response_json(outcome: &HookOutcome) -> serde_json::Value {
        serde_json::from_str(&outcome.stdout_json).expect("stdout_json must be valid JSON")
    }

    #[test]
    fn deny_on_foreign_live_lease_sets_permission_deny_with_agent_message() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["permission"], "deny");
        let agent_message = json["agent_message"]
            .as_str()
            .expect("agent_message must be a string");
        assert!(agent_message.contains("3000"));
        assert!(agent_message.contains("dev-server"));
        assert!(json["user_message"].as_str().unwrap().contains("3000"));
    }

    #[test]
    fn allow_on_kill_of_unleased_pid_sets_permission_allow_with_no_messages() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("kill 9999"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["permission"], "allow");
        assert!(json.get("user_message").is_none());
        assert!(json.get("agent_message").is_none());
    }

    #[test]
    fn allow_on_dead_lease() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysDead);
        assert_eq!(response_json(&outcome)["permission"], "allow");
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_conversation() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "conv-mine")];
        let outcome = handle(
            &shell_input_with_conversation("kill 1234", "conv-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(response_json(&outcome)["permission"], "allow");
    }

    #[test]
    fn denies_kill_of_a_lease_owned_by_a_different_conversation() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "conv-theirs")];
        let outcome = handle(
            &shell_input_with_conversation("kill 1234", "conv-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(response_json(&outcome)["permission"], "deny");
    }

    #[test]
    fn warn_on_unresolvable_process_name_allows_with_agent_message_carrying_the_warning() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("pkill node"), &leases, &AlwaysAlive);

        let json = response_json(&outcome);
        // beforeShellExecution's agent_message is not documented as
        // deny-only (see the HookResponse doc comment for what that does
        // and doesn't establish — delivery on "allow" is not positively
        // confirmed, just not ruled out), so a Warn is implemented to carry
        // the explanation while still allowing the command through.
        assert_eq!(json["permission"], "allow");
        let agent_message = json["agent_message"].as_str().unwrap();
        assert!(agent_message.contains("node"));
        assert!(!json["user_message"].as_str().unwrap().is_empty());
    }

    #[test]
    fn forwards_conversation_id_when_serializing_a_foreign_lease_deny() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "conv-theirs")];
        let outcome = handle(
            &shell_input_with_conversation("kill 1234", "conv-mine"),
            &leases,
            &AlwaysAlive,
        );

        let json = response_json(&outcome);
        assert_eq!(json["permission"], "deny");
        assert!(json["agent_message"].as_str().unwrap().contains("3000"));
    }

    #[test]
    fn serializes_warning_as_an_allow_with_both_messages() {
        let outcome = handle(&shell_input("pkill node"), &[], &AlwaysAlive);

        let json = response_json(&outcome);
        assert_eq!(json["permission"], "allow");
        assert!(json["user_message"].as_str().is_some());
        assert!(json["agent_message"].as_str().unwrap().contains("node"));
    }

    #[test]
    fn malformed_json_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle("{ not valid json", &leases, &AlwaysAlive);

        assert_eq!(response_json(&outcome)["permission"], "allow");
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open"))
        );
    }

    #[test]
    fn missing_command_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let input = serde_json::json!({"conversation_id": "conv-1"}).to_string();
        let outcome = handle(&input, &leases, &AlwaysAlive);

        assert_eq!(response_json(&outcome)["permission"], "allow");
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn hooks_snippet_registers_before_shell_execution() {
        let value: serde_json::Value =
            serde_json::from_str(HOOKS_SNIPPET).expect("HOOKS_SNIPPET must itself be valid JSON");
        assert_eq!(value["version"], 1);
        assert_eq!(
            value["hooks"]["beforeShellExecution"][0]["command"],
            "portzilla hook cursor"
        );
    }
}
