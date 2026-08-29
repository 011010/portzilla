//! Gemini CLI adapter for the kill-guard.
//!
//! Owns parsing of Gemini CLI's `BeforeTool` hook payload and rendering of the
//! resulting [`crate::guard::Verdict`] into the JSON shape Gemini CLI expects
//! on stdout. `hook_common::evaluate` delegates normalized command evaluation
//! to the harness-agnostic guard core; payload parsing and wire rendering stay
//! local to this adapter. Thin by design — see `src/guard.rs`'s module doc for
//! why.
//!
//! Schema verified against the current Gemini CLI hooks reference
//! (<https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md>,
//! `docs/hooks/index.md`, and `docs/tools/shell.md`) — see the doc comments
//! below for exactly which fields and behavior that verification covers.
//!
//! # Correction to the assumed premise
//!
//! Hook registration for Gemini CLI does NOT live in a standalone
//! `hooks/hooks.json` inside an extension directory (the initially assumed
//! premise for this adapter). The current reference
//! (`docs/hooks/index.md`, "Configuration" section) is explicit: hooks are
//! configured under a `"hooks"` key inside `settings.json` — project
//! (`.gemini/settings.json`), user (`~/.gemini/settings.json`), system
//! (`/etc/gemini-cli/settings.json`), or an installed extension's own
//! settings contribution (still merged the same way, no separate file
//! format documented). `portzilla init gemini` therefore prints a
//! `.gemini/settings.json` snippet, not a `hooks/hooks.json` one.
//!
//! # Ownership caveat (documented, not silently dropped)
//!
//! `self_session` is set from the hook payload's `session_id`, which IS
//! documented as a base input field for every hook event including
//! `BeforeTool`. But per `docs/tools/shell.md` ("Environment variables"),
//! the `run_shell_command` tool's own subprocess — i.e. the actual shell
//! commands the agent runs, as opposed to the hook script's process — is
//! only ever given `GEMINI_CLI=1`, a bare presence flag with no session
//! identity in it. (`GEMINI_SESSION_ID` exists, but is documented under
//! "Hooks are executed with a sanitized environment" — scoped to hook
//! script subprocesses, not shell-tool subprocesses.) So, same as Cursor: a
//! claim made from inside a Gemini CLI session cannot currently be tagged
//! with the session id this hook receives — foreign-lease protection only,
//! no own-lease recognition. See the README's Kill guard section.
//!
//! # No agent-visible non-blocking channel for `BeforeTool`
//!
//! Unlike Claude Code's `additionalContext` or Cursor's `agent_message` on
//! an "allow" response, Gemini's `BeforeTool` output schema (reference.md,
//! "BeforeTool" section) only documents `decision`/`reason` (deny-only,
//! reason reaching the agent) and `hookSpecificOutput.tool_input` (argument
//! rewriting) as tool-specific fields. There is no documented field that
//! puts free text in front of the agent on a non-blocking path from this
//! specific event. [`Verdict::Warn`] can therefore only reach the *human*
//! here, via the common `systemMessage` output field — the agent never
//! sees it. This is a real capability gap versus the other two adapters,
//! not an oversight; it's called out explicitly in the README.
//!
//! # Fail-open, concretely
//!
//! Per `guard`'s fail-open principle: any failure here resolves to no
//! `decision` field (Gemini's own docs: "Pollution = Failure ... defaults
//! to Allow") plus a stderr diagnostic, never a crash or a `deny`.

use crate::guard::Verdict;
use crate::hook_common::{EvaluationRequest, evaluate};
use crate::lease::{Lease, PidChecker};
use serde::{Deserialize, Serialize};

/// What the caller (`main.rs`) should do with the result of handling one
/// `BeforeTool` hook invocation: print `stdout_json` to stdout, and
/// `stderr_note` to stderr if present.
pub struct HookOutcome {
    pub stdout_json: String,
    pub stderr_note: Option<String>,
}

/// The subset of the Gemini CLI `BeforeTool` hook input JSON this adapter
/// reads. Verified fields: base input (all hooks) is `session_id`,
/// `transcript_path`, `cwd`, `hook_event_name`, `timestamp`; `BeforeTool`
/// adds `tool_name`, `tool_input`, `mcp_context`, `original_request_name`.
/// The shell tool's exact name (`run_shell_command`) and its `tool_input`
/// shape (`command`, `description`, `dir_path`, `is_background`) are
/// verified against `docs/tools/shell.md`. This adapter only needs
/// `session_id`, `tool_name`, and `tool_input.command`.
#[derive(Debug, Deserialize)]
struct BeforeToolInput {
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

/// The exact tool name Gemini CLI uses for shell execution, per
/// `docs/tools/shell.md`'s own heading (`# Shell tool (\`run_shell_command\`)`)
/// and its use as a literal string in the hooks writing guide's own example
/// (`allowed.push('run_shell_command')`).
const SHELL_TOOL_NAME: &str = "run_shell_command";

/// Output JSON shapes, verified against reference.md's `BeforeTool` section
/// and the "Common output fields" table:
/// - Deny: `{"decision": "deny", "reason": "<explanation>"}` — `reason` is
///   "sent to the agent as a tool error, allowing it to respond or retry."
/// - Allow (no concern): no `decision` field at all — an explicit
///   `{"decision": "allow"}` is also valid per the common fields table, but
///   omitting it entirely matches "Preferred for all logic" more literally
///   and avoids over-claiming a documented default we didn't verify.
/// - Warn: `{"systemMessage": "<explanation>"}` — see the module doc
///   comment on why this can only reach the human, not the agent.
#[derive(Debug, Serialize, PartialEq, Default)]
struct HookResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "systemMessage")]
    system_message: Option<String>,
}

/// Handles one `BeforeTool` hook invocation: parses `raw_input`, and if the
/// tool is [`SHELL_TOOL_NAME`], runs its command through
/// [`crate::hook_common::evaluate`]
/// against `leases`.
///
/// `self_pid` is always `None`: `BeforeTool` fires before the tool runs, so
/// there is no process yet whose PID this adapter could offer.
/// `self_session` comes from `session_id` — see the module doc comment for
/// why that only ever protects against foreign kills.
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
    let input: BeforeToolInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome {
                    stdout_json: fail_closed_response("could not parse hook input JSON"),
                    stderr_note: None,
                };
            }
            return HookOutcome {
                stdout_json: empty_response(),
                stderr_note: Some(format!(
                    "portzilla hook gemini: could not parse hook input JSON, failing open (allow): {err}"
                )),
            };
        }
    };

    if input.tool_name.as_deref() != Some(SHELL_TOOL_NAME) {
        return HookOutcome {
            stdout_json: empty_response(),
            stderr_note: None,
        };
    }

    let self_session = input.session_id;

    let Some(command) = input.tool_input.and_then(|t| t.command) else {
        if fail_closed {
            return HookOutcome {
                stdout_json: fail_closed_response(
                    "run_shell_command call had no tool_input.command",
                ),
                stderr_note: None,
            };
        }
        return HookOutcome {
            stdout_json: empty_response(),
            stderr_note: Some(
                "portzilla hook gemini: run_shell_command call had no tool_input.command, failing \
                 open (allow)"
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
        Verdict::Allow => HookResponse::default(),
        Verdict::Deny { explanation, .. } => HookResponse {
            decision: Some("deny"),
            reason: Some(explanation),
            system_message: None,
        },
        Verdict::Warn { explanation } => HookResponse {
            decision: None,
            reason: None,
            system_message: Some(explanation),
        },
    };

    HookOutcome {
        stdout_json: serde_json::to_string(&response).expect("HookResponse always serializes"),
        stderr_note: None,
    }
}

fn empty_response() -> String {
    serde_json::to_string(&HookResponse::default()).expect("HookResponse always serializes")
}

/// Builds the Gemini CLI `BeforeTool` deny JSON for a portzilla-side
/// failure under fail-closed mode. The reason is sent to the agent via
/// `reason` (the `decision`/`reason` pair is documented as the only
/// model-visible channel on a deny). Kept here so the adapter owns the
/// exact byte shape Gemini expects on stdout.
pub fn fail_closed_response(reason: &str) -> String {
    let response = HookResponse {
        decision: Some("deny"),
        reason: Some(reason.to_string()),
        system_message: None,
    };
    serde_json::to_string(&response).expect("HookResponse always serializes")
}

/// The `.gemini/settings.json` snippet printed by `portzilla init gemini`,
/// registering this hook on `BeforeTool` scoped to the shell tool. Format
/// verified against `docs/hooks/index.md`'s own "Configuration schema"
/// example (`hooks.<Event>[].matcher` + `.hooks[].{type,command}`).
pub const SETTINGS_SNIPPET: &str = r#"{
  "hooks": {
    "BeforeTool": [
      {
        "matcher": "run_shell_command",
        "hooks": [
          {
            "name": "portzilla-guard",
            "type": "command",
            "command": "portzilla hook gemini"
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

    fn shell_input(command: &str) -> String {
        shell_input_with_session(command, "sess-abc")
    }

    fn shell_input_with_session(command: &str, session_id: &str) -> String {
        serde_json::json!({
            "session_id": session_id,
            "cwd": "/home/user/project",
            "hook_event_name": "BeforeTool",
            "tool_name": "run_shell_command",
            "tool_input": { "command": command }
        })
        .to_string()
    }

    fn response_json(outcome: &HookOutcome) -> serde_json::Value {
        serde_json::from_str(&outcome.stdout_json).expect("stdout_json must be valid JSON")
    }

    #[test]
    fn deny_on_foreign_live_lease_sets_decision_deny_with_reason() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["decision"], "deny");
        let reason = json["reason"].as_str().expect("reason must be a string");
        assert!(reason.contains("3000"));
        assert!(reason.contains("dev-server"));
    }

    #[test]
    fn allow_on_kill_of_unleased_pid_emits_no_decision() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("kill 9999"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert!(json.get("decision").is_none());
        assert!(json.get("reason").is_none());
    }

    #[test]
    fn allow_on_dead_lease() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shell_input("kill 1234"), &leases, &AlwaysDead);
        assert!(response_json(&outcome).get("decision").is_none());
    }

    #[test]
    fn non_shell_tool_is_a_silent_no_op() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "session_id": "sess-abc",
            "hook_event_name": "BeforeTool",
            "tool_name": "read_file",
            "tool_input": { "path": "/tmp/x" }
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert!(response_json(&outcome).get("decision").is_none());
        assert!(outcome.stderr_note.is_none());
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "sess-mine")];
        let outcome = handle(
            &shell_input_with_session("kill 1234", "sess-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert!(response_json(&outcome).get("decision").is_none());
    }

    #[test]
    fn denies_kill_of_a_lease_owned_by_a_different_session() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "sess-theirs")];
        let outcome = handle(
            &shell_input_with_session("kill 1234", "sess-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(response_json(&outcome)["decision"], "deny");
    }

    #[test]
    fn warn_on_unresolvable_process_name_uses_system_message_only_no_decision() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shell_input("pkill node"), &leases, &AlwaysAlive);

        let json = response_json(&outcome);
        // No documented agent-visible non-blocking channel for BeforeTool —
        // this must not deny, and must not claim a `reason` the agent would
        // never actually see reach it via `decision`.
        assert!(json.get("decision").is_none());
        assert!(json.get("reason").is_none());
        let system_message = json["systemMessage"]
            .as_str()
            .expect("systemMessage must be present");
        assert!(system_message.contains("node"));
    }

    #[test]
    fn forwards_session_id_when_serializing_a_foreign_lease_deny() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "sess-theirs")];
        let outcome = handle(
            &shell_input_with_session("kill 1234", "sess-mine"),
            &leases,
            &AlwaysAlive,
        );

        let json = response_json(&outcome);
        assert_eq!(json["decision"], "deny");
        assert!(json["reason"].as_str().unwrap().contains("3000"));
    }

    #[test]
    fn serializes_warning_as_system_message_without_a_decision() {
        let outcome = handle(&shell_input("pkill node"), &[], &AlwaysAlive);

        let json = response_json(&outcome);
        assert!(json.get("decision").is_none());
        assert!(json.get("reason").is_none());
        assert!(json["systemMessage"].as_str().unwrap().contains("node"));
    }

    #[test]
    fn malformed_json_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle("{ not valid json", &leases, &AlwaysAlive);

        assert!(response_json(&outcome).get("decision").is_none());
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open"))
        );
    }

    #[test]
    fn shell_call_missing_command_fails_open_with_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let input = serde_json::json!({
            "session_id": "sess-abc",
            "hook_event_name": "BeforeTool",
            "tool_name": "run_shell_command",
            "tool_input": {}
        })
        .to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert!(response_json(&outcome).get("decision").is_none());
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn settings_snippet_registers_before_tool_on_the_shell_tool_matcher() {
        let value: serde_json::Value = serde_json::from_str(SETTINGS_SNIPPET)
            .expect("SETTINGS_SNIPPET must itself be valid JSON");
        let hook_entry = &value["hooks"]["BeforeTool"][0];
        assert_eq!(hook_entry["matcher"], "run_shell_command");
        assert_eq!(hook_entry["hooks"][0]["type"], "command");
        assert_eq!(hook_entry["hooks"][0]["command"], "portzilla hook gemini");
    }
}
