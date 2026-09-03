---
name: portzilla
description: Coordinate local dev-server ports across parallel agent sessions. Use when starting, stopping, checking, or resolving a local dev-server port.
---

# Portzilla — dev-server port coordination

Use this skill when starting, stopping, checking, or resolving a local dev-server port.

## Prerequisites

- `portzilla` must be on PATH. Verify with `portzilla --version` before doing anything else.

## Starting a dev server

1. Inspect the project (its manifest, README, and existing scripts) to choose the right dev-server command for this project.
2. The chosen command must consume the assigned port from the `PORTZILLA_PORT` environment variable.
3. Launch it in one step with `portzilla run <port> --tag "<what it is>" -- <command>`. Do not claim the port first — `run` claims it and holds the lease for the server process.
4. If the requested port is busy, `run` reassigns to the next free port and reports it on stderr. Always report the actual assigned port (`PORTZILLA_PORT`) to the user, especially when it differs from the requested one.

## Inspecting and stopping

- Check a single port with `portzilla who <port>`; list everything with `portzilla ls`.
- Release a port only when you explicitly know it is yours to release (the user named it, or your own session claimed it): confirm ownership with `who` first, then `portzilla release <port>`.

## Kill-guard setup

This skill does not set up kill protection. If explicitly asked for kill-guard or hook setup, direct that request to `portzilla init opencode` instead.
