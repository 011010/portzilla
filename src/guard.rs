//! Harness-agnostic kill-guard core.
//!
//! Takes a shell command string and the current lease registry, and decides
//! whether the command looks like it's about to kill a process that owns a
//! live lease belonging to someone else. This module knows nothing about
//! Claude Code, hooks, or any other harness — [`check`] is a pure function
//! over a command string and a lease list. Harness adapters (e.g.
//! `portzilla hook claude-code`) call it and translate the [`Verdict`] into
//! whatever their host tool's protocol expects.
//!
//! # Fail-open principle
//!
//! A guard is only useful if it is trustworthy, and it can only be
//! trustworthy if it never makes things worse than not being there at all.
//! [`check`] never panics on malformed or adversarial input — worst case it
//! returns [`Verdict::Allow`], because a false negative (a kill that should
//! have been denied but wasn't) just means the guard behaves as if it
//! weren't installed, while a false positive or a crash could block or break
//! a legitimate command and directly harm the user's session. Harness
//! adapters MUST preserve this: if anything upstream of calling `check`
//! fails (unparseable hook payload, unreadable lease store, etc.), the
//! adapter's job is to fail open — allow the command through and log a
//! warning — never to block or crash because the guard itself had a problem.
//!
//! # Detection approximation
//!
//! This is deliberately NOT a shell parser. Splitting on `|`, `;`, `&&`, and
//! `||`, then looking at each segment's first word, is enough to catch the
//! documented patterns without the complexity (and bug surface) of real
//! shell grammar — quoting, subshells, variable expansion, and heredocs are
//! not modeled. This is a conscious tradeoff toward false negatives over
//! false positives: see the "detection boundaries" tests below for exactly
//! what does and doesn't get caught, e.g. a command name inside a quoted
//! string (`echo "kill 123"`) is not detected, because the first word of
//! that segment is `echo`, not `kill`.
//!
//! # Normalization (shared by every caller — hooks and `guard` alike)
//!
//! Before segment detection, [`check`] normalizes the command in two ways,
//! so harness hooks (which hand over raw command strings) and the universal
//! `portzilla guard` wrapper see the same thing:
//!
//! - A LEADING `sh -c '<payload>'`-family invocation (`sh`/`bash`/`zsh`/
//!   `dash` by basename, any run of dash-prefixed flags with a `c` in a
//!   short cluster or as `-c`) is unwrapped to its payload, recursively, up
//!   to [`MAX_SHELL_UNWRAP_DEPTH`] levels. Only at the very start of the
//!   command string: a shell invocation appearing after another statement
//!   (`echo hi && sh -c 'kill 1234'`) is NOT unwrapped — an accepted false
//!   negative.
//! - Each pipeline segment's leading wrapper prefixes are stripped before
//!   the verb is read: env-var assignments (`FOO=1`), `sudo`, `env`
//!   (including its flags `-i`/`-u NAME`/... and its own `VAR=val`
//!   assignments), `command`, `exec`, `nohup`, `builtin`, and `npx` —
//!   repeatedly, until a non-wrapper first word is exposed
//!   (`sudo env FOO=1 command kill 1234` resolves to `kill`). Boundaries:
//!   `command` followed by its own flags (`command -v kill`, which only
//!   LOCATES the binary) is left as-is and therefore not detected; no
//!   command substitution, variable expansion, or backslash escapes.

use crate::lease::{Lease, PidChecker};

/// The result of checking a command against the lease registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No kill intent was detected, or the resolved target has no live
    /// lease, or its live lease is owned by the caller's own PID.
    Allow,
    /// The command targets a port or PID with a live lease owned by someone
    /// else. `explanation` is meant to be shown to the caller (human or
    /// agent) and says what to do instead of killing it.
    Deny {
        port: u16,
        owner_pid: u32,
        tag: String,
        explanation: String,
    },
    /// A kill intent was detected but could not be resolved to a specific
    /// port or PID (e.g. killing by process name) — portzilla cannot verify
    /// whether it's safe. `explanation` says why and what to check manually.
    Warn { explanation: String },
}

/// Evaluates `command` against `leases`, deciding whether it looks safe to
/// run.
///
/// A live lease is recognized as "your own" (and so never denied) when
/// EITHER:
/// - `self_session` is given and equals the lease's own session (both must
///   be present — a lease with no session never matches by session), OR
/// - `self_pid` is given and equals the lease's PID.
///
/// Both are optional and independent; pass `None` for whichever the caller
/// doesn't have. A caller with neither treats every live lease as
/// deny-worthy, since there is no way to tell "foreign" from "self" at all.
///
/// This dual check exists because no single harness necessarily has both:
/// the Claude Code `PreToolUse` adapter can supply `self_session` (from the
/// hook payload) but never `self_pid` (the command hasn't run yet, so there
/// is no PID), while a hypothetical harness driving `guard::check` after the
/// fact might have a real PID but no session concept at all.
pub fn check(
    command: &str,
    leases: &[Lease],
    self_pid: Option<u32>,
    self_session: Option<&str>,
    checker: &dyn PidChecker,
) -> Verdict {
    match detect_target(command) {
        None => Verdict::Allow,
        Some(KillTarget::Pids(pids)) => {
            for pid in pids {
                if let Some(lease) = leases.iter().find(|l| l.pid == pid)
                    && lease.is_alive(checker)
                    && !owned_by_self(lease, self_pid, self_session)
                {
                    return deny(lease);
                }
            }
            Verdict::Allow
        }
        Some(KillTarget::Port(port)) => match leases.iter().find(|l| l.port == port) {
            Some(lease)
                if lease.is_alive(checker) && !owned_by_self(lease, self_pid, self_session) =>
            {
                deny(lease)
            }
            _ => Verdict::Allow,
        },
        Some(KillTarget::ProcessName(name)) => Verdict::Warn {
            explanation: format!(
                "This command targets processes by name (\"{name}\"), which portzilla cannot \
                 resolve to a specific port or lease — it may affect a live dev server \
                 belonging to another session. Run `portzilla ls` to check for live leases \
                 before proceeding, or target a specific PID/port instead."
            ),
        },
    }
}

fn owned_by_self(lease: &Lease, self_pid: Option<u32>, self_session: Option<&str>) -> bool {
    let session_match =
        self_session.is_some_and(|session| lease.session.as_deref() == Some(session));
    let pid_match = self_pid.is_some_and(|pid| pid == lease.pid);
    session_match || pid_match
}

fn deny(lease: &Lease) -> Verdict {
    Verdict::Deny {
        port: lease.port,
        owner_pid: lease.pid,
        tag: lease.tag.clone(),
        explanation: format!(
            "Port {port} is leased to pid {pid} (tag: \"{tag}\") — a live process owned by \
             another session, not a stale one. Do not kill it. Instead: run `portzilla who \
             {port}` to confirm ownership, claim a different port for your own server with \
             `portzilla claim`, or ask the user before proceeding if you believe this lease is \
             actually stale.",
            port = lease.port,
            pid = lease.pid,
            tag = lease.tag,
        ),
    }
}

/// What a detected kill-shaped command is trying to target.
enum KillTarget {
    /// `kill`/`kill -9 <pid...>` — one or more explicit numeric PIDs.
    Pids(Vec<u32>),
    /// `lsof ... | xargs kill`, `fuser -k <port>/tcp`, `kill-port <port>` —
    /// a specific port.
    Port(u16),
    /// `pkill`/`killall <name>` — a process name, not resolvable against
    /// the (port, pid)-keyed lease registry.
    ProcessName(String),
}

/// Detects a kill intent anywhere in `command` and resolves what it targets.
///
/// See the module doc comment for the detection approximation, the shared
/// normalization, and their documented boundaries.
fn detect_target(command: &str) -> Option<KillTarget> {
    detect_target_impl(command, MAX_SHELL_UNWRAP_DEPTH)
}

/// The recursive core of [`detect_target`], carrying the remaining
/// `sh -c`-unwrap budget (`depth`). A leading `sh -c`-family invocation is
/// unwrapped one level and detection re-runs on the raw payload — so a kill
/// (including a whole `lsof | xargs kill` pipeline) hidden inside the
/// payload is analyzed exactly as if it had been typed directly. Once the
/// budget is exhausted the command is analyzed as-is (accepted false
/// negative beyond [`MAX_SHELL_UNWRAP_DEPTH`] nesting levels; never a panic).
fn detect_target_impl(command: &str, depth: u32) -> Option<KillTarget> {
    if depth > 0 {
        let tokens = shell_words(command);
        if let Some(payload) = find_shell_c_payload(&tokens) {
            return detect_target_impl(&payload, depth - 1);
        }
    }

    // Checked first and across the WHOLE command, not per-segment: the verb
    // (`xargs kill`) and the target (`lsof`'s `-ti:<port>`) live in
    // different pipeline segments.
    if let Some(port) = detect_lsof_xargs_kill(command) {
        return Some(KillTarget::Port(port));
    }

    for segment in split_segments(command) {
        if let Some(target) = detect_segment(segment) {
            return Some(target);
        }
    }
    None
}

/// Maximum `sh -c` nesting depth the unwrap will descend before analyzing
/// whatever payload it has as-is. Bounds the recursion against pathological
/// input; 8 levels is far beyond any legitimate invocation. Shared with the
/// universal `portzilla guard` wrapper (`crate::guard_cmd`), which unwraps
/// the same shape from real argv instead of a joined string.
pub(crate) const MAX_SHELL_UNWRAP_DEPTH: u32 = 8;

/// If `args` is `<shell> <flags...> <payload>` where `<shell>`'s basename is
/// `sh`/`bash`/`zsh`/`dash` and at least one flag token in the leading run
/// of dash-prefixed tokens is `-c` or a short cluster containing `c` (e.g.
/// `-lc`), returns the payload token. A `--`-prefixed (long) flag is never
/// treated as containing `-c` regardless of its letters (`--norc` must not
/// match on the `c` in "norc"). Shared with `crate::guard_cmd`.
pub(crate) fn find_shell_c_payload(args: &[String]) -> Option<String> {
    let (shell, rest) = args.split_first()?;
    if !is_shell_name(shell) {
        return None;
    }

    let mut index = 0;
    let mut saw_c_flag = false;
    while index < rest.len() && rest[index].starts_with('-') {
        let token = &rest[index];
        if !token.starts_with("--") && token[1..].contains('c') {
            saw_c_flag = true;
        }
        index += 1;
    }

    if !saw_c_flag {
        return None;
    }
    rest.get(index).cloned()
}

pub(crate) fn is_shell_name(token: &str) -> bool {
    matches!(
        token.rsplit('/').next().unwrap_or(token),
        "sh" | "bash" | "zsh" | "dash"
    )
}

/// Minimal, quote-aware whitespace tokenizer. Not a shell parser:
/// understands single and double quotes (each preserves whitespace and, per
/// POSIX rules, treats the OTHER quote character as literal while active —
/// which is exactly what makes `sh -c 'sh -c "kill 1234"'` tokenize with the
/// inner `"kill 1234"` intact), but has no concept of escape characters,
/// command substitution, or variable expansion. Shared with
/// `crate::guard_cmd`.
pub(crate) fn shell_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_content = false;

    for c in input.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_content = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_content = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_content {
                    words.push(std::mem::take(&mut current));
                    has_content = false;
                }
            }
            c => {
                current.push(c);
                has_content = true;
            }
        }
    }
    if has_content {
        words.push(current);
    }
    words
}

/// Splits `command` into pipeline/sequencing segments on `|`, `;`, `&&`, and
/// `||`, with no distinction between the separators. Used by [`detect_segment`],
/// which only cares about "is this segment's first word a kill verb" and
/// doesn't need to know whether the previous separator was a pipe or a
/// sequencer. Not shell-aware: does not respect quoting, so a literal `|`
/// inside a quoted string would also split there. See the module doc comment.
fn split_segments(command: &str) -> Vec<&str> {
    split_statements(command)
        .into_iter()
        .flat_map(split_pipeline)
        .collect()
}

/// Splits `command` into top-level statements on `;`, `&&`, and `||` —
/// everything EXCEPT `|`. Each statement may itself be a pipeline (`a | b`);
/// see [`split_pipeline`]. This distinction matters for
/// [`detect_lsof_xargs_kill`], which must only correlate an `lsof` segment
/// with a later `xargs kill` segment when they're actually piped together,
/// not merely sequenced by `;`/`&&`/`||` — those are unrelated statements
/// that happen to run one after another.
fn split_statements(command: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    for part in command.split(';') {
        for sub in part.split("&&") {
            for sub2 in sub.split("||") {
                let trimmed = sub2.trim();
                if !trimmed.is_empty() {
                    statements.push(trimmed);
                }
            }
        }
    }
    statements
}

/// Splits a single statement into its `|`-piped segments, in order.
fn split_pipeline(statement: &str) -> Vec<&str> {
    statement
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Detects a kill intent within a single pipeline segment (everything
/// except the `lsof | xargs kill` pattern, which spans segments).
fn detect_segment(segment: &str) -> Option<KillTarget> {
    let raw_tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut tokens: &[&str] = &raw_tokens;

    // `FOO=1 BAR=baz kill 1234`: leading per-command environment variable
    // assignments, which a real shell would apply only to this command.
    while tokens.first().is_some_and(|token| is_env_assignment(token)) {
        tokens = &tokens[1..];
    }

    while tokens
        .first()
        .is_some_and(|token| basename(token) == "sudo")
    {
        tokens = &tokens[1..];
    }

    // Leading command-wrappers that a real shell would still resolve to the
    // wrapped verb: `env`, `command`, `exec`, `nohup`, `builtin`. Each is
    // stripped repeatedly (so `sudo env command kill 1234` resolves to
    // `kill`). See the module doc for the documented boundaries: `command`
    // followed by its own flags (`command -v kill`, which only LOCATES the
    // binary) is left as-is and therefore not detected.
    while let Some(first) = tokens.first() {
        match basename(first) {
            "env" => {
                tokens = skip_env_args(&tokens[1..]);
            }
            "command" => {
                // `command -v kill` / `command -p kill ...`: a `command`
                // whose next token is a flag of its own is not an execution
                // of the wrapped verb we can resolve — leave it alone
                // (first word is then `command` itself, which is not a kill
                // verb, so this is a clean false negative).
                if tokens.get(1).is_some_and(|next| next.starts_with('-')) {
                    break;
                }
                tokens = &tokens[1..];
            }
            "exec" | "nohup" | "builtin" => {
                tokens = &tokens[1..];
            }
            _ => break,
        }
        // After stripping a wrapper, new env assignments or `sudo` may be
        // exposed before the next wrapper/verb — re-strip them too.
        while tokens.first().is_some_and(|token| is_env_assignment(token)) {
            tokens = &tokens[1..];
        }
        while tokens
            .first()
            .is_some_and(|token| basename(token) == "sudo")
        {
            tokens = &tokens[1..];
        }
    }

    if tokens.first().is_some_and(|token| basename(token) == "npx") {
        tokens = &tokens[1..];
        // Skip npx's own flags (e.g. `-y`/`--yes` to auto-confirm the
        // install prompt) before the package name they'd otherwise shadow.
        // Doesn't handle flags that take a separate argument (e.g. `-p
        // <package>`) — an accepted approximation, see the module doc.
        while tokens.first().is_some_and(|token| token.starts_with('-')) {
            tokens = &tokens[1..];
        }
    }

    let (name, rest): (&str, &[&str]) = match tokens {
        [] => return None,
        [name, rest @ ..] => (name, rest),
    };

    // `/usr/bin/kill 1234`: match on the basename, not the full path, but
    // still require an EXACT match — `./my-killer-app` must not match
    // `kill` just because it starts with a related-looking prefix.
    match basename(name) {
        "kill" => {
            let pids: Vec<u32> = rest
                .iter()
                .filter(|token| !token.starts_with('-'))
                .filter_map(|token| token.parse().ok())
                .collect();
            if pids.is_empty() {
                None
            } else {
                Some(KillTarget::Pids(pids))
            }
        }
        "pkill" | "killall" => {
            let target_name = rest
                .iter()
                .find(|token| !token.starts_with('-'))
                .map(|token| token.to_string())
                .unwrap_or_else(|| "<unspecified>".to_string());
            Some(KillTarget::ProcessName(target_name))
        }
        "kill-port" => rest
            .iter()
            .find_map(|token| token.parse::<u16>().ok())
            .map(KillTarget::Port),
        "fuser" => {
            if rest.contains(&"-k") {
                // fuser only treats a numeric argument as a port when it
                // carries an explicit `/tcp` or `/udp` suffix — a bare
                // number (`fuser -k 3000`) is a FILENAME argument to real
                // fuser, not a port, and must not be mistaken for one.
                rest.iter()
                    .find_map(|token| {
                        token
                            .strip_suffix("/tcp")
                            .or_else(|| token.strip_suffix("/udp"))
                            .and_then(|port| port.parse::<u16>().ok())
                    })
                    .map(KillTarget::Port)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Returns the last `/`-separated component of `token` (the whole token
/// unchanged if it has no `/`). Used so `/usr/bin/kill` matches on `kill`
/// the same way a bare `kill` does — while still requiring an exact match,
/// so `./my-killer-app` does NOT match just because of a shared prefix.
fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Heuristic for a leading shell-style environment variable assignment
/// (`FOO=1`, `NODE_ENV=production`): a `NAME=value` token where `NAME` is a
/// non-empty run of ASCII letters/digits/underscore not starting with a
/// digit — deliberately narrow so it doesn't accidentally swallow an
/// unrelated argument that happens to contain `=` (e.g. a URL query string).
fn is_env_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Skips `env`'s OWN arguments — `VAR=value` assignments and option flags
/// (`-i`, `-0`, `-u NAME`, `--unset=NAME`, ...) — returning the slice
/// starting at the command `env` would actually run. Used to strip an `env`
/// wrapper prefix before verb detection, so `env FOO=1 kill 1234` resolves
/// to `kill`. Deliberately conservative: `-u NAME` is the one common flag
/// that takes a SEPARATE argument, so it also consumes its next token; every
/// other dash-prefixed token is skipped singly. A flag whose separate
/// argument this doesn't model would leave that argument as the apparent
/// verb — a false NEGATIVE, the accepted tradeoff direction.
fn skip_env_args<'a>(tokens: &'a [&'a str]) -> &'a [&'a str] {
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if is_env_assignment(token) {
            i += 1;
        } else if token == "-u" {
            // `-u NAME`: skip the flag and its separate argument (if any).
            i = (i + 2).min(tokens.len());
        } else if token.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    &tokens[i..]
}

/// Detects the `lsof -ti:<port> | xargs kill` family: a `|`-piped segment
/// starting with `lsof` (from which a `:<port>` is extracted) followed,
/// later in the SAME pipeline (not merely the same command — see
/// [`split_statements`] vs [`split_pipeline`]), by a segment starting with
/// `xargs` that itself contains `kill` as one of its own tokens.
///
/// `lsof -ti:3000; find /tmp -name '*.pid' | xargs kill` must NOT match:
/// the `lsof` and the `xargs kill` are two unrelated statements that happen
/// to run one after another, not a pipeline — the `xargs kill` there isn't
/// fed by the `lsof` at all. Correlation is therefore scoped to one
/// statement's own `|` chain, reset between statements.
fn detect_lsof_xargs_kill(command: &str) -> Option<u16> {
    for statement in split_statements(command) {
        let mut port: Option<u16> = None;
        for segment in split_pipeline(statement) {
            let tokens: Vec<&str> = segment.split_whitespace().collect();
            match tokens.first() {
                Some(&"lsof") => port = scan_port_after_colon(segment),
                Some(&"xargs") if port.is_some() && tokens.contains(&"kill") => {
                    return port;
                }
                _ => {}
            }
        }
    }
    None
}

/// Scans `segment` for the first `:` immediately followed by one or more
/// ASCII digits, and parses that run as a port. Covers both `-ti:3000` and
/// `-ti :3000` (the colon and the digits are what matters, not what
/// immediately precedes the colon).
fn scan_port_after_colon(segment: &str) -> Option<u16> {
    let bytes = segment.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        if byte != b':' {
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > i + 1
            && let Ok(port) = segment[i + 1..end].parse::<u16>()
        {
            return Some(port);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Lease;
    use crate::lease::test_support::{AlivePids, AlwaysAlive, AlwaysDead};

    fn lease(port: u16, pid: u32, tag: &str) -> Lease {
        Lease::new(port, pid, tag, None)
    }

    fn lease_with_session(port: u16, pid: u32, tag: &str, session: &str) -> Lease {
        Lease::new(port, pid, tag, Some(session.to_string()))
    }

    // ==== kill / kill -9 <pid> ====

    #[test]
    fn kill_pid_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("kill 1234", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny {
                port,
                owner_pid,
                tag,
                explanation,
            } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 1234);
                assert_eq!(tag, "dev-server");
                assert!(explanation.contains("3000"));
                assert!(
                    explanation.contains("who"),
                    "explanation should point at `who`: {explanation}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn kill_pid_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check("kill 1234", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_pid_owned_by_self_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("kill 1234", &leases, Some(1234), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_pid_with_dead_lease_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("kill 1234", &leases, None, None, &AlwaysDead),
            Verdict::Allow
        );
    }

    // ==== ownership: self_pid / self_session ====

    #[test]
    fn matching_self_session_allows_even_without_a_matching_pid() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        // self_pid is deliberately absent/mismatched: session alone must be
        // sufficient (this is exactly the Claude Code adapter's situation —
        // it never has a self_pid to offer).
        assert_eq!(
            check("kill 1234", &leases, None, Some("session-a"), &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn mismatched_self_session_denies_even_with_no_self_pid_given() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        let verdict = check("kill 1234", &leases, None, Some("session-b"), &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn lease_without_a_session_denies_when_only_self_session_is_given() {
        // The lease has no session recorded at all (e.g. claimed without
        // `--session`) — self_session can't match something that isn't
        // there, and no self_pid was given either, so this must be denied
        // rather than assumed-safe.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("kill 1234", &leases, None, Some("session-a"), &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn self_pid_still_allows_when_self_session_is_not_given() {
        // Backward-compatible path: a caller with only a PID (no session
        // concept at all) can still recognize its own lease by PID.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("kill 1234", &leases, Some(1234), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn matching_self_session_allows_even_with_a_mismatched_self_pid() {
        // Session match alone is sufficient — a wrong/irrelevant self_pid
        // must not override it.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        assert_eq!(
            check(
                "kill 1234",
                &leases,
                Some(9999),
                Some("session-a"),
                &AlwaysAlive
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn neither_self_pid_nor_self_session_matching_denies() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        let verdict = check(
            "kill 1234",
            &leases,
            Some(9999),
            Some("session-b"),
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn kill_dash_9_pid_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("kill -9 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn kill_with_signal_name_flag_still_resolves_the_pid() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("kill -s KILL 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn kill_multiple_pids_denies_on_the_first_foreign_live_one() {
        let leases = vec![lease(3001, 222, "api")];
        // 111 has no lease; 222 has a foreign live lease.
        let verdict = check("kill 111 222", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny { owner_pid, .. } => assert_eq!(owner_pid, 222),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn sudo_kill_pid_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("sudo kill -9 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn bare_kill_with_no_pid_allows() {
        // No numeric target at all (e.g. `kill -l` lists signal names) — we
        // can't resolve a target, and this is a documented, accepted
        // false-negative boundary rather than a Warn.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("kill -l", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    // ==== pkill / killall <name> ====

    #[test]
    fn pkill_by_name_warns() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("pkill node", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Warn { explanation } => {
                assert!(explanation.contains("node"));
                assert!(explanation.contains("portzilla ls"));
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn killall_by_name_warns() {
        let leases: Vec<Lease> = vec![];
        let verdict = check("killall node", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Warn { .. }));
    }

    // ==== lsof -ti:<port> | xargs kill ====

    #[test]
    fn lsof_xargs_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "lsof -ti:3000 | xargs kill",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        match verdict {
            Verdict::Deny {
                port, owner_pid, ..
            } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 5555);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn lsof_xargs_kill_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check(
                "lsof -ti:3000 | xargs kill",
                &leases,
                None,
                None,
                &AlwaysAlive
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_xargs_kill_owned_by_self_allows() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(
            check(
                "lsof -ti:3000 | xargs kill",
                &leases,
                Some(5555),
                None,
                &AlwaysAlive
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_xargs_kill_with_dead_lease_allows() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(
            check(
                "lsof -ti:3000 | xargs kill",
                &leases,
                None,
                None,
                &AlwaysDead
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_space_colon_variant_with_dash_9_denies() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "lsof -ti :3000 | xargs kill -9",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn bare_lsof_without_xargs_kill_allows() {
        // Just inspecting, not killing anything.
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(
            check("lsof -ti:3000", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_and_unrelated_xargs_kill_across_semicolon_does_not_correlate() {
        // Two unrelated statements joined by `;`, not a real pipeline: the
        // lsof here never feeds the xargs kill. Must not deny.
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "lsof -ti:3000; find /tmp -name '*.pid' | xargs kill",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn lsof_and_unrelated_xargs_kill_across_double_ampersand_does_not_correlate() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "lsof -ti:3000 && find /tmp -name '*.pid' | xargs kill",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn real_lsof_xargs_kill_pipeline_still_denies_even_with_a_trailing_statement() {
        // Regression guard alongside the two tests above: a genuine `|`
        // pipeline must still be caught, including when followed by an
        // unrelated `;`-joined statement.
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "lsof -ti:3000 | xargs kill; echo done",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== fuser -k <port>/tcp ====

    #[test]
    fn fuser_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 777, "vite-dev")];
        let verdict = check("fuser -k 3000/tcp", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny {
                port, owner_pid, ..
            } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 777);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn fuser_kill_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check("fuser -k 3000/tcp", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn fuser_kill_owned_by_self_allows() {
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(
            check("fuser -k 3000/tcp", &leases, Some(777), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn fuser_kill_with_dead_lease_allows() {
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(
            check("fuser -k 3000/tcp", &leases, None, None, &AlwaysDead),
            Verdict::Allow
        );
    }

    #[test]
    fn fuser_without_kill_flag_allows() {
        // Read-only: lists processes on the port, doesn't kill them.
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(
            check("fuser 3000/tcp", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn bare_fuser_kill_numeric_arg_without_tcp_or_udp_suffix_allows() {
        // Real `fuser -k 3000` (no /tcp or /udp) treats "3000" as a
        // FILENAME argument (kill processes with that file open), not a
        // port — fuser only understands port numbers with an explicit
        // `/tcp` or `/udp` suffix. Must not be mistaken for a port kill.
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(
            check("fuser -k 3000", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn fuser_kill_with_udp_suffix_denies() {
        let leases = vec![lease(3000, 777, "vite-dev")];
        let verdict = check("fuser -k 3000/udp", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== npx kill-port <port> / kill-port <port> ====

    #[test]
    fn kill_port_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 888, "storybook")];
        let verdict = check("kill-port 3000", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny {
                port, owner_pid, ..
            } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 888);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn kill_port_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check("kill-port 3000", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_port_owned_by_self_allows() {
        let leases = vec![lease(3000, 888, "storybook")];
        assert_eq!(
            check("kill-port 3000", &leases, Some(888), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_port_with_dead_lease_allows() {
        let leases = vec![lease(3000, 888, "storybook")];
        assert_eq!(
            check("kill-port 3000", &leases, None, None, &AlwaysDead),
            Verdict::Allow
        );
    }

    #[test]
    fn npx_kill_port_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 888, "storybook")];
        let verdict = check("npx kill-port 3000", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn npx_dash_y_kill_port_denies() {
        let leases = vec![lease(3000, 888, "storybook")];
        let verdict = check("npx -y kill-port 3000", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn npx_dash_dash_yes_kill_port_denies() {
        let leases = vec![lease(3000, 888, "storybook")];
        let verdict = check(
            "npx --yes kill-port 3000",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== absolute-path / env-prefixed commands ====

    #[test]
    fn absolute_path_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("/usr/bin/kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn relative_path_to_an_unrelated_binary_named_like_kill_does_not_trigger() {
        // Basenaming the verb must still require an EXACT match against the
        // basename — `my-killer-app` is not `kill`, wherever it lives.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("./my-killer-app 1234", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn env_var_prefixed_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("FOO=1 kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn multiple_env_var_prefixes_before_kill_still_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("FOO=1 BAR=baz kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== non-kill commands / detection boundaries ====

    #[test]
    fn ordinary_non_kill_command_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check("git log --oneline", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn empty_command_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(check("", &leases, None, None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn killall_whatever_substring_does_not_trigger_killall_detection() {
        // `killall-whatever` is one token, not the `killall` command with an
        // argument — must not be treated as a kill intent at all.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("killall-whatever 3000", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn quoted_kill_inside_echo_does_not_trigger_detection() {
        // Documented boundary: this is NOT a real shell parser. The first
        // word of the (only) segment is `echo`, not `kill`, so this is
        // never flagged — even though the string "kill 123" appears inside
        // the command. See the module doc comment.
        let leases = vec![lease(3000, 123, "dev-server")];
        assert_eq!(
            check("echo \"kill 123\"", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn script_name_containing_kill_as_a_substring_does_not_trigger() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(
            check("npm run kill-server", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn unrelated_command_mentioning_a_leased_port_number_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check(
                "curl -X DELETE http://localhost:3000",
                &leases,
                None,
                None,
                &AlwaysAlive
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_as_second_pipeline_segment_after_unrelated_command_still_resolves() {
        // Detection is per-pipeline-segment: an unrelated first segment
        // shouldn't stop a real `kill` in a later segment from being seen.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check(
            "echo about to clean up; kill 1234",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== sh -c unwrap (core normalization, covers the hook path) ====

    #[test]
    fn sh_c_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("sh -c 'kill 1234'", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn bash_c_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("bash -c 'kill 1234'", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn sh_combined_flags_c_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("sh -lc 'kill 1234'", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn nested_sh_c_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check(
            "sh -c \"sh -c 'kill 1234'\"",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn sh_c_wrapped_pipeline_kill_with_foreign_live_lease_denies() {
        // The unwrap must hand the whole payload to segment detection, so a
        // pipeline INSIDE the payload is still correlated.
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check(
            "sh -c 'lsof -ti:3000 | xargs kill'",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn sh_c_wrapped_kill_owned_by_self_allows() {
        // Normalization changes HOW the command is read, not the ownership
        // rules that apply once it is read.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("sh -c 'kill 1234'", &leases, Some(1234), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn sh_c_after_another_statement_is_a_documented_boundary() {
        // The `sh -c` unwrap applies only at the very start of the command
        // string (same shape the universal `guard` wrapper unwraps). A shell
        // invocation that only appears after another statement is not
        // unwrapped — an accepted false negative, see the module doc.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check(
                "echo hi && sh -c 'kill 1234'",
                &leases,
                None,
                None,
                &AlwaysAlive
            ),
            Verdict::Allow
        );
    }

    // ==== wrapper-prefix stripping (env / command / exec / nohup / builtin) ====

    #[test]
    fn env_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("env kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn env_with_ignore_environment_flag_wrapped_kill_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("env -i kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn env_with_assignment_wrapped_kill_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("env FOO=1 kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn env_with_unset_flag_wrapped_kill_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("env -u FOO kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn command_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("command kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn exec_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("exec kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn nohup_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("nohup kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn builtin_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("builtin kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn sudo_env_wrapped_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("sudo env kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn env_assignment_then_command_wrapped_kill_denies() {
        // Combinations must strip repeatedly until the real verb is exposed.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check(
            "env FOO=1 command kill 1234",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn wrapper_stripping_is_per_pipeline_segment() {
        // Same per-segment scope as the existing verb detection: an unrelated
        // first segment must not hide a wrapped kill in a later one.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check(
            "echo cleaning up; env kill 1234",
            &leases,
            None,
            None,
            &AlwaysAlive,
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== false-positive guards: wrappers are not verbs ====

    #[test]
    fn bare_env_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("env", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn bare_command_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("command", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn bare_nohup_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("nohup", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn command_dash_v_locating_kill_allows() {
        // `command -v kill` only LOCATES the binary, it never executes it —
        // and more generally a wrapper followed by its own flags is a
        // documented false-negative boundary (see the module doc), never a
        // reason to deny.
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("command -v kill", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn echo_mentioning_env_kill_does_not_trigger() {
        // `env` appearing anywhere but the wrapper position must not strip
        // anything: the segment's first word is still `echo`.
        let leases = vec![lease(3000, 123, "dev-server")];
        assert_eq!(
            check("echo env kill 123", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn nohup_wrapping_a_non_kill_command_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("nohup server", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    // ==== robustness: never panics ====

    #[test]
    fn never_panics_on_adversarial_wrapper_and_shell_input() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let adversarial_inputs = [
            "env",
            "env -i",
            "env -u",
            "env -u FOO",
            "env FOO=1",
            "command",
            "exec",
            "nohup",
            "builtin",
            "sudo env",
            "sudo env command",
            "sh -c",
            "sh -c ''",
            "sh -c '",
            "sh -c \"",
            "sh -c 'kill",
            "bash -c",
            "env sh -c 'kill 1234'",
            &"sh -c 'kill 1234'; ".repeat(200),
            &{
                // A pathologically deep sh -c nest must hit the recursion
                // bound and stop, not blow the stack.
                let mut payload = "kill 1234".to_string();
                for _ in 0..50 {
                    payload = format!("sh -c '{payload}'");
                }
                payload
            },
        ];
        for input in adversarial_inputs {
            let _ = check(input, &leases, None, None, &AlwaysAlive);
            let _ = check(input, &leases, Some(1234), None, &AlivePids(vec![1234]));
        }
    }

    #[test]
    fn never_panics_on_adversarial_input() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        let adversarial_inputs = [
            "",
            "   ",
            "|||||",
            "kill",
            "kill -",
            "kill -9",
            "kill kill kill",
            "lsof",
            "lsof -ti:",
            "lsof -ti:999999999999999999999999 | xargs kill",
            "fuser -k /tcp",
            "kill-port",
            &"kill 1234; ".repeat(500),
            "kill\u{0}1234",
        ];
        for input in adversarial_inputs {
            let _ = check(input, &leases, None, None, &AlwaysAlive);
            let _ = check(input, &leases, Some(1234), None, &AlivePids(vec![1234]));
        }
    }
}
