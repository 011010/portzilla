# Kill Guard

The kill guard intercepts kill-shaped commands before they run and blocks the ones that would kill another session's live, leased process — instead of relying on the agent to remember to run `who` first.

For a high-level overview and support matrix, see [`README.md`](../README.md#kill-guard).

## How it works

The guard itself (`src/guard.rs`) is harness-agnostic: it takes a shell command string and the current lease list and returns a verdict, with no knowledge of Claude Code, Cursor, Gemini CLI, Codex CLI, Kimi CLI, OpenCode, Windsurf, or any other tool. Each harness gets a thin adapter over the same core (`src/claude_code.rs`, `src/cursor.rs`, `src/gemini.rs`, `src/codex.rs`, `src/kimi.rs`, `src/opencode.rs`, `src/windsurf.rs`) that only translates the harness's hook payload in and its response shape out — adding a new harness means writing another adapter like these, never touching the guard itself.

### Adapter boundary

All seven adapters own their harness payload parsing and wire-compatible
response rendering. Claude Code, Codex, Cursor, and Gemini CLI use JSON
contracts; Kimi CLI and Windsurf use exit-code contracts; OpenCode uses its
`{ action, reason }` JSON protocol. This includes each harness's stdout/stderr
behavior and exit codes. The shared `hook_common::evaluate` path delegates
normalized command evaluation to the guard core. External contracts remain
harness-specific: adapters do not share a response schema or impose one
harness's failure and exit semantics on another.

It recognizes:

- `kill <pid>` / `kill -9 <pid>` — explicit PIDs, including `sudo kill ...`, `FOO=1 kill ...` (leading env-var assignments), and `/usr/bin/kill ...` (matched by basename)
- `pkill <name>` / `killall <name>` — process names (can't be resolved to a specific lease, so this only ever warns, never denies)
- `lsof -ti:<port> | xargs kill` (and the `lsof -ti :<port>` / `-9` variants) — only when actually piped together, not merely run one after another with `;`/`&&`
- `fuser -k <port>/tcp` (and `/udp`) — a bare `fuser -k <port>` with no protocol suffix is not treated as a port kill, matching real `fuser`'s own argument parsing
- `npx kill-port <port>` / `kill-port <port>` (including `npx -y kill-port ...` / `npx --yes kill-port ...`)

Leading wrapper prefixes are stripped before the verb is read, so the kill intent is detected whether it appears directly or behind one of `env`, `command`, `exec`, `nohup`, `builtin`, or any combination with `sudo` — `sudo env FOO=1 command kill <pid>` resolves to `kill <pid>`. A leading `sh -c '<payload>'` invocation is also unwrapped (the same shape the universal `guard` wrapper handles for argv), so a kill hidden inside the payload is detected through the hook adapters too.

For each, it resolves the target PID or port against the lease registry and decides:

- **Allow** — no kill intent detected, the target has no live lease, the lease is dead, or the lease is yours (you're allowed to kill/restart your own claimed process — see "Session ownership" below).
- **Deny** — the target has a live lease owned by someone else. The explanation names the port, PID, and tag, and says what to do instead: check `portzilla who <port>`, claim a different port, or ask before proceeding if the lease looks stale.
- **Warn** — a kill intent was detected but can't be resolved to a specific port/PID (killing by process name). portzilla can't verify safety here, so it surfaces a warning instead of silently allowing or incorrectly denying.

This is intentionally not a real shell parser — see the module doc comment in `src/guard.rs` for the exact approximation (split on `|`/`;`/`&&`/`||`, look at each segment's first word) and its tested boundaries, e.g. `killall-whatever` doesn't trigger `killall` detection, and `echo "kill 123"` doesn't trigger `kill` detection. False negatives are an accepted tradeoff; false positives are not.

## Setup per harness

### Claude Code

```console
$ portzilla init claude-code
```

prints the exact `.claude/settings.json` snippet to add (project-level `.claude/settings.json` or user-level `~/.claude/settings.json`), registering `portzilla hook claude-code` as a `PreToolUse` hook on the `Bash` tool. This only prints instructions — it never writes to your settings file.

For session ownership to resolve end to end, claim with the session id Claude Code exposes to Bash tool subprocesses (verified against the Claude Code environment variables reference — `CLAUDE_CODE_SESSION_ID` "matches the `session_id` field in the hook JSON input" for Bash, PowerShell, and hook subprocesses):

```console
$ portzilla claim 3000 --tag "vite dev" --session "$CLAUDE_CODE_SESSION_ID"
```

### Cursor

```console
$ portzilla init cursor
```

prints the `.cursor/hooks.json` snippet (project `.cursor/hooks.json` or user `~/.cursor/hooks.json`), registering `portzilla hook cursor` as a `beforeShellExecution` hook. Cursor's own hook runner already fails open by default on a hook crash/timeout/invalid-JSON (unless `failClosed: true` is set, which this setup does not ask for).

Cursor does not currently expose `conversation_id` to the shell commands it runs — only to the hook payload itself (verified: the hooks doc's own "Environment Variables" table lists `CURSOR_PROJECT_DIR`, `CURSOR_VERSION`, `CURSOR_USER_EMAIL`, `CURSOR_TRANSCRIPT_PATH`, `CURSOR_CODE_REMOTE`, and `CLAUDE_PROJECT_DIR` — none of them a session/conversation id, and that table is explicitly scoped to what the hook script receives, not the agent's own commands). So there is currently no `--session` value a Cursor-driven claim can use to be recognized as its own later. Foreign-lease protection still works fully; own-lease recognition doesn't yet, pending Cursor documenting such a variable.

### Gemini CLI

```console
$ portzilla init gemini
```

prints the `.gemini/settings.json` snippet (project `.gemini/settings.json`, user `~/.gemini/settings.json`, or system `/etc/gemini-cli/settings.json`), registering `portzilla hook gemini` as a `BeforeTool` hook matched on the `run_shell_command` tool.

Two things are worth knowing here, both found during verification rather than assumed: first, hook registration lives in `settings.json`, not a standalone `hooks/hooks.json` file inside an extension directory — the current reference documents only the `settings.json`-based mechanism. Second, `run_shell_command`'s own subprocess (the agent's actual shell commands) is only ever given `GEMINI_CLI=1` — a bare presence flag, not a session id (`GEMINI_SESSION_ID` exists, but is documented as available to hook script subprocesses, not shell-tool subprocesses) — so, same as Cursor, own-lease recognition isn't available yet.

### Codex CLI

```console
$ portzilla init codex
```

prints the exact `.codex/hooks.json` snippet to add (project-level `.codex/hooks.json` or user-level `~/.codex/hooks.json`), registering `portzilla hook codex` as a `PreToolUse` hook matched on `Bash`. Codex's hooks are [generally available](https://developers.openai.com/codex/hooks); project-level hooks are only loaded once the project's `.codex/` layer is trusted (Codex asks you to review/trust new hooks — see its `/hooks` command).

Codex's own env-vars reference does not expose the session id to the shell commands the agent runs — only to the hook payload itself — so, same as Cursor and Gemini CLI, own-lease recognition isn't available yet. Deny reasons do reach the model (`permissionDecisionReason`), and `Warn` verdicts ride the `additionalContext` field Codex adds to the model's context.

### Kimi CLI

```console
$ portzilla init kimi
```

prints the exact `~/.kimi/config.toml` snippet to add, registering `portzilla hook kimi` as a `[[hooks]]` entry on `PreToolUse` matched on `Shell`. Kimi's hooks are Beta (implementation details may change), and Kimi CLI is being [wound down in favor of Kimi Code CLI](https://github.com/MoonshotAI/kimi-cli) — re-verify this integration when adopting the successor.

Kimi's `PreToolUse` contract is exit-code driven rather than JSON-response driven: `portzilla hook kimi` exits 2 (with the reason on stderr, which Kimi feeds back to the model) to deny, and prints `Warn` explanations as plain text on stdout, which Kimi adds to the model's context on allow. As with Cursor/Gemini/Codex, the session id reaches the hook payload but not the agent's own shell commands, so own-lease recognition isn't available yet.

### OpenCode

```console
$ portzilla init opencode
```

prints the full source of the `portzilla.js` plugin shim to save as `.opencode/plugin/portzilla.js` (project) or `~/.config/opencode/plugin/portzilla.js` (user), then restart OpenCode — plugins load once at startup, not hot-reloaded. Your editor needs `portzilla` on PATH. OpenCode hooks run in-process as JS/TS plugin modules, which is why this harness uses a shim instead of running `portzilla` as the hook directly.

The shim hooks `tool.execute.before` for the `bash` tool, shells out to `portzilla hook opencode`, and throws with the deny reason (OpenCode surfaces it to the model as a tool error) or defers a warn to `tool.execute.after` so the model sees it on the tool result without blocking. Because the shim also adds a `shell.env` hook that injects `PORTZILLA_SESSION` into every bash subprocess, this is the only non-Claude harness where end-to-end own-lease recognition works:

```console
$ portzilla claim 3000 --tag "vite dev" --session "$PORTZILLA_SESSION"
```

### Windsurf

```console
$ portzilla init windsurf
```

prints the exact `.windsurf/hooks.json` snippet to add, registering `portzilla hook windsurf` as a `pre_run_command` hook. Cascade Hooks' contract is exit-code driven: exit 2 blocks and the message on stderr reaches the agent, exit 0 allows, and Windsurf treats any other exit code as allow (fail-open). Windsurf has no non-blocking model-visible warn channel, so `Verdict::Warn` rides stderr on exit 0 — never blocks, and is visible to a human when `show_output: true` is set. `trajectory_id` reaches the hook payload but not the agent's own shell commands, so own-lease recognition isn't available yet. Note that Cascade hooks don't load at all while a workspace is open in Restricted Mode.

### Anything else: `portzilla guard`

For harnesses with no hook mechanism (Aider), and for a human or a script that just wants the guard in front of a command:

```console
$ portzilla guard -- kill 57107
portzilla guard: blocked — Port 3000 is leased to pid 57107 (tag: "next-dev") — a live process owned by another session, not a stale one. ...
$ echo $?
2
```

- **Deny** — the command is not executed at all; the explanation goes to stderr and `portzilla guard` exits `2`.
- **Warn** — a warning goes to stderr, then the command runs.
- **Allow** — the command runs with no extra output.
- `--session <S>`, or the `PORTZILLA_SESSION` environment variable if `--session` is omitted, enables own-lease recognition the same way `--session` does for `claim`.
- The command after `--` is executed directly — no shell, so pipes and compounds are not interpreted by `portzilla guard` itself. On Unix this replaces the current process via `exec`, so exit codes pass straight through as if `portzilla guard` were never there (and if the command itself can't be started, `portzilla guard` exits with the same POSIX convention a shell would: `127` if it wasn't found, `126` for any other reason — not executable, permission denied, etc.). For a pipe or compound command, wrap it in `sh -c`:

  ```console
  $ portzilla guard -- sh -c 'lsof -ti:3000 | xargs kill'
  ```

  `portzilla guard` recognizes an `sh`/`bash`/`zsh`/`dash` invocation with a `-c`-family flag (combined, like `-lc`, or separate, like `-x -c` or `--norc -c`) ahead of the payload, and analyzes the raw payload directly instead of the literal `sh -c ...` text — including recursively through a nested `sh -c "sh -c '...'"`, up to 8 levels deep. This is a targeted unwrap for that specific shape, not a shell parser: it does not follow `$(...)` command substitution, variable expansion, or backslash escaping inside the payload, and only recognizes `sh`/`bash`/`zsh`/`dash` by name (not `ksh`, `fish`, `python3 -c`, PowerShell's `-Command`, etc.) — see `src/guard_cmd.rs`'s module doc comment for the complete, current list of what it does and doesn't catch.

## Failure modes

By default, portzilla-side failures in every adapter (`hook claude-code`, `hook codex`, `hook cursor`, `hook gemini`, `hook kimi`, `hook opencode`, and `hook windsurf`) and `guard` fail open: unreadable stdin, malformed hook JSON, an unreadable lease store, and internal panics are handled through the harness's allow path with a diagnostic where supported. The harness's normal permission flow then applies as if the hook were not installed.

## Fail-closed mode (opt-in)

Set `PORTZILLA_FAIL_CLOSED=1` to convert those portzilla-side failures to a deny according to each adapter's contract. The JSON adapters use their documented deny fields, Kimi CLI and Windsurf use exit 2 with stderr, and OpenCode returns an `action: "deny"` JSON verdict. `portzilla guard` exits 2 without running the command. Default mode remains fail-open; this opt-in is for users who prefer blocking when safety cannot be verified.
