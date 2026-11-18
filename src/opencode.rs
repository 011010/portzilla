//! OpenCode adapter: kill-guard for shell commands, file intents for edits.
//!
//! There are two halves to this adapter, because OpenCode's extensibility
//! model is fundamentally different from the other harnesses': OpenCode
//! hooks are in-process JavaScript/TypeScript plugin modules
//! (<https://opencode.ai/docs/plugins/>), not external processes invoked
//! with a JSON payload on stdin. The other adapters (`claude_code.rs`,
//! `codex.rs`, `cursor.rs`, `gemini.rs`, `kimi.rs`, `windsurf.rs`) each own
//! their harness payload parsing and wire rendering. All seven use
//! `hook_common::evaluate`, which delegates normalized command evaluation to
//! the guard core; OpenCode cannot run `portzilla` as the hook directly, so:
//!
//! 1. **The shim** — a small JS plugin (`portzilla init opencode` prints
//!    its full source) in OpenCode's V2 plugin shape (`id` + `setup`). It
//!    registers `tool.execute.before`, translates `write`/`edit` into a
//!    file-intent check and `shell` into a kill-guard check, shells out to
//!    `portzilla hook opencode`, and turns a deny into
//!    `throw new Error(reason)` (which OpenCode surfaces to the model as a
//!    tool error; the tool does not run). See [`PLUGIN_SNIPPET`].
//! 2. **This module** — the binary-side verdict protocol the shim calls:
//!    stdin JSON `{ "session_id": ..., "command": ... }` for a shell command
//!    or `{ "session_id": ..., "file": ..., "cwd": ... }` for an edit,
//!    stdout JSON `{ "action": "allow" | "deny" | "warn", "reason": ... }`,
//!    always exit 0 (exit codes mean nothing to the shim; the verdict is the
//!    JSON).
//!
//! # File intents
//!
//! `write` and `edit` are checked by claiming the edited file's intent
//! ([`crate::intent`]) under the tool call's session. The claim *is* the
//! check: the holding session refreshes its own intent, an expired intent is
//! taken over, and a different live holder produces a deny that names the
//! file and the holder. A deny never writes, so the blocked session does not
//! end up holding the file it was just denied.
//!
//! An intent is taken *before* the tool runs and is never released afterwards:
//! V2's verified `execute.before` hook has no release counterpart, so a file
//! stays claimed for the TTL (30 minutes by default) after its last edit,
//! refreshed by every further edit from the same session. That is the price of
//! catching a peer that reaches the file seconds after the first edit; it is
//! why the deny text tells the caller to wait rather than to retry.
//!
//! # Own-lease recognition
//!
//! `tool.execute.before` carries OpenCode's `sessionID` and the shim forwards
//! it, so a lease claimed with `--session <that id>` is recognized as the
//! caller's own. OpenCode's V1 `shell.env` hook — which let the old shim
//! inject `PORTZILLA_SESSION` into every shell subprocess — does not exist in
//! the V2 plugin API, so nothing here can hand the session id to the agent's
//! environment; a claim is only recognized as own when its session id matches
//! the id OpenCode reports for the call.
//!
//! # No pre-execution model-visible warn channel
//!
//! `tool.execute.before` is binary: return = allow, throw = deny. V1 deferred
//! warn text to `tool.execute.after`; the V2 registration for that event has
//! not been verified, and a hook that fails to register can keep the whole
//! plugin from loading, so the shim logs a warn to OpenCode's console instead
//! of the model. The warning still never blocks.
//!
//! # Fail-open, concretely
//!
//! This module prints `{"action":"allow"}` and exit 0 on every
//! portzilla-side failure (unparseable JSON, missing command or file, missing
//! session id for an edit, unreadable store, panic — see `main.rs`'s
//! `catch_unwind`), with a diagnostic on stderr. The shim also fails open on
//! its own side (any spawn/parse/timeout problem allows), so a portzilla
//! problem can never block an OpenCode tool call. Note that OpenCode itself
//! has no hook timeout — the shim enforces its own (5000 ms) around the
//! subprocess.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure
//! flips from allow to `{"action":"deny","reason":...}` instead; the shim's
//! existing throw-on-deny path then blocks the tool call exactly as a real
//! deny would.

use crate::guard::{GuardAuditDraft, Verdict};
use crate::hook_common::{EvaluationRequest, evaluate_with_evidence};
use crate::intent::{IntentError, IntentStore, key_for_file};
use crate::lease::{Lease, PidChecker, current_unix_timestamp};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How long a file intent stays binding after the tool call that took it.
///
/// Thirty minutes covers a multi-step edit of one file in a slow agent
/// session while still freeing a crashed session's file without manual
/// cleanup. Overridable with `PORTZILLA_INTENT_TTL_SECS` for tests and
/// tuning.
const DEFAULT_INTENT_TTL_SECS: u64 = 30 * 60;

/// Environment variable that overrides [`DEFAULT_INTENT_TTL_SECS`]. It exists
/// for tests and tuning, not as a supported contract: a hook path must keep
/// working when the value is garbage, so an unparseable or zero value falls
/// back to the default instead of failing the hook.
const INTENT_TTL_ENV: &str = "PORTZILLA_INTENT_TTL_SECS";

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_json` to stdout (ALWAYS present — the
/// shim parses the verdict from stdout, so silence must never mean
/// anything), and print `stderr_note` to stderr if present. Exit code is
/// always 0.
pub struct HookOutcome {
    pub stdout_json: String,
    pub stderr_note: Option<String>,
    #[allow(dead_code)]
    pub(crate) audit: Option<GuardAuditDraft>,
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

/// Builds the outcome for a portzilla-side failure: deny under fail-closed,
/// otherwise allow with a diagnostic on stderr. Every failure in this module
/// funnels through here so the two policies cannot drift apart.
fn failure_outcome(fail_closed: bool, message: &str) -> HookOutcome {
    if fail_closed {
        return HookOutcome {
            stdout_json: fail_closed_response(message),
            stderr_note: None,
            audit: None,
        };
    }
    HookOutcome {
        stdout_json: verdict_json("allow", None),
        stderr_note: Some(format!(
            "portzilla hook opencode: {message}, failing open (allow)"
        )),
        audit: None,
    }
}

/// The subset of the shim's stdin JSON this adapter reads. The shim (not
/// OpenCode's hook payload) owns the translation from OpenCode's input
/// shape, so this contract is portzilla's own and minimal by design:
/// `session_id` (null when absent), plus either `command` (a `shell` call)
/// or `file` + `cwd` (a `write`/`edit` call).
#[derive(Debug, Deserialize)]
struct OpenCodeInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Output JSON shape: a flat `action` verdict plus an optional `reason`.
/// The shim recognizes `deny` (throw) and `warn` (log); anything else —
/// including missing/unknown `action` — is treated as allow by the shim.
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

/// Handles one shim invocation: parses `raw_input` and dispatches to the
/// file-intent path when it carries a `file`, otherwise to the command path.
///
/// The command path passes `self_pid` through `hook_common::evaluate`, which
/// delegates to the guard core. It is always `None`: OpenCode's hook fires
/// *before* the tool runs, so there is no process yet whose PID this adapter
/// could pass. Ownership resolves through `self_session`, taken from the
/// shim's `session_id` — which the shim receives from OpenCode's
/// `tool.execute.before` input.
///
/// When `fail_closed` is `true`, every portzilla-side failure (unparseable
/// JSON, missing command, missing session id for an edit, …) returns a deny
/// JSON on stdout instead of allow + stderr note.
pub fn handle_with_policy(
    raw_input: &str,
    leases: &[Lease],
    checker: &dyn PidChecker,
    fail_closed: bool,
) -> HookOutcome {
    let input: OpenCodeInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            return failure_outcome(
                fail_closed,
                &format!("could not parse hook input JSON: {err}"),
            );
        }
    };

    if let Some(file) = input.file.as_deref() {
        return handle_file_intent(
            file,
            input.session_id.as_deref(),
            input.cwd.as_deref(),
            None,
            fail_closed,
        );
    }

    let Some(command) = input.command else {
        return failure_outcome(fail_closed, "hook input had no command or file");
    };

    let evaluation = evaluate_with_evidence(EvaluationRequest {
        command: &command,
        session: input.session_id.as_deref(),
        leases,
        checker,
    });
    match evaluation.verdict {
        Verdict::Allow => HookOutcome {
            stdout_json: verdict_json("allow", None),
            stderr_note: None,
            audit: evaluation.evidence,
        },
        Verdict::Deny { explanation, .. } => HookOutcome {
            stdout_json: verdict_json("deny", Some(explanation)),
            stderr_note: None,
            audit: evaluation.evidence,
        },
        Verdict::Warn { explanation } => HookOutcome {
            stdout_json: verdict_json("warn", Some(explanation)),
            stderr_note: None,
            audit: evaluation.evidence,
        },
    }
}

/// Handles the file-intent half of the protocol: `{ session_id, file, cwd }`.
///
/// The claim IS the check. [`IntentStore::claim`] runs under the store's
/// exclusive lock, so one atomic operation both decides and records: the
/// holding session refreshes its own intent, an expired intent is taken over,
/// and a live intent held by a different session is refused before anything
/// is written. Two sessions racing for one file therefore cannot both see it
/// free.
///
/// An edit with no session id cannot be attributed, so it follows the same
/// fail-open / `PORTZILLA_FAIL_CLOSED` policy as every other portzilla-side
/// failure.
///
/// `intent_dir` overrides the data directory (tests only); `None` resolves it
/// through `PORTZILLA_DATA_DIR`/XDG exactly like the lease store.
fn handle_file_intent(
    file: &str,
    session: Option<&str>,
    cwd: Option<&str>,
    intent_dir: Option<&Path>,
    fail_closed: bool,
) -> HookOutcome {
    let Some(session) = session.filter(|value| !value.trim().is_empty()) else {
        return failure_outcome(
            fail_closed,
            "hook input for a file edit had no session_id, so the edit cannot be attributed to a session",
        );
    };
    if file.trim().is_empty() {
        return failure_outcome(
            fail_closed,
            "hook input for a file edit had an empty file path",
        );
    }

    let cwd = match cwd {
        Some(cwd) => PathBuf::from(cwd),
        // `key_for_file` anchors a relative cwd to the process directory, so
        // this is the OpenCode process's working directory.
        None => PathBuf::from("."),
    };
    let key = key_for_file(&cwd, Path::new(file));

    let store = match IntentStore::open(intent_dir.map(Path::to_path_buf)) {
        Ok(store) => store,
        Err(err) => {
            return failure_outcome(
                fail_closed,
                &format!("could not open the file-intent store: {err:#}"),
            );
        }
    };

    let now = current_unix_timestamp();
    // The location is wired in when the adapter uses the conflict check; until
    // then a claim records only the key, exactly like a v1-era record.
    match store.claim(&key, session, now, now + intent_ttl_secs(), None) {
        Ok(_) => HookOutcome {
            stdout_json: verdict_json("allow", None),
            stderr_note: None,
            audit: None,
        },
        Err(IntentError::HeldBy { holder, .. }) => HookOutcome {
            stdout_json: verdict_json("deny", Some(held_file_reason(file, &holder))),
            stderr_note: None,
            audit: None,
        },
        Err(err) => failure_outcome(
            fail_closed,
            &format!("could not record a file intent for {file}: {err}"),
        ),
    }
}

/// The deny text for an edit of a file another live session holds. Follows
/// the kill-guard's shape: name what is blocked and who holds it, then say
/// what to do instead. The expiry is deliberately not rendered as a timestamp
/// — it is not useful prose, and the holding session may refresh it anyway.
fn held_file_reason(file: &str, holder: &str) -> String {
    format!(
        "{file} is held by session {holder}, which is editing it right now — a live claim from \
         another session, not a stale one. Do not edit it. Instead: wait for that session's claim \
         to expire, coordinate with it so one of you edits the file at a time, or ask the user \
         before proceeding."
    )
}

/// Resolves the intent TTL from an optional `PORTZILLA_INTENT_TTL_SECS`
/// value. Zero and unparseable values fall back to the default: this runs on
/// a hook path, where a bad knob must not turn into a failed edit.
fn intent_ttl_secs_from(value: Option<&str>) -> u64 {
    value
        .map(str::trim)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_INTENT_TTL_SECS)
}

fn intent_ttl_secs() -> u64 {
    intent_ttl_secs_from(std::env::var(INTENT_TTL_ENV).ok().as_deref())
}

/// The full source of the OpenCode plugin shim, printed by `portzilla init
/// opencode`. Save it as `.opencode/plugin/portzilla.js` (project) or
/// `~/.config/opencode/plugin/portzilla.js` (user) — OpenCode
/// auto-discovers `*.js`/`*.ts` files in those directories. The plugin uses
/// the V2 shape (a default export with `id` + `setup`), registers one
/// `tool.execute.before` hook, and enforces its own 5000 ms timeout around
/// the subprocess, since OpenCode applies no timeout to plugin hooks.
pub const PLUGIN_SNIPPET: &str = r#"// portzilla guard for OpenCode 2.x.
//
// Save as .opencode/plugin/portzilla.js (project) or
// ~/.config/opencode/plugin/portzilla.js (user) — OpenCode auto-discovers
// *.js/*.ts files in those directories. Requires `portzilla` on PATH.
// Quit and restart OpenCode after saving; plugins load once at startup.
//
// This is the V2 plugin shape: a default export with an `id` and a `setup`
// function. The V1 shape — an async function that receives `$` and returns a
// hooks object — is rejected by OpenCode 2.3.0 with "Plugin must export a
// default definition with an id and an effect or setup function".
//
// One hook, `tool.execute.before`, with three branches:
//   - write / edit: ask portzilla for a file intent on the path being
//     edited. If another live session holds that file, the tool call throws
//     and the file is not modified.
//   - shell: ask portzilla's kill-guard about the command. A command that
//     would kill another session's leased process throws before it runs.
//   - anything else: not ours, return.
//
// Any portzilla-side problem (binary missing, non-zero exit, unparseable
// output, the 5000 ms timeout) allows the tool call: a guard that breaks the
// session is worse than no guard.
//
// The subprocess is spawned with Bun.spawn rather than the `$` helper: `$`
// came with the V1 plugin context and is not part of the documented V2
// `setup(ctx)` context, while OpenCode runs plugins in Bun. Only
// askPortzilla would need to change if that moves.
//
// A `shell.create.before` hook is deliberately NOT registered: it carries no
// session id, so a shell command cannot be attributed to a session, and V2
// has no `shell.env` equivalent to inject one. File intents and the lease
// kill-guard both work from `execute.before` alone.
export default {
  id: "portzilla",
  async setup(ctx) {
    const CHECK_TIMEOUT_MS = 5000;

    // Sends one payload to `portzilla hook opencode` and parses the verdict.
    // Returns null for anything that is not a usable verdict; callers treat
    // null as allow.
    const askPortzilla = async (payload) => {
      let proc;
      try {
        proc = Bun.spawn(["portzilla", "hook", "opencode"], {
          stdin: new TextEncoder().encode(JSON.stringify(payload)),
          stdout: "pipe",
          stderr: "ignore",
        });
      } catch (err) {
        console.error(`portzilla: could not spawn the guard, allowing (${err})`);
        return null;
      }

      // OpenCode applies no timeout to plugin hooks, so enforce one here.
      const timer = setTimeout(() => {
        try {
          proc.kill();
        } catch {
          // Already exited; nothing to kill.
        }
      }, CHECK_TIMEOUT_MS);

      try {
        const text = await new Response(proc.stdout).text();
        const exitCode = await proc.exited;
        if (exitCode !== 0) return null;
        return JSON.parse(text);
      } catch (err) {
        console.error(`portzilla: guard returned no usable verdict, allowing (${err})`);
        return null;
      } finally {
        clearTimeout(timer);
      }
    };

    await ctx.tool.hook("execute.before", async (event) => {
      if (event.tool === "write" || event.tool === "edit") {
        const file = event.input?.path;
        if (typeof file !== "string" || file.trim() === "") return;

        const verdict = await askPortzilla({
          session_id: event.sessionID ?? null,
          file: file,
          cwd: ctx.directory ?? null,
        });
        if (verdict?.action === "deny") throw new Error(verdict.reason);
        return;
      }

      if (event.tool === "shell") {
        const command = event.input?.command;
        if (typeof command !== "string" || command.trim() === "") return;

        const verdict = await askPortzilla({
          session_id: event.sessionID ?? null,
          command: command,
        });
        if (verdict?.action === "deny") throw new Error(verdict.reason);
        // No verified non-blocking hook in V2 appends to the tool result, so
        // a warn goes to OpenCode's console instead of the model.
        if (verdict?.action === "warn") console.warn(`portzilla: ${verdict.reason}`);
      }
    });
  },
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

    fn action_of(outcome: &HookOutcome) -> String {
        response_json(outcome)["action"]
            .as_str()
            .expect("action must be a string")
            .to_string()
    }

    fn reason_of(outcome: &HookOutcome) -> String {
        response_json(outcome)["reason"]
            .as_str()
            .expect("reason must be a string")
            .to_string()
    }

    /// A project directory and a separate intent-store directory, plus a
    /// handle to seed intents the way a peer session would.
    struct FileFixture {
        _project: tempfile::TempDir,
        _intents: tempfile::TempDir,
        project: PathBuf,
        intents: PathBuf,
        store: IntentStore,
    }

    fn file_fixture() -> FileFixture {
        let project = tempfile::tempdir().unwrap();
        let intents = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(intents.path().to_path_buf())).unwrap();
        FileFixture {
            project: project.path().to_path_buf(),
            intents: intents.path().to_path_buf(),
            _project: project,
            _intents: intents,
            store,
        }
    }

    impl FileFixture {
        fn key(&self, file: &str) -> String {
            key_for_file(&self.project, Path::new(file))
        }

        fn handle(&self, file: &str, session: &str, fail_closed: bool) -> HookOutcome {
            handle_file_intent(
                file,
                Some(session),
                Some(self.project.to_str().unwrap()),
                Some(&self.intents),
                fail_closed,
            )
        }
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
        // kill it. With self_pid always None from this adapter, session is the
        // ONLY way ownership can ever resolve here, so this must Allow.
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
    fn audit_is_present_for_deny_and_warn_and_absent_for_allow() {
        let leases = vec![lease_with_session(3000, 1234, "server", "owner")];
        assert!(
            handle(
                &shim_input_with_session("kill 1234", "caller"),
                &leases,
                &AlwaysAlive
            )
            .audit
            .is_some()
        );
        assert!(
            handle(&shim_input("pkill node"), &[], &AlwaysAlive)
                .audit
                .is_some()
        );
        assert!(
            handle(&shim_input("git status"), &leases, &AlwaysAlive)
                .audit
                .is_none()
        );
    }

    #[test]
    fn deny_on_edit_of_a_file_held_by_another_session_names_file_and_holder() {
        let fixture = file_fixture();
        let now = current_unix_timestamp();
        fixture
            .store
            .claim(
                &fixture.key("src/auth.rs"),
                "session-theirs",
                now,
                now + 600,
                None,
            )
            .unwrap();

        let outcome = fixture.handle("src/auth.rs", "session-mine", false);

        assert!(outcome.stderr_note.is_none());
        assert_eq!(action_of(&outcome), "deny");
        let reason = reason_of(&outcome);
        assert!(
            reason.contains("src/auth.rs"),
            "the deny must name the file: {reason}"
        );
        assert!(
            reason.contains("session-theirs"),
            "the deny must name the holding session: {reason}"
        );
        assert!(
            reason.contains("Do not edit it"),
            "the deny must say what not to do: {reason}"
        );
    }

    #[test]
    fn deny_on_edit_leaves_the_holders_intent_untouched() {
        let fixture = file_fixture();
        let key = fixture.key("src/auth.rs");
        let now = current_unix_timestamp();
        fixture
            .store
            .claim(&key, "session-theirs", now, now + 600, None)
            .unwrap();
        let before = fixture.store.get(&key).unwrap().unwrap();

        let outcome = fixture.handle("src/auth.rs", "session-mine", false);
        assert_eq!(action_of(&outcome), "deny");

        let after = fixture.store.get(&key).unwrap().unwrap();
        assert_eq!(
            after, before,
            "a denied edit must not claim or refresh the file"
        );
        assert_eq!(after.session, "session-theirs");
    }

    #[test]
    fn allow_on_edit_by_the_holding_session_and_refresh_its_intent() {
        let fixture = file_fixture();
        let key = fixture.key("src/auth.rs");
        let ttl = intent_ttl_secs();
        let before_call = current_unix_timestamp();
        fixture
            .store
            .claim(
                &key,
                "session-mine",
                before_call - 1_000,
                before_call + 60,
                None,
            )
            .unwrap();
        let created_at = fixture.store.get(&key).unwrap().unwrap().created_at;

        let outcome = fixture.handle("src/auth.rs", "session-mine", false);
        let after_call = current_unix_timestamp();

        assert_eq!(action_of(&outcome), "allow");
        assert!(outcome.stderr_note.is_none());
        let after = fixture.store.get(&key).unwrap().unwrap();
        assert_eq!(after.session, "session-mine");
        assert_eq!(
            after.created_at, created_at,
            "a refresh keeps the original created_at"
        );
        assert!(
            after.expires_at >= before_call + ttl && after.expires_at <= after_call + ttl,
            "expected a refreshed expiry in [{}, {}], got {}",
            before_call + ttl,
            after_call + ttl,
            after.expires_at
        );
    }

    #[test]
    fn expired_intent_is_taken_over_and_the_edit_is_allowed() {
        let fixture = file_fixture();
        let key = fixture.key("src/auth.rs");
        let before_call = current_unix_timestamp();
        fixture
            .store
            .claim(
                &key,
                "session-theirs",
                before_call - 2_000,
                before_call - 1_000,
                None,
            )
            .unwrap();

        let outcome = fixture.handle("src/auth.rs", "session-mine", false);

        assert_eq!(action_of(&outcome), "allow");
        let after = fixture.store.get(&key).unwrap().unwrap();
        assert_eq!(after.session, "session-mine");
        assert!(
            after.created_at >= before_call,
            "an expired intent must be replaced, not refreshed: {}",
            after.created_at
        );
    }

    #[test]
    fn file_input_without_a_session_fails_open() {
        let project = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "file": "src/lib.rs",
            "cwd": project.path().to_string_lossy(),
        })
        .to_string();

        let outcome = handle_with_policy(&input, &[], &AlwaysAlive, false);

        assert_eq!(action_of(&outcome), "allow");
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("session_id")),
            "the note should say why the edit could not be attributed"
        );
    }

    #[test]
    fn file_input_without_a_session_denies_under_fail_closed() {
        let project = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "file": "src/lib.rs",
            "cwd": project.path().to_string_lossy(),
        })
        .to_string();

        let outcome = handle_with_policy(&input, &[], &AlwaysAlive, true);

        assert!(outcome.stderr_note.is_none());
        assert_eq!(action_of(&outcome), "deny");
        assert!(reason_of(&outcome).contains("session_id"));
    }

    #[test]
    fn corrupt_intent_store_fails_open() {
        let fixture = file_fixture();
        let intents = tempfile::tempdir().unwrap();
        std::fs::write(intents.path().join("intents.json"), "{ not json").unwrap();

        let outcome = handle_file_intent(
            "src/lib.rs",
            Some("session-mine"),
            Some(fixture.project.to_str().unwrap()),
            Some(intents.path()),
            false,
        );

        assert_eq!(action_of(&outcome), "allow");
        assert!(
            outcome
                .stderr_note
                .is_some_and(|note| note.contains("failing open")),
            "a store error must leave a diagnostic trail"
        );
    }

    #[test]
    fn corrupt_intent_store_denies_under_fail_closed() {
        let fixture = file_fixture();
        let intents = tempfile::tempdir().unwrap();
        std::fs::write(intents.path().join("intents.json"), "{ not json").unwrap();

        let outcome = handle_file_intent(
            "src/lib.rs",
            Some("session-mine"),
            Some(fixture.project.to_str().unwrap()),
            Some(intents.path()),
            true,
        );

        assert!(outcome.stderr_note.is_none());
        assert_eq!(action_of(&outcome), "deny");
        assert!(
            reason_of(&outcome).contains("invalid JSON"),
            "the reason should name the store problem"
        );
    }

    #[test]
    fn unopenable_intent_store_follows_the_same_policies() {
        let fixture = file_fixture();
        // A regular file where a data directory is expected: creating the
        // directory fails, so the store cannot open at all.
        let not_a_dir = tempfile::NamedTempFile::new().unwrap();

        let failing_open = handle_file_intent(
            "src/lib.rs",
            Some("session-mine"),
            Some(fixture.project.to_str().unwrap()),
            Some(not_a_dir.path()),
            false,
        );
        assert_eq!(action_of(&failing_open), "allow");
        assert!(
            failing_open
                .stderr_note
                .is_some_and(|note| note.contains("failing open"))
        );

        let failing_open_closed = handle_file_intent(
            "src/lib.rs",
            Some("session-mine"),
            Some(fixture.project.to_str().unwrap()),
            Some(not_a_dir.path()),
            true,
        );
        assert_eq!(action_of(&failing_open_closed), "deny");
    }

    #[test]
    fn empty_file_path_follows_the_failure_policy() {
        let fixture = file_fixture();

        assert_eq!(
            action_of(&fixture.handle("   ", "session-mine", false)),
            "allow"
        );
        assert_eq!(
            action_of(&fixture.handle("   ", "session-mine", true)),
            "deny"
        );
    }

    #[test]
    fn intent_ttl_defaults_and_overrides() {
        assert_eq!(intent_ttl_secs_from(None), DEFAULT_INTENT_TTL_SECS);
        assert_eq!(DEFAULT_INTENT_TTL_SECS, 30 * 60);
        assert_eq!(intent_ttl_secs_from(Some("120")), 120);
        assert_eq!(intent_ttl_secs_from(Some(" 120 ")), 120);
        assert_eq!(
            intent_ttl_secs_from(Some("0")),
            DEFAULT_INTENT_TTL_SECS,
            "zero must fall back rather than produce an already-expired claim"
        );
        assert_eq!(intent_ttl_secs_from(Some("-5")), DEFAULT_INTENT_TTL_SECS);
        assert_eq!(intent_ttl_secs_from(Some("soon")), DEFAULT_INTENT_TTL_SECS);
        assert_eq!(intent_ttl_secs_from(Some("")), DEFAULT_INTENT_TTL_SECS);
    }

    #[test]
    fn plugin_snippet_is_a_v2_plugin_guarding_edits_and_shell_commands() {
        // The shim is the load-bearing half of this adapter: these are the
        // exact strings OpenCode's V2 plugin loader and the guard protocol
        // depend on. It must export an id + setup definition, register
        // `execute.before`, route write/edit to the file-intent payload and
        // shell to the command payload, shell out to the binary, and throw
        // on deny.
        assert!(PLUGIN_SNIPPET.contains(r#"id: "portzilla""#));
        assert!(PLUGIN_SNIPPET.contains("async setup(ctx)"));
        assert!(PLUGIN_SNIPPET.contains("export default {"));
        assert_eq!(
            PLUGIN_SNIPPET.matches("ctx.tool.hook(").count(),
            1,
            "only execute.before is registered"
        );
        assert!(PLUGIN_SNIPPET.contains(r#"ctx.tool.hook("execute.before""#));
        assert!(PLUGIN_SNIPPET.contains(r#"event.tool === "write" || event.tool === "edit""#));
        assert!(PLUGIN_SNIPPET.contains(r#"event.tool === "shell""#));
        assert!(PLUGIN_SNIPPET.contains("event.input?.path"));
        assert!(PLUGIN_SNIPPET.contains("file: file"));
        assert!(PLUGIN_SNIPPET.contains("cwd: ctx.directory"));
        assert!(PLUGIN_SNIPPET.contains("portzilla hook opencode"));
        assert!(PLUGIN_SNIPPET.contains("Bun.spawn"));
        assert!(PLUGIN_SNIPPET.contains("CHECK_TIMEOUT_MS"));
        assert!(PLUGIN_SNIPPET.contains("throw new Error"));

        // The V1 shapes must not come back: this plugin does not load on
        // OpenCode 2.x, and the removed env injection would be dead code.
        assert!(!PLUGIN_SNIPPET.contains("export default async"));
        assert!(!PLUGIN_SNIPPET.contains("output.env.PORTZILLA_SESSION"));
        assert!(!PLUGIN_SNIPPET.contains(r#""bash""#));
    }
}
