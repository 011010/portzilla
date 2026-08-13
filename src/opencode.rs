//! OpenCode adapter for the kill-guard.
//!
//! There are two halves to this adapter, because OpenCode's extensibility
//! model is fundamentally different from the other harnesses': OpenCode
//! hooks are in-process JavaScript/TypeScript plugin modules
//! (<https://opencode.ai/docs/plugins/>), not external processes invoked
//! with a JSON payload on stdin. The other adapters (`claude_code.rs`,
//! `cursor.rs`, `gemini.rs`, `codex.rs`, `kimi.rs`) each translate a
//! harness-owned JSON contract to and from [`crate::guard`]; OpenCode cannot
//! run `portzilla` as the hook directly, so:
//!
//! 1. **The shim** — a small JS plugin (`portzilla init opencode` prints
//!    its full source) that hooks `tool.execute.before`, and shells out to
//!    `portzilla hook opencode` with the command and session id. It turns
//!    a deny into `throw new Error(reason)` (which OpenCode surfaces to the
//!    model as a tool error), and defers warn text to `tool.execute.after`,
//!    where appending to the tool result's `output` puts it in front of the
//!    model without blocking anything (verified: `tool.execute.after`
//!    receives a mutable `{ title, output, metadata }` whose content
//!    becomes the tool result).
//! 2. **This module** — the binary-side verdict protocol the shim calls:
//!    stdin JSON `{ "session_id": ..., "command": ... }`, stdout JSON
//!    `{ "action": "allow" | "deny" | "warn", "reason": ... }`, always exit
//!    0 (exit codes mean nothing to the shim; the verdict is the JSON).
//!
//! # Own-lease recognition: unique among the non-Claude harnesses
//!
//! OpenCode's `shell.env` hook receives the session id and can inject
//! environment variables into every bash subprocess the agent runs
//! (verified: the bash subprocess env is `{ ...process.env, ...extra.env }`
//! where `extra.env` is built from `shell.env` outputs). The shim injects
//! `PORTZILLA_SESSION`, so an agent can claim with
//! `--session "$PORTZILLA_SESSION"` and the guard side (which receives the
//! session id through `tool.execute.before`'s `sessionID` field) recognizes
//! the lease as its own. This is the only non-Claude-Code harness where
//! end-to-end own-lease recognition works today.
//!
//! # No pre-execution model-visible warn channel
//!
//! `tool.execute.before` is binary: return = allow, throw = deny — there is
//! no documented non-blocking pre-execution channel the model sees. The
//! shim therefore surfaces [`Verdict::Warn`] *after* the command ran, by
//! appending to the tool result via `tool.execute.after` (keyed on the
//! call id so the warning lands on the right tool call). This is a
//! deliberate, documented tradeoff: the warning never blocks, and it
//! reaches the model on the very tool result the command produced.
//!
//! # Fail-open, concretely
//!
//! This module prints `{"action":"allow"}` and exit 0 on every
//! portzilla-side failure (unparseable JSON, missing command, unreadable
//! store, panic — see `main.rs`'s `catch_unwind`), with a diagnostic on
//! stderr. The shim also fails open on its own side (any spawn/parse/timeout
//! problem allows), so a portzilla problem can never block an OpenCode
//! bash call. Note that OpenCode itself has no hook timeout — the shim
//! enforces its own (5000 ms) around the subprocess.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure
//! flips from allow to `{"action":"deny","reason":...}` instead; the shim's
//! existing throw-on-deny path then blocks the tool call exactly as a real
//! deny would.

use crate::guard::{self, Verdict};
use crate::lease::{Lease, PidChecker};
use serde::{Deserialize, Serialize};

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_json` to stdout (ALWAYS present — the
/// shim parses the verdict from stdout, so silence must never mean
/// anything), and print `stderr_note` to stderr if present. Exit code is
/// always 0.
pub struct HookOutcome {
    pub stdout_json: String,
    pub stderr_note: Option<String>,
}

/// Builds the verdict JSON for a given action.
fn verdict_json(action: &'static str, reason: Option<String>) -> String {
    let response = VerdictResponse { action, reason };
    serde_json::to_string(&response).expect("VerdictResponse always serializes")
}

/// Builds the deny JSON for a portzilla-side failure under fail-closed
/// mode. Kept here so the adapter owns the exact byte shape the shim
/// parses.
pub fn fail_closed_response(reason: &str) -> String {
    verdict_json("deny", Some(reason.to_string()))
}

/// The subset of the shim's stdin JSON this adapter reads. The shim (not
/// OpenCode's hook payload) owns the translation from OpenCode's input
/// shape, so this contract is portzilla's own and minimal by design:
/// `session_id` (null when absent) and `command`.
#[derive(Debug, Deserialize)]
struct OpenCodeInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

/// Output JSON shape: a flat `action` verdict plus an optional `reason`.
/// The shim recognizes `deny` (throw) and `warn` (defer to after-hook);
/// anything else — including missing/unknown `action` — is treated as
/// allow by the shim.
#[derive(Debug, Serialize, PartialEq)]
struct VerdictResponse {
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Fail-open wrapper kept for parity with the other adapters' in-process
/// unit tests; `main.rs` calls [`handle_with_policy`] so it can pass the
/// `PORTZILLA_FAIL_CLOSED` policy.
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, false)
}

/// Handles one shim invocation: parses `raw_input`, and runs the command
/// through [`guard::check`] against `leases`.
///
/// `self_pid` is always `None` when calling into `guard`: OpenCode's hook
/// fires *before* the tool runs, so there is no process yet whose PID this
/// adapter could pass. Ownership resolves through `self_session`, taken
/// from the shim's `session_id` — which the shim receives from OpenCode's
/// `tool.execute.before` input — and, end to end, requires claims tagged
/// with the same session (`PORTZILLA_SESSION` injected by the shim's
/// `shell.env` hook — see the module doc).
///
/// When `fail_closed` is `true`, every portzilla-side failure (unparseable
/// JSON, missing command, …) returns a deny JSON on stdout instead of
/// allow + stderr note.
pub fn handle_with_policy(
    raw_input: &str,
    leases: &[Lease],
    checker: &dyn PidChecker,
    fail_closed: bool,
) -> HookOutcome {
    let input: OpenCodeInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome {
                    stdout_json: fail_closed_response("could not parse hook input JSON"),
                    stderr_note: None,
                };
            }
            return HookOutcome {
                stdout_json: verdict_json("allow", None),
                stderr_note: Some(format!(
                    "portzilla hook opencode: could not parse hook input JSON, failing open (allow): {err}"
                )),
            };
        }
    };

    let Some(command) = input.command else {
        if fail_closed {
            return HookOutcome {
                stdout_json: fail_closed_response("hook input had no command"),
                stderr_note: None,
            };
        }
        return HookOutcome {
            stdout_json: verdict_json("allow", None),
            stderr_note: Some(
                "portzilla hook opencode: hook input had no command, failing open (allow)"
                    .to_string(),
            ),
        };
    };

    match guard::check(&command, leases, None, input.session_id.as_deref(), checker) {
        Verdict::Allow => HookOutcome {
            stdout_json: verdict_json("allow", None),
            stderr_note: None,
        },
        Verdict::Deny { explanation, .. } => HookOutcome {
            stdout_json: verdict_json("deny", Some(explanation)),
            stderr_note: None,
        },
        Verdict::Warn { explanation } => HookOutcome {
            stdout_json: verdict_json("warn", Some(explanation)),
            stderr_note: None,
        },
    }
}

/// The full source of the OpenCode plugin shim, printed by `portzilla init
/// opencode`. Save it as `.opencode/plugin/portzilla.js` (project) or
/// `~/.config/opencode/plugin/portzilla.js` (user) — OpenCode
/// auto-discovers `*.js`/`*.ts` files in those directories. The plugin
/// exports a function returning hooks (the documented plugin shape) and
/// uses Bun's `$` shell API (provided in the plugin context) with its own
/// 5000 ms timeout, since OpenCode applies no timeout to plugin hooks.
pub const PLUGIN_SNIPPET: &str = r#"// portzilla kill-guard for opencode.
// Save as .opencode/plugin/portzilla.js (project) or
// ~/.config/opencode/plugin/portzilla.js (user) — opencode auto-discovers
// *.js/*.ts files in those directories. Requires `portzilla` on PATH.
// Restart opencode after saving for the plugin to load.
export default async ({ $ }) => {
  const CHECK_TIMEOUT_MS = 5000;

  // callID -> warn reason, surfaced by tool.execute.after so the model
  // sees it on the tool result without the command being blocked.
  const pendingWarnings = new Map();

  const checkCommand = async (command, sessionID) => {
    const payload = JSON.stringify({ session_id: sessionID ?? null, command });
    try {
      const result = await $({ timeout: CHECK_TIMEOUT_MS, nothrow: true })`portzilla hook opencode`.input(payload);
      if (result.exitCode !== 0) return { action: "allow" };
      const text = new TextDecoder().decode(result.stdout ?? new Uint8Array());
      const parsed = JSON.parse(text);
      if (parsed.action === "deny" || parsed.action === "warn") return parsed;
      return { action: "allow" };
    } catch (err) {
      console.error(`portzilla: guard check failed, allowing (${err})`);
      return { action: "allow" };
    }
  };

  return {
    "tool.execute.before": async (input, output) => {
      if (input.tool !== "bash") return;
      const command = output.args?.command;
      if (typeof command !== "string" || command.trim() === "") return;
      const verdict = await checkCommand(command, input.sessionID);
      if (verdict.action === "deny") throw new Error(verdict.reason);
      if (verdict.action === "warn") pendingWarnings.set(input.callID, verdict.reason);
    },
    "shell.env": async (input, output) => {
      if (input.sessionID) output.env.PORTZILLA_SESSION = input.sessionID;
    },
    "tool.execute.after": async (input, output) => {
      if (input.tool !== "bash") return;
      const reason = pendingWarnings.get(input.callID);
      if (reason) {
        pendingWarnings.delete(input.callID);
        if (typeof output.output === "string") {
          output.output += "\n\nportzilla warning: " + reason;
        }
      }
    },
  };
};"#;

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

    fn shim_input(command: &str) -> String {
        shim_input_with_session(command, "abc123")
    }

    fn shim_input_with_session(command: &str, session_id: &str) -> String {
        serde_json::json!({
            "session_id": session_id,
            "command": command
        })
        .to_string()
    }

    fn response_json(outcome: &HookOutcome) -> serde_json::Value {
        serde_json::from_str(&outcome.stdout_json).expect("stdout_json must be valid JSON")
    }

    #[test]
    fn deny_on_foreign_live_lease_emits_deny_action_with_reason() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shim_input("kill 1234"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["action"], "deny");
        let reason = json["reason"].as_str().expect("reason must be a string");
        assert!(reason.contains("3000"));
        assert!(reason.contains("dev-server"));
    }

    #[test]
    fn allow_on_kill_of_unleased_pid_emits_allow_action() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shim_input("kill 9999"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["action"], "allow");
        assert!(json.get("reason").is_none());
    }

    #[test]
    fn allow_on_non_kill_command_emits_allow_action() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shim_input("git status"), &leases, &AlwaysAlive);
        assert_eq!(response_json(&outcome)["action"], "allow");
    }

    #[test]
    fn allow_on_dead_lease_emits_allow_action() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&shim_input("kill 1234"), &leases, &AlwaysDead);
        assert_eq!(response_json(&outcome)["action"], "allow");
    }

    #[test]
    fn malformed_json_fails_open_with_allow_action_and_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle("{ not valid json", &leases, &AlwaysAlive);

        assert_eq!(
            response_json(&outcome)["action"],
            "allow",
            "malformed input must fail open (never deny)"
        );
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open")),
            "malformed input should still leave a diagnostic trail"
        );
    }

    #[test]
    fn missing_command_fails_open_with_allow_action_and_a_stderr_note() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({ "session_id": "abc123" }).to_string();

        let outcome = handle(&input, &leases, &AlwaysAlive);
        assert_eq!(response_json(&outcome)["action"], "allow");
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn empty_stdin_fails_open_with_allow_action_and_a_stderr_note() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle("", &leases, &AlwaysAlive);
        assert_eq!(response_json(&outcome)["action"], "allow");
        assert!(outcome.stderr_note.is_some());
    }

    #[test]
    fn warn_on_unresolvable_process_name_emits_warn_action() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle(&shim_input("pkill node"), &leases, &AlwaysAlive);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["action"], "warn");
        let reason = json["reason"].as_str().expect("reason must be a string");
        assert!(reason.contains("node"));
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        // The lease was claimed under the SAME session that's now trying to
        // kill it — the end-to-end path enabled by the shim's PORTZILLA_SESSION
        // injection. With self_pid always None from this adapter, session is
        // the ONLY way ownership can ever resolve here, so this must Allow.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-mine")];
        let outcome = handle(
            &shim_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(
            response_json(&outcome)["action"],
            "allow",
            "own-session kill must be allowed"
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
            &shim_input_with_session("kill 1234", "session-mine"),
            &leases,
            &AlwaysAlive,
        );
        assert_eq!(response_json(&outcome)["action"], "deny");
    }

    #[test]
    fn fail_closed_policy_denies_on_malformed_json() {
        let leases: Vec<Lease> = vec![];
        let outcome = handle_with_policy("{ not valid json", &leases, &AlwaysAlive, true);

        assert!(outcome.stderr_note.is_none());
        let json = response_json(&outcome);
        assert_eq!(json["action"], "deny");
        assert!(
            json["reason"]
                .as_str()
                .expect("reason must be a string")
                .contains("could not parse hook input JSON")
        );
    }

    #[test]
    fn plugin_snippet_is_a_valid_js_module_with_all_three_hooks() {
        // The shim is the load-bearing half of this adapter: these are the
        // exact strings OpenCode's plugin loader and the guard protocol
        // depend on. The snippet must export a function (plugin shape),
        // hook all three events, shell out to the binary, throw on deny,
        // and inject the session id.
        assert!(PLUGIN_SNIPPET.contains("export default async"));
        assert!(PLUGIN_SNIPPET.contains("\"tool.execute.before\""));
        assert!(PLUGIN_SNIPPET.contains("\"tool.execute.after\""));
        assert!(PLUGIN_SNIPPET.contains("\"shell.env\""));
        assert!(PLUGIN_SNIPPET.contains("portzilla hook opencode"));
        assert!(PLUGIN_SNIPPET.contains("throw new Error"));
        assert!(PLUGIN_SNIPPET.contains("PORTZILLA_SESSION"));
        assert!(PLUGIN_SNIPPET.contains("input.tool !== \"bash\""));
    }
}
