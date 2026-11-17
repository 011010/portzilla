# Add reliable agent server launches

Add a `portzilla run` command and a companion agent skill so an AI agent can start a local server on a claimed port without losing the connection between the lease and the real server process.

## Quick path

1. Install the generated skill in the consumer project.
2. Let the skill start a server through `portzilla run`.
3. Use the `PORTZILLA_PORT` environment variable in the server command.

The command reserves a port before the server starts. If the requested port is unavailable, it sets `PORTZILLA_PORT` to the reassigned port and starts the server with that value.

## Problem

`portzilla claim` records a PID. A skill cannot safely claim a port before launching a server and then hand that lease to a different server PID: a live lease is only updated in place for the same PID. Claiming after a server binds is also unsafe because occupancy detection reassigns the requested port.

This is especially visible with agent Bash calls. The shell that runs a pre-claim can be short-lived while the development server keeps running in the background. The lease then becomes dead even though the server still owns the port.

## Scope

| Area | Decision |
| --- | --- |
| Process lifecycle | Add `portzilla run` to claim and launch one server command. |
| Dynamic ports | Inject the actual claimed port as `PORTZILLA_PORT`. |
| Server ownership | Keep the wrapper lease alive while the child starts, then transfer it to the verified child PID through a private store operation. |
| Agent workflow | Add an installable, portable `portzilla` skill. |
| OpenCode guard | Keep the existing opt-in plugin shim unchanged. |

Out of scope: a daemon, automatic framework detection, automatic configuration writes, cross-machine coordination, and a generic server-command parser.

## Command design

```console
portzilla run <PORT> --tag <TAG> [--session <SESSION>] -- <COMMAND> [ARGS...]
```

`run` uses the current Portzilla process PID for the initial claim. It uses the same conflict and OS-occupancy logic as `claim`.

Before launching the command, it adds these variables to its environment:

```text
PORTZILLA_PORT=<actual claimed port>
PORTZILLA_SESSION=<session, when supplied>
```

The command writes its claim or reassignment notice to stderr. Stdout belongs to the launched server, so `run` deliberately does not offer the single-value `--json` contract used by data-producing commands.

### Ownership transfer

`run` starts the requested child while its own lease is still live, then transfers that lease to the child's verified PID and waits for the child. The transfer does not repeat OS occupancy probing because the wrapper already owns the lease. It verifies both the wrapper identity and the child's process start time under the store lock, so an unrelated process cannot take the lease through this internal path. If the child cannot be verified or the transfer fails, `run` terminates the child where possible and returns an error rather than reporting a protected server.

When the server exits, its lease becomes dead and is reusable by a later claim. `portzilla watch` remains an optional foreground cleanup tool; `run` does not add a daemon or promise immediate deletion.

## Skill design

The source-of-truth skill lives at `skills/portzilla/SKILL.md`. `portzilla init skill` prints that exact content so users can install it explicitly:

```console
mkdir -p .opencode/skills/portzilla
portzilla init skill > .opencode/skills/portzilla/SKILL.md
```

OpenCode discovers that standard project location. The command only prints content; it never writes user or project configuration.

The skill must:

- Trigger for starting, stopping, checking, or resolving a local development-server port.
- Verify `portzilla` is on `PATH` before depending on it.
- Discover the target framework's port configuration before starting a server.
- Use `portzilla run`, with a command that consumes `PORTZILLA_PORT`.
- Report a reassigned port clearly to the user.
- Use `who`, `ls`, and MCP tools for inspection.
- Never emulate `run` with a pre-claim from an ephemeral shell.
- Treat `release` as destructive and run it only for an explicitly known lease after checking `who`.
- Direct users to `portzilla init opencode` when they explicitly request kill-guard setup.

## Error handling

| Condition | Behavior |
| --- | --- |
| `portzilla` unavailable | Stop before starting the server and present an installation command. |
| Requested port unavailable | Start with the reassigned value in `PORTZILLA_PORT`; do not silently use the requested port. |
| Server command ignores `PORTZILLA_PORT` | Do not claim success; adapt the framework command or ask for the required configuration. |
| Initial claim fails | Do not launch the server. |
| Windows lease transfer fails | Stop the child where possible, return an error, and do not report a protected server. |
| Unknown lease during cleanup | Do not release it automatically. |

## Verification

- Unit-test command parsing and `PORTZILLA_PORT`/session environment construction.
- Unit-test the identity-checked transfer path and rejection cases.
- Add cross-platform integration coverage that launches a fixture command, observes its environment, and confirms the recorded lease belongs to the running process.
- Verify reassignment passes the actual port to the fixture.
- Verify the lease is dead or prunable after the fixture exits.
- Verify `portzilla init skill` output matches the checked-in `SKILL.md`.

## Acceptance checklist

- [ ] `portzilla run` never launches a command if it cannot create a lease.
- [ ] A launched server owns the lease after a guarded transfer on every supported platform.
- [ ] Server stdout remains unmodified by Portzilla control output.
- [ ] The skill uses the actual assigned port and never relies on an ephemeral pre-claim.
- [ ] Skill installation is explicit and does not write configuration implicitly.
