//! Claude Code adapter: kill guard for `Bash`, file intents for `Edit`/`Write`.
//!
//! Owns parsing of Claude Code's hook payloads and rendering of the resulting
//! verdict into the JSON shape Claude Code expects on stdout. One command
//! serves two events, dispatched on `hook_event_name`:
//!
//! - `PreToolUse` + `Bash`: the kill guard, unchanged.
//! - `PreToolUse` + `Write`/`Edit`: claim the file's intent
//!   ([`crate::intent`]) and deny only a proven conflict, or an edit the
//!   conflict check cannot prove clean. A clean edit emits **nothing**:
//!   `permissionDecision: "allow"` would skip the user's permission prompt,
//!   and allowing an edit is not the hook's job — denying conflicts is.
//! - `PostToolUse` + `Write`/`Edit`: re-check the file now on disk and append
//!   `additionalContext` for the model. This event can never deny; its only
//!   failure mode is a missing warning, which is fail-open by construction.
//!
//! `hook_common::evaluate` delegates normalized command evaluation to the
//! harness-agnostic guard core; payload parsing and wire rendering stay local
//! to this adapter. Adding another harness means writing another module like
//! this one, not touching `guard`.
//!
//! Schema verified against the current Claude Code hooks documentation
//! (<https://code.claude.com/docs/en/hooks>, redirected from
//! <https://docs.anthropic.com/en/docs/claude-code/hooks>) — see the doc
//! comments below for exactly which fields and behavior that verification
//! covers. The edit payloads and the two-event contract were verified against
//! Claude Code 2.1.283: `Write` carries `file_path` + `content`, `Edit` carries
//! `file_path` + `old_string` + `new_string` (+ `replace_all`), and
//! `PostToolUse` alone supports `additionalContext`.
//!
//! # Fail-open, concretely
//!
//! Per `guard`'s fail-open principle: this adapter treats every failure
//! mode (unparseable JSON, missing fields, an unreadable lease or intent
//! store, even an internal panic — see `main.rs`'s `catch_unwind` around the
//! call into this module) the same way — print nothing to stdout and exit 0.
//! Per the Claude Code hook contract, exit 0 with empty stdout means "no
//! decision; continue with normal permission flow," i.e. allow. A diagnostic
//! goes to stderr for troubleshooting, which Claude Code only writes to its
//! debug log on exit 0 — it is never shown to the user or the model, so it
//! can never itself interrupt or alarm anyone.
//!
//! `PostToolUse` is exempt from the deny half of this policy: it never emits
//! `permissionDecision` at all, so a failure there is always silence.
//!
//! # Fail-closed mode (opt-in)
//!
//! When `PORTZILLA_FAIL_CLOSED=1` is set, every portzilla-side failure on a
//! `PreToolUse` path (unparseable input, missing command, store read error,
//! oversized stdin) flips from "allow + note" to "deny + reason" instead. The
//! exact JSON shape on stdout is owned by [`fail_closed_response`] and the same
//! `permissionDecisionReason` field Claude Code forwards to the model on
//! a deny. Pass-through is strict: the only way to enter fail-closed mode
//! is through the policy flag set by `main.rs` based on the environment
//! variable.

use crate::conflict::{self, ConflictVerdict, PendingEdit};
use crate::guard::{GuardAuditDraft, Verdict};
use crate::hook_common::{EvaluationRequest, evaluate_with_evidence};
use crate::intent::{DEFAULT_TTL_SECS, IntentError, IntentStore, locate_file};
use crate::lease::{Lease, PidChecker, current_unix_timestamp};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the caller (`main.rs`) should do with the result of handling one
/// hook invocation: print `stdout_json` to stdout if present (nothing if
/// `None` — see the fail-open note above), and print `stderr_note` to
/// stderr if present. Both are independent; either, both, or neither may be
/// set. Exit code is always 0 — the caller must never turn this into a
/// nonzero exit for a `PreToolUse` hook, since that risks blocking (exit 2)
/// or alarming (any nonzero exit) the user over what should be silent.
pub struct HookOutcome {
    pub stdout_json: Option<String>,
    pub stderr_note: Option<String>,
    #[allow(dead_code)]
    pub(crate) audit: Option<GuardAuditDraft>,
}

impl HookOutcome {
    fn allow_silent() -> Self {
        Self {
            stdout_json: None,
            stderr_note: None,
            audit: None,
        }
    }

    fn allow_with_note(note: impl Into<String>) -> Self {
        Self {
            stdout_json: None,
            stderr_note: Some(note.into()),
            audit: None,
        }
    }

    fn deny_with_reason(reason: String) -> Self {
        Self {
            stdout_json: Some(fail_closed_response(&reason)),
            stderr_note: None,
            audit: None,
        }
    }
}

/// Builds the Claude Code `PreToolUse` deny JSON for a portzilla-side
/// failure under fail-closed mode. The reason is sent to the agent via
/// `permissionDecisionReason`, the field Claude Code forwards to the model
/// when a tool call is denied. Kept here (rather than in `main.rs`) so the
/// adapter owns the exact byte shape Claude Code expects on stdout.
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

/// The hook event a payload belongs to, taken from `hook_event_name`.
///
/// One command serves both events, so the event decides the dispatch. Payloads
/// from Claude Code always carry the field (verified against 2.1.283); a
/// payload without it is treated as `PreToolUse` so the kill-guard behavior
/// that predates event dispatch survives incomplete mocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    PreToolUse,
    PostToolUse,
    Other,
}

impl HookEvent {
    fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("PreToolUse") => Self::PreToolUse,
            Some("PostToolUse") => Self::PostToolUse,
            Some(_) => Self::Other,
            None => Self::PreToolUse,
        }
    }
}

/// The subset of the Claude Code hook input JSON this adapter reads. Verified
/// fields (input JSON, from the docs' own worked example):
/// `session_id`, `prompt_id`, `transcript_path`, `cwd`, `permission_mode`,
/// `hook_event_name`, `tool_name`, `tool_input`, `tool_use_id`. This adapter
/// needs `hook_event_name` (dispatch), `session_id` (ownership and file
/// intents), `cwd` (resolving a relative `file_path`), `tool_name`, and the
/// tool input's `command` or edit fields; every other field is ignored via
/// `#[serde(default)]` tolerance rather than declared, so payload fields we
/// don't use (or future additions to them) never cause a parse failure.
#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<ToolInput>,
}

/// The tool inputs this adapter reads. `command` belongs to `Bash`; the rest
/// belong to `Write` and `Edit`, whose verified shapes are
/// `{file_path, content}` and `{file_path, old_string, new_string,
/// replace_all}`. `replace_all` is not modelled: the conflict check
/// reconstructs the caller's version by replacing the first occurrence, the
/// same way the OpenCode adapter does.
#[derive(Debug, Deserialize)]
struct ToolInput {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
}

/// Output JSON shape, verified against the docs' worked example for a
/// `PreToolUse` deny:
/// ```json
/// {
///   "hookSpecificOutput": {
///     "hookEventName": "PreToolUse",
///     "permissionDecision": "deny",
///     "permissionDecisionReason": "..."
///   }
/// }
/// ```
/// and the documented top-level `systemMessage` (shown to the user, not Claude)
/// field. `permissionDecision` is only ever `deny`; `additionalContext` exists
/// only on `PostToolUse`, where it carries the file-intent warning. A Bash
/// `Verdict::Warn` on `PreToolUse` therefore reaches the user through
/// `systemMessage` and never reaches the model: this event has no
/// model-visible, non-blocking channel.
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

/// Handles one hook invocation through the fail-open policy: parses
/// `raw_input` (the JSON Claude Code writes to the hook's stdin) and
/// dispatches on `hook_event_name` and `tool_name`.
///
/// Bash calls run through [`crate::hook_common::evaluate`] against `leases`.
/// `Write`/`Edit` calls need an [`IntentStore`], which this wrapper does not
/// have, so they follow the fail-open policy like any other missing input.
/// `main.rs` calls [`handle_with_policy`] so it can pass the store it opened
/// and the `PORTZILLA_FAIL_CLOSED` policy.
///
/// The adapter passes `self_pid` through `hook_common::evaluate`, which
/// delegates to the guard core. It is always `None`: `PreToolUse` fires
/// *before* the tool runs, so there is no process yet whose PID this adapter
/// could pass as "the process about to run this." Ownership therefore
/// resolves entirely through `self_session`, taken from the hook
/// payload's `session_id` field. For that to actually recognize anything as
/// "your own," the lease must have been claimed with a matching `--session`
/// — see the README's Kill guard section for exactly how an agent should
/// claim (`$CLAUDE_CODE_SESSION_ID`, verified against the Claude Code
/// environment variables reference) for this to resolve end to end.
#[allow(dead_code)]
pub fn handle(raw_input: &str, leases: &[Lease], checker: &dyn PidChecker) -> HookOutcome {
    handle_with_policy(raw_input, leases, checker, None, false)
}

/// Same as [`handle`] but with the intent store and the explicit fail-closed
/// policy.
///
/// `intents` is the store `main.rs` opened next to the lease store, or `None`
/// when it could not be opened. Edit checks treat `None` like any other
/// portzilla-side failure: fail open with a note, or deny under `fail_closed`.
/// When `fail_closed` is `true`, every portzilla-side failure (unparseable
/// JSON, missing command, …) returns a deny shape on stdout instead of allow +
/// stderr note. `PostToolUse` never denies, so its failures are always silent.
pub fn handle_with_policy(
    raw_input: &str,
    leases: &[Lease],
    checker: &dyn PidChecker,
    intents: Option<&IntentStore>,
    fail_closed: bool,
) -> HookOutcome {
    let input: HookInput = match serde_json::from_str(raw_input) {
        Ok(input) => input,
        Err(err) => {
            if fail_closed {
                return HookOutcome::deny_with_reason(
                    "could not parse hook input JSON".to_string(),
                );
            }
            return HookOutcome::allow_with_note(format!(
                "portzilla hook claude-code: could not parse hook input JSON, failing open (allow): {err}"
            ));
        }
    };

    match HookEvent::from_name(input.hook_event_name.as_deref()) {
        HookEvent::PreToolUse => handle_pre_tool_use(&input, leases, checker, intents, fail_closed),
        HookEvent::PostToolUse => handle_post_tool_use(&input, intents),
        HookEvent::Other => HookOutcome::allow_silent(),
    }
}

/// Dispatches a `PreToolUse` payload: `Bash` keeps the kill guard,
/// `Write`/`Edit` go to the file-intent check. Any other tool is not
/// portzilla's business, so the hook stays silent and Claude Code's normal
/// permission flow is untouched.
fn handle_pre_tool_use(
    input: &HookInput,
    leases: &[Lease],
    checker: &dyn PidChecker,
    intents: Option<&IntentStore>,
    fail_closed: bool,
) -> HookOutcome {
    match input.tool_name.as_deref() {
        Some("Bash") => handle_bash(input, leases, checker, fail_closed),
        Some("Write" | "Edit") => handle_edit(input, intents, fail_closed),
        // Silent, since this is the overwhelmingly common case (every tool
        // use fires this hook too, if the matcher isn't scoped) and not worth
        // a diagnostic line.
        _ => HookOutcome::allow_silent(),
    }
}

/// Runs one `Bash` tool call through the kill guard. The payload never
/// carries a PID for the process about to run, so ownership resolves through
/// the payload's `session_id` alone.
fn handle_bash(
    input: &HookInput,
    leases: &[Lease],
    checker: &dyn PidChecker,
    fail_closed: bool,
) -> HookOutcome {
    let Some(command) = input
        .tool_input
        .as_ref()
        .and_then(|tool| tool.command.as_deref())
    else {
        if fail_closed {
            return HookOutcome::deny_with_reason(
                "Bash tool call had no tool_input.command".to_string(),
            );
        }
        return HookOutcome::allow_with_note(
            "portzilla hook claude-code: Bash tool call had no tool_input.command, failing open (allow)"
                .to_string(),
        );
    };

    let evaluation = evaluate_with_evidence(EvaluationRequest {
        command,
        session: input.session_id.as_deref(),
        leases,
        checker,
    });
    match evaluation.verdict {
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
                audit: evaluation.evidence,
            }
        }
        Verdict::Warn { explanation } => {
            // `PreToolUse` has no `additionalContext` in the verified contract:
            // emitting it there was inert, so the model never saw the warning
            // this adapter claimed to deliver. `systemMessage` is documented as
            // reaching the user, which is the only channel this event offers
            // for a non-blocking warning.
            let response = HookResponse {
                system_message: Some(explanation),
                hook_specific_output: HookSpecificOutput {
                    hook_event_name: "PreToolUse",
                    permission_decision: None,
                    permission_decision_reason: None,
                    additional_context: None,
                },
            };
            HookOutcome {
                stdout_json: Some(
                    serde_json::to_string(&response).expect("HookResponse always serializes"),
                ),
                stderr_note: None,
                audit: evaluation.evidence,
            }
        }
    }
}

/// Handles a `PreToolUse` for `Write` or `Edit`: claims the file and denies
/// only when another live session holds it and the edit cannot be proven
/// clean.
///
/// The claim IS the check, exactly as in the OpenCode adapter:
/// [`IntentStore::claim`] runs under the store's exclusive lock, so one atomic
/// operation both decides and records. The holder refreshes its own intent, an
/// expired intent is taken over, and a live intent held by a different session
/// is refused before anything is written.
///
/// Claiming happens before the payload is judged. A missing or unusable edit
/// payload therefore still protects a free file — the next session is denied
/// rather than racing an unprotected one — and only matters when the file is
/// already held, where it makes the check unprovable and keeps today's deny.
///
/// A clean edit emits nothing. `permissionDecision: "allow"` would skip the
/// user's permission prompt, and allowing an edit is not this hook's job.
fn handle_edit(input: &HookInput, intents: Option<&IntentStore>, fail_closed: bool) -> HookOutcome {
    let Some(session) = input
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return edit_failure_outcome(
            fail_closed,
            "hook input for a file edit had no session_id, so the edit cannot be attributed to a session",
        );
    };
    let Some(file) = input
        .tool_input
        .as_ref()
        .and_then(|tool| tool.file_path.as_deref())
        .filter(|value| !value.trim().is_empty())
    else {
        return edit_failure_outcome(
            fail_closed,
            "hook input for a file edit had an empty file path",
        );
    };
    let Some(store) = intents else {
        return edit_failure_outcome(fail_closed, "the file-intent store is unavailable");
    };

    let cwd = input
        .cwd
        .as_deref()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let (key, location) = locate_file(&cwd, Path::new(file));
    // `conflict::check` joins the holder's repo-relative path onto the
    // directory it is handed, so it needs the caller's worktree root rather
    // than a possibly nested `cwd`.
    let caller_worktree = location.worktree.clone();
    let pending = pending_edit(input.tool_name.as_deref(), input.tool_input.as_ref());

    let now = current_unix_timestamp();
    match store.claim(&key, session, now, now + DEFAULT_TTL_SECS, Some(location)) {
        Ok(_) => HookOutcome::allow_silent(),
        Err(IntentError::HeldBy {
            holder,
            location: Some(holder_location),
            ..
        }) => {
            let verdict = pending.map_or(ConflictVerdict::Unprovable, |edit| {
                conflict::check(&caller_worktree, &holder_location, edit)
            });
            match verdict {
                ConflictVerdict::Clean => HookOutcome::allow_silent(),
                ConflictVerdict::Conflict => {
                    HookOutcome::deny_with_reason(conflicting_file_reason(file, &holder))
                }
                ConflictVerdict::Unprovable => {
                    HookOutcome::deny_with_reason(held_file_reason(file, &holder))
                }
            }
        }
        // A v1 record carries no location, so the peer cannot read the
        // holder's version: unprovable, and today's deny stays.
        Err(IntentError::HeldBy { holder, .. }) => {
            HookOutcome::deny_with_reason(held_file_reason(file, &holder))
        }
        Err(err) => edit_failure_outcome(
            fail_closed,
            &format!("could not record a file intent for {file}: {err}"),
        ),
    }
}

/// Handles a `PostToolUse` for `Write` or `Edit`: the edit already happened, so
/// this event may never deny. When another session still holds a live intent
/// for the file, the caller's file as written is compared with the holder's
/// version and the result is appended to the tool result as
/// `additionalContext`.
///
/// The warning is recomputed rather than stashed: the two hook invocations are
/// separate processes and the payload carries no reliable call id, so a
/// pending-warning map could not be shared safely. Recomputing is also stricter
/// than the OpenCode version — it compares the file *as written* and can report
/// a conflict the edit just created, which `PreToolUse` could not know.
///
/// No store, no session, no file, no record, an own claim, an expired claim, a
/// v1 record without a location, an unreadable file or any store error produce
/// no output: the only failure mode here is a missing warning.
fn handle_post_tool_use(input: &HookInput, intents: Option<&IntentStore>) -> HookOutcome {
    let Some(store) = intents else {
        return HookOutcome::allow_silent();
    };
    let Some(session) = input
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return HookOutcome::allow_silent();
    };
    let Some(file) = input
        .tool_input
        .as_ref()
        .and_then(|tool| tool.file_path.as_deref())
        .filter(|value| !value.trim().is_empty())
    else {
        return HookOutcome::allow_silent();
    };

    let cwd = input
        .cwd
        .as_deref()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let (key, location) = locate_file(&cwd, Path::new(file));
    let record = match store.get(&key) {
        Ok(Some(record)) => record,
        Ok(None) | Err(_) => return HookOutcome::allow_silent(),
    };
    if record.session == session || !record.is_live_at(current_unix_timestamp()) {
        return HookOutcome::allow_silent();
    }
    let Some(holder_location) = record.location else {
        return HookOutcome::allow_silent();
    };

    // The caller's file is the post-edit version: the tool already ran. Read it
    // from the caller's own worktree so a tracked edit in a linked worktree is
    // compared against this checkout, not the holder's.
    let Ok(post_edit) = std::fs::read_to_string(location.worktree.join(&holder_location.rel))
    else {
        return HookOutcome::allow_silent();
    };
    let verdict = conflict::check(
        &location.worktree,
        &holder_location,
        PendingEdit::Write {
            content: &post_edit,
        },
    );
    match verdict {
        ConflictVerdict::Clean => context_response(clean_post_edit_context(file, &record.session)),
        ConflictVerdict::Conflict => {
            context_response(conflicting_post_edit_context(file, &record.session))
        }
        // The check could not prove anything, and this event may not deny. A
        // missing warning is the only cost, so stay silent.
        ConflictVerdict::Unprovable => HookOutcome::allow_silent(),
    }
}

/// Builds the `PostToolUse` `additionalContext` response. There is
/// deliberately no `permissionDecision` field: a post-edit hook may only add
/// information to the tool result the model reads.
fn context_response(context: String) -> HookOutcome {
    let response = HookResponse {
        system_message: None,
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PostToolUse",
            permission_decision: None,
            permission_decision_reason: None,
            additional_context: Some(context),
        },
    };
    HookOutcome {
        stdout_json: Some(
            serde_json::to_string(&response).expect("HookResponse always serializes"),
        ),
        stderr_note: None,
        audit: None,
    }
}

/// The context for an edit that provably merges with the holder's version. It
/// says the merge was clean *after* the fact and asks for coordination before
/// the two changes drift into the same lines.
fn clean_post_edit_context(file: &str, holder: &str) -> String {
    format!("{file} is also being edited by session {holder}; the versions merged cleanly.")
}

/// The context for an edit the check finds conflicting. The edit already
/// happened, so this informs rather than blocks: the next edit is the one that
/// will be denied if the versions still conflict.
fn conflicting_post_edit_context(file: &str, holder: &str) -> String {
    format!(
        "{file} now conflicts with session {holder}'s version; coordinate before the next edit."
    )
}

/// Converts the tool payload into the conflict check's input.
///
/// The tool name decides the required shape: `Write` needs the whole new file
/// (`content`), `Edit` needs both halves of the replacement. Anything missing
/// means the payload cannot describe an edit, which the caller treats as
/// unprovable. `replace_all` is ignored: the check replaces the first
/// occurrence, matching the OpenCode adapter.
fn pending_edit<'a>(
    tool_name: Option<&str>,
    input: Option<&'a ToolInput>,
) -> Option<PendingEdit<'a>> {
    let input = input?;
    match tool_name {
        Some("Write") => input
            .content
            .as_deref()
            .map(|content| PendingEdit::Write { content }),
        Some("Edit") => match (input.old_string.as_deref(), input.new_string.as_deref()) {
            (Some(old), Some(new)) => Some(PendingEdit::Replace { old, new }),
            _ => None,
        },
        _ => None,
    }
}

/// The outcome for a portzilla-side failure on an edit: deny under
/// fail-closed, otherwise allow with a diagnostic on stderr. Mirrors the
/// OpenCode adapter's `failure_outcome` so both harnesses follow the same
/// policy.
fn edit_failure_outcome(fail_closed: bool, message: &str) -> HookOutcome {
    if fail_closed {
        return HookOutcome::deny_with_reason(message.to_string());
    }
    HookOutcome::allow_with_note(format!(
        "portzilla hook claude-code: {message}, failing open (allow)"
    ))
}

/// The deny text for an edit that really conflicts with the holder's version,
/// as opposed to one that could not be checked. Mirrors
/// `opencode::conflicting_file_reason` word for word so both harnesses read
/// the same policy; keep the two in sync.
fn conflicting_file_reason(file: &str, holder: &str) -> String {
    format!(
        "{file} is held by session {holder}, and the pending edit conflicts with the version that \
         session is editing right now. Do not edit it. Instead: wait for that session's claim to \
         expire, coordinate with it so one of you edits the file at a time, or ask the user before \
         proceeding."
    )
}

/// The deny text for an edit of a file another live session holds. Mirrors
/// `opencode::held_file_reason` word for word; keep the two in sync.
fn held_file_reason(file: &str, holder: &str) -> String {
    format!(
        "{file} is held by session {holder}, which is editing it right now — a live claim from \
         another session, not a stale one. Do not edit it. Instead: wait for that session's claim \
         to expire, coordinate with it so one of you edits the file at a time, or ask the user \
         before proceeding."
    )
}

/// The `.claude/settings.json` snippet printed by `portzilla init
/// claude-code`. It registers the one hook command on both events: the
/// `PreToolUse` entry covers the kill guard (`Bash`) and the file-intent check
/// (`Write`/`Edit`), and the `PostToolUse` entry appends the clean-merge or
/// conflict-created context to the tool result the model reads.
///
/// Format verified against the docs' own worked example (`matcher`, `hooks:
/// [{ type: "command", command: ... }]`) and against Claude Code 2.1.283 for
/// the combined matcher `"Write|Edit"`; a `PreToolUse` matcher accepts
/// alternatives, so one entry covers all three tools without a second process
/// registration.
pub const SETTINGS_SNIPPET: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "portzilla hook claude-code"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "portzilla hook claude-code"
          }
        ]
      }
    ]
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Intent;
    use crate::lease::Lease;
    use crate::lease::test_support::{AlwaysAlive, AlwaysDead};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn lease(port: u16, pid: u32, tag: &str) -> Lease {
        Lease::new(port, pid, tag, None)
    }

    fn bash_input(command: &str) -> String {
        bash_input_with_session(command, "abc123")
    }

    fn bash_input_with_session(command: &str, session_id: &str) -> String {
        serde_json::json!({
            "session_id": session_id,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_use_id": "toolu_01"
        })
        .to_string()
    }

    fn lease_with_session(port: u16, pid: u32, tag: &str, session: &str) -> Lease {
        Lease::new(port, pid, tag, Some(session.to_string()))
    }

    #[test]
    fn a_write_payload_exposes_the_file_and_content() {
        let input: HookInput = serde_json::from_str(
            &serde_json::json!({
                "session_id": "session-a",
                "hook_event_name": "PreToolUse",
                "cwd": "/repo",
                "tool_name": "Write",
                "tool_input": { "file_path": "src/auth.rs", "content": "a\nb\n" },
                "tool_use_id": "toolu_01"
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            HookEvent::from_name(input.hook_event_name.as_deref()),
            HookEvent::PreToolUse
        );
        assert_eq!(input.cwd.as_deref(), Some("/repo"));
        assert_eq!(input.tool_name.as_deref(), Some("Write"));
        let tool_input = input.tool_input.expect("tool_input must parse");
        assert_eq!(tool_input.file_path.as_deref(), Some("src/auth.rs"));
        assert_eq!(tool_input.content.as_deref(), Some("a\nb\n"));
        assert!(tool_input.command.is_none());
    }

    #[test]
    fn an_edit_payload_exposes_old_and_new() {
        let input: HookInput = serde_json::from_str(
            &serde_json::json!({
                "session_id": "session-a",
                "hook_event_name": "PreToolUse",
                "tool_name": "Edit",
                "tool_input": {
                    "file_path": "src/auth.rs",
                    "old_string": "b\n",
                    "new_string": "B\n",
                    "replace_all": false
                }
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(input.tool_name.as_deref(), Some("Edit"));
        let tool_input = input.tool_input.expect("tool_input must parse");
        assert_eq!(tool_input.file_path.as_deref(), Some("src/auth.rs"));
        assert_eq!(tool_input.old_string.as_deref(), Some("b\n"));
        assert_eq!(tool_input.new_string.as_deref(), Some("B\n"));
        assert!(tool_input.content.is_none());
    }

    #[test]
    fn a_post_tool_use_payload_is_recognized_as_such() {
        let input: HookInput = serde_json::from_str(
            &serde_json::json!({
                "session_id": "session-a",
                "hook_event_name": "PostToolUse",
                "tool_name": "Edit",
                "tool_input": {
                    "file_path": "src/auth.rs",
                    "old_string": "b\n",
                    "new_string": "B\n"
                }
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            HookEvent::from_name(input.hook_event_name.as_deref()),
            HookEvent::PostToolUse
        );
    }

    #[test]
    fn the_bash_path_is_unchanged() {
        // Parsing the extra fields must not disturb the kill guard: the same
        // payload still denies a foreign live lease with the same shape.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let outcome = handle(&bash_input("kill 1234"), &leases, &AlwaysAlive);

        let json: serde_json::Value =
            serde_json::from_str(&outcome.stdout_json.expect("deny must produce stdout JSON"))
                .unwrap();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
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
    fn audit_is_present_for_deny_and_warn_and_absent_for_allow() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "owner")];
        assert!(
            handle(
                &bash_input_with_session("kill 1234", "caller"),
                &leases,
                &AlwaysAlive
            )
            .audit
            .is_some()
        );
        assert!(
            handle(&bash_input("pkill node"), &[], &AlwaysAlive)
                .audit
                .is_some()
        );
        assert!(
            handle(&bash_input("git status"), &leases, &AlwaysAlive)
                .audit
                .is_none()
        );
    }

    #[test]
    fn allow_on_kill_of_unleased_port_emits_nothing() {
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
    fn an_unguarded_tool_is_a_silent_no_op() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": { "file_path": "/tmp/x" }
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
    fn warn_on_unresolvable_process_name_emits_system_message_only() {
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
        // PreToolUse has no additionalContext in the verified contract: emitting
        // one was inert. The warning reaches the user through systemMessage.
        assert!(
            json["hookSpecificOutput"]
                .get("additionalContext")
                .is_none(),
            "PreToolUse must not carry additionalContext: {json}"
        );
        let system_message = json["systemMessage"]
            .as_str()
            .expect("systemMessage must be a string");
        assert!(system_message.contains("node"));
    }

    #[test]
    fn allows_kill_of_a_lease_owned_by_the_same_session() {
        // The lease was claimed under the SAME Claude Code session that's
        // now trying to kill it (e.g. restarting its own dev server) — this
        // is the case CRITICAL 2 was about: with self_pid always None from
        // this adapter, session is the ONLY way ownership can ever resolve
        // here, so this must actually Allow, not Deny.
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
    fn settings_snippet_registers_both_hook_events() {
        let value: serde_json::Value = serde_json::from_str(SETTINGS_SNIPPET)
            .expect("SETTINGS_SNIPPET must itself be valid JSON");

        let pre = &value["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Bash|Write|Edit");
        assert_eq!(pre["hooks"][0]["type"], "command");
        assert_eq!(pre["hooks"][0]["command"], "portzilla hook claude-code");

        let post = &value["hooks"]["PostToolUse"][0];
        assert_eq!(post["matcher"], "Write|Edit");
        assert_eq!(post["hooks"][0]["type"], "command");
        assert_eq!(post["hooks"][0]["command"], "portzilla hook claude-code");
    }

    /// A git repository whose `src/auth.rs` is committed as `"a\nb\nc\n"`,
    /// plus a holder copy outside the repository and an isolated intent store.
    /// Mirrors the fixture the OpenCode file-intent tests use: the holder's
    /// worktree is the repository (so both HEADs share a merge base), while
    /// its working copy diverges without touching the caller's checkout.
    struct EditFixture {
        _repo: tempfile::TempDir,
        _holder: tempfile::TempDir,
        _intents: tempfile::TempDir,
        repo: PathBuf,
        store: IntentStore,
        key: String,
        location: crate::intent::IntentLocation,
    }

    /// Runs a git command for the fixture, under the same fork lock the
    /// product's git helpers take: a child that execs while another test holds
    /// a listener duplicates that fd until the exec and makes socket tests
    /// flaky.
    fn run_git(dir: &Path, args: &[&str]) {
        let _fork_guard = crate::store::port_test_support::lock();
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git must be installed to run the file-intent tests");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn edit_fixture(holder_content: &str) -> EditFixture {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = repo_dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/auth.rs"), "a\nb\nc\n").unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["add", "."]);
        // Explicit identity and signing flags: a developer's global git config
        // must not be able to make the fixture commit fail.
        run_git(
            &repo,
            &[
                "-c",
                "user.email=portzilla@example.com",
                "-c",
                "user.name=Portzilla Test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        );

        let holder_dir = tempfile::tempdir().unwrap();
        let holder_file = holder_dir.path().join("auth.rs");
        std::fs::write(&holder_file, holder_content).unwrap();

        let intents_dir = tempfile::tempdir().unwrap();
        let store = IntentStore::open(Some(intents_dir.path().to_path_buf())).unwrap();
        let (key, located) = locate_file(&repo, Path::new("src/auth.rs"));
        let location = crate::intent::IntentLocation {
            file: holder_file,
            worktree: repo.clone(),
            rel: located.rel,
        };
        EditFixture {
            repo,
            store,
            key,
            location,
            _repo: repo_dir,
            _holder: holder_dir,
            _intents: intents_dir,
        }
    }

    impl EditFixture {
        /// Seeds the holder claim the way a peer session would have. Returns
        /// the stored record so a test can compare it before and after.
        fn seed_holder(&self, location: Option<crate::intent::IntentLocation>) -> Intent {
            let now = current_unix_timestamp();
            self.store
                .claim(
                    &self.key,
                    "session-theirs",
                    now.saturating_sub(10),
                    now + 600,
                    location,
                )
                .unwrap()
        }

        fn pre_tool_use(&self, tool: &str, tool_input: serde_json::Value) -> HookOutcome {
            let payload = serde_json::json!({
                "session_id": "session-mine",
                "hook_event_name": "PreToolUse",
                "cwd": self.repo.to_str().unwrap(),
                "tool_name": tool,
                "tool_input": tool_input,
                "tool_use_id": "toolu_01"
            })
            .to_string();
            handle_with_policy(&payload, &[], &AlwaysAlive, Some(&self.store), false)
        }

        fn post_tool_use(&self, tool: &str, tool_input: serde_json::Value) -> HookOutcome {
            let payload = serde_json::json!({
                "session_id": "session-mine",
                "hook_event_name": "PostToolUse",
                "cwd": self.repo.to_str().unwrap(),
                "tool_name": tool,
                "tool_input": tool_input,
                "tool_response": { "success": true },
                "tool_use_id": "toolu_01"
            })
            .to_string();
            handle_with_policy(&payload, &[], &AlwaysAlive, Some(&self.store), false)
        }

        fn set_caller_file(&self, content: &str) {
            std::fs::write(self.repo.join("src/auth.rs"), content).unwrap();
        }
    }

    fn edit_input(old: &str, new: &str) -> serde_json::Value {
        serde_json::json!({
            "file_path": "src/auth.rs",
            "old_string": old,
            "new_string": new
        })
    }

    fn write_input(content: &str) -> serde_json::Value {
        serde_json::json!({ "file_path": "src/auth.rs", "content": content })
    }

    fn deny_reason(outcome: &HookOutcome) -> String {
        let json: serde_json::Value = serde_json::from_str(
            outcome
                .stdout_json
                .as_deref()
                .expect("a deny must produce stdout JSON"),
        )
        .expect("the deny must be valid JSON");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
        json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("permissionDecisionReason must be a string")
            .to_string()
    }

    #[test]
    fn an_edit_of_a_free_file_is_claimed_and_allowed_silently() {
        let fixture = edit_fixture("A\nb\nc\n");

        let outcome = fixture.pre_tool_use("Edit", edit_input("c\n", "C\n"));

        assert!(
            outcome.stdout_json.is_none(),
            "a free edit must emit nothing: {:?}",
            outcome.stdout_json
        );
        assert!(outcome.stderr_note.is_none());
        let stored = fixture
            .store
            .get(&fixture.key)
            .unwrap()
            .expect("the free file must be claimed");
        assert_eq!(stored.session, "session-mine");
        let location = stored.location.expect("the claim must carry a location");
        assert_eq!(
            location.file,
            std::fs::canonicalize(fixture.repo.join("src/auth.rs")).unwrap()
        );
        assert_eq!(location.rel, Path::new("src/auth.rs"));
        assert_eq!(
            location.worktree,
            std::fs::canonicalize(&fixture.repo).unwrap()
        );
    }

    #[test]
    fn a_conflicting_edit_on_a_held_file_is_denied_with_the_reason() {
        let fixture = edit_fixture("A\nb\nc\n");
        let before = fixture.seed_holder(Some(fixture.location.clone()));

        // The holder changed line 1; the caller changes the same line.
        let outcome = fixture.pre_tool_use("Edit", edit_input("a\n", "X\n"));

        assert!(outcome.stderr_note.is_none());
        let reason = deny_reason(&outcome);
        assert!(
            reason.to_lowercase().contains("conflict"),
            "the deny must say the edit really conflicts: {reason}"
        );
        assert!(reason.contains("src/auth.rs"));
        assert!(reason.contains("session-theirs"));
        assert!(reason.contains("Do not edit it"));
        assert_eq!(
            fixture.store.get(&fixture.key).unwrap().unwrap(),
            before,
            "a deny must not take over the claim"
        );
    }

    #[test]
    fn a_clean_edit_on_a_held_file_is_allowed_silently() {
        let fixture = edit_fixture("A\nb\nc\n");
        let before = fixture.seed_holder(Some(fixture.location.clone()));

        // The holder changed line 1; the caller changes line 3.
        let outcome = fixture.pre_tool_use("Edit", edit_input("c\n", "C\n"));

        assert!(
            outcome.stdout_json.is_none(),
            "a clean edit must emit nothing: {:?}",
            outcome.stdout_json
        );
        assert!(outcome.stderr_note.is_none());
        assert_eq!(
            fixture.store.get(&fixture.key).unwrap().unwrap(),
            before,
            "a clean edit must leave the holder's record byte-for-byte identical"
        );
    }

    #[test]
    fn a_held_file_without_a_location_is_denied() {
        // A v1 record: the peer cannot read the holder's version, so the
        // check is unprovable and today's deny stays.
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(None);

        let outcome = fixture.pre_tool_use("Edit", edit_input("c\n", "C\n"));

        let reason = deny_reason(&outcome);
        assert!(
            reason.contains("Do not edit it"),
            "the v1 deny must use today's wording: {reason}"
        );
    }

    #[test]
    fn a_write_payload_uses_content_as_the_post_edit_version() {
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(Some(fixture.location.clone()));

        // Only line 3 differs from the base, so it merges with the holder's
        // line 1 change.
        let clean = fixture.pre_tool_use("Write", write_input("a\nb\nC\n"));
        assert!(
            clean.stdout_json.is_none(),
            "a clean write must emit nothing: {:?}",
            clean.stdout_json
        );

        // The whole-file write replaces the holder's line 1.
        let conflict = fixture.pre_tool_use("Write", write_input("X\nb\nc\n"));
        let reason = deny_reason(&conflict);
        assert!(reason.contains("conflicts"));
        assert!(reason.contains("session-theirs"));
    }

    #[test]
    fn a_missing_edit_payload_on_a_free_file_is_still_claimed() {
        let fixture = edit_fixture("A\nb\nc\n");

        let outcome =
            fixture.pre_tool_use("Edit", serde_json::json!({ "file_path": "src/auth.rs" }));

        assert!(
            outcome.stdout_json.is_none(),
            "an unusable payload on a free file must still be allowed silently"
        );
        assert_eq!(
            fixture.store.get(&fixture.key).unwrap().unwrap().session,
            "session-mine",
            "the file must be protected even when the payload cannot be checked"
        );
    }

    #[test]
    fn a_missing_edit_payload_on_a_held_file_is_denied() {
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(Some(fixture.location.clone()));

        let outcome =
            fixture.pre_tool_use("Edit", serde_json::json!({ "file_path": "src/auth.rs" }));

        let reason = deny_reason(&outcome);
        assert!(reason.contains("Do not edit it"));
    }

    fn additional_context_of(outcome: &HookOutcome) -> String {
        let json: serde_json::Value = serde_json::from_str(
            outcome
                .stdout_json
                .as_deref()
                .expect("a warning must produce stdout JSON"),
        )
        .expect("the warning must be valid JSON");
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(json.get("systemMessage").is_none());
        json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext must be a string")
            .to_string()
    }

    #[test]
    fn a_clean_edit_on_a_held_file_emits_additional_context() {
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(Some(fixture.location.clone()));
        // The edit already happened: the caller's file is the post-edit
        // version, and only line 3 changed relative to the base.
        fixture.set_caller_file("a\nb\nC\n");

        let outcome = fixture.post_tool_use("Edit", edit_input("c\n", "C\n"));

        assert!(outcome.stderr_note.is_none());
        let context = additional_context_of(&outcome);
        assert!(context.contains("src/auth.rs"), "{context}");
        assert!(context.contains("session-theirs"), "{context}");
        assert!(context.contains("merged cleanly"), "{context}");
    }

    #[test]
    fn an_edit_that_created_a_conflict_emits_a_conflict_context() {
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(Some(fixture.location.clone()));
        // The caller rewrote the holder's line 1.
        fixture.set_caller_file("X\nb\nc\n");

        let outcome = fixture.post_tool_use("Edit", edit_input("a\n", "X\n"));

        assert!(outcome.stderr_note.is_none());
        let context = additional_context_of(&outcome);
        assert!(context.contains("src/auth.rs"), "{context}");
        assert!(context.contains("session-theirs"), "{context}");
        assert!(context.contains("now conflicts"), "{context}");
        assert!(
            !context.contains("merged cleanly"),
            "the conflict wording must differ from the clean one: {context}"
        );
    }

    #[test]
    fn a_post_tool_use_without_a_foreign_claim_is_silent() {
        // No record at all.
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.set_caller_file("X\nb\nc\n");
        let outcome = fixture.post_tool_use("Edit", edit_input("a\n", "X\n"));
        assert!(outcome.stdout_json.is_none(), "{:?}", outcome.stdout_json);
        assert!(outcome.stderr_note.is_none());

        // The caller's own claim: nothing to warn about.
        let now = current_unix_timestamp();
        fixture
            .store
            .claim(&fixture.key, "session-mine", now, now + 600, None)
            .unwrap();
        let own = fixture.post_tool_use("Edit", edit_input("a\n", "X\n"));
        assert!(own.stdout_json.is_none(), "{:?}", own.stdout_json);
        assert!(own.stderr_note.is_none());
    }

    #[test]
    fn post_tool_use_never_emits_a_permission_decision() {
        let fixture = edit_fixture("A\nb\nc\n");
        fixture.seed_holder(Some(fixture.location.clone()));
        fixture.set_caller_file("a\nb\nC\n");

        let outcome = fixture.post_tool_use("Edit", edit_input("c\n", "C\n"));
        let raw = outcome.stdout_json.expect("the clean case must warn");
        assert!(
            !raw.contains("permissionDecision"),
            "PostToolUse may only add context, never a decision: {raw}"
        );
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            json["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        assert!(json.get("permissionDecision").is_none());
    }
}
