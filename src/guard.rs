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
            Some(lease) if lease.is_alive(checker) && !owned_by_self(lease, self_pid, self_session) => {
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
    let session_match = self_session.is_some_and(|session| lease.session.as_deref() == Some(session));
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
/// See the module doc comment for the detection approximation and its
/// documented boundaries.
fn detect_target(command: &str) -> Option<KillTarget> {
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

/// Splits `command` into pipeline/sequencing segments on `|`, `;`, `&&`, and
/// `||`, with no distinction between the separators. Used by [`detect_segment`],
/// which only cares about "is this segment's first word a kill verb" and
/// doesn't need to know whether the previous separator was a pipe or a
/// sequencer. Not shell-aware: does not respect quoting, so a literal `|`
/// inside a quoted string would also split there. See the module doc comment.
fn split_segments(command: &str) -> Vec<&str> {
    split_statements(command).into_iter().flat_map(split_pipeline).collect()
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
    statement.split('|').map(str::trim).filter(|s| !s.is_empty()).collect()
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

    while tokens.first().is_some_and(|token| basename(token) == "sudo") {
        tokens = &tokens[1..];
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
        "kill-port" => rest.iter().find_map(|token| token.parse::<u16>().ok()).map(KillTarget::Port),
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
    use crate::lease::test_support::{AlivePids, AlwaysAlive, AlwaysDead};
    use crate::lease::Lease;

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
            Verdict::Deny { port, owner_pid, tag, explanation } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 1234);
                assert_eq!(tag, "dev-server");
                assert!(explanation.contains("3000"));
                assert!(explanation.contains("who"), "explanation should point at `who`: {explanation}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn kill_pid_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(check("kill 1234", &leases, None, None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn kill_pid_owned_by_self_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(check("kill 1234", &leases, Some(1234), None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn kill_pid_with_dead_lease_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(check("kill 1234", &leases, None, None, &AlwaysDead), Verdict::Allow);
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
        assert_eq!(check("kill 1234", &leases, Some(1234), None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn matching_self_session_allows_even_with_a_mismatched_self_pid() {
        // Session match alone is sufficient — a wrong/irrelevant self_pid
        // must not override it.
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        assert_eq!(
            check("kill 1234", &leases, Some(9999), Some("session-a"), &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn neither_self_pid_nor_self_session_matching_denies() {
        let leases = vec![lease_with_session(3000, 1234, "dev-server", "session-a")];
        let verdict = check("kill 1234", &leases, Some(9999), Some("session-b"), &AlwaysAlive);
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
        assert_eq!(check("kill -l", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
        let verdict = check("lsof -ti:3000 | xargs kill", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny { port, owner_pid, .. } => {
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
            check("lsof -ti:3000 | xargs kill", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_xargs_kill_owned_by_self_allows() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(
            check("lsof -ti:3000 | xargs kill", &leases, Some(5555), None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_xargs_kill_with_dead_lease_allows() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(
            check("lsof -ti:3000 | xargs kill", &leases, None, None, &AlwaysDead),
            Verdict::Allow
        );
    }

    #[test]
    fn lsof_space_colon_variant_with_dash_9_denies() {
        let leases = vec![lease(3000, 5555, "next-dev")];
        let verdict = check("lsof -ti :3000 | xargs kill -9", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn bare_lsof_without_xargs_kill_allows() {
        // Just inspecting, not killing anything.
        let leases = vec![lease(3000, 5555, "next-dev")];
        assert_eq!(check("lsof -ti:3000", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
        let verdict = check("lsof -ti:3000 | xargs kill; echo done", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== fuser -k <port>/tcp ====

    #[test]
    fn fuser_kill_with_foreign_live_lease_denies() {
        let leases = vec![lease(3000, 777, "vite-dev")];
        let verdict = check("fuser -k 3000/tcp", &leases, None, None, &AlwaysAlive);
        match verdict {
            Verdict::Deny { port, owner_pid, .. } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 777);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn fuser_kill_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(check("fuser -k 3000/tcp", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
        assert_eq!(check("fuser -k 3000/tcp", &leases, None, None, &AlwaysDead), Verdict::Allow);
    }

    #[test]
    fn fuser_without_kill_flag_allows() {
        // Read-only: lists processes on the port, doesn't kill them.
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(check("fuser 3000/tcp", &leases, None, None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn bare_fuser_kill_numeric_arg_without_tcp_or_udp_suffix_allows() {
        // Real `fuser -k 3000` (no /tcp or /udp) treats "3000" as a
        // FILENAME argument (kill processes with that file open), not a
        // port — fuser only understands port numbers with an explicit
        // `/tcp` or `/udp` suffix. Must not be mistaken for a port kill.
        let leases = vec![lease(3000, 777, "vite-dev")];
        assert_eq!(check("fuser -k 3000", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
            Verdict::Deny { port, owner_pid, .. } => {
                assert_eq!(port, 3000);
                assert_eq!(owner_pid, 888);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn kill_port_with_no_lease_allows() {
        let leases: Vec<Lease> = vec![];
        assert_eq!(check("kill-port 3000", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
        assert_eq!(check("kill-port 3000", &leases, None, None, &AlwaysDead), Verdict::Allow);
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
        let verdict = check("npx --yes kill-port 3000", &leases, None, None, &AlwaysAlive);
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
        assert_eq!(check("git log --oneline", &leases, None, None, &AlwaysAlive), Verdict::Allow);
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
        assert_eq!(check("npm run kill-server", &leases, None, None, &AlwaysAlive), Verdict::Allow);
    }

    #[test]
    fn unrelated_command_mentioning_a_leased_port_number_allows() {
        let leases = vec![lease(3000, 1234, "dev-server")];
        assert_eq!(
            check("curl -X DELETE http://localhost:3000", &leases, None, None, &AlwaysAlive),
            Verdict::Allow
        );
    }

    #[test]
    fn kill_as_second_pipeline_segment_after_unrelated_command_still_resolves() {
        // Detection is per-pipeline-segment: an unrelated first segment
        // shouldn't stop a real `kill` in a later segment from being seen.
        let leases = vec![lease(3000, 1234, "dev-server")];
        let verdict = check("echo about to clean up; kill 1234", &leases, None, None, &AlwaysAlive);
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    // ==== robustness: never panics ====

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
