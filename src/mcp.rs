//! MCP (Model Context Protocol) server exposing portzilla's lease operations
//! as tools over stdio, so an AI agent with MCP access calls `claim`/`who`/
//! `ls`/`release`/`prune` the same way it calls any other structured tool —
//! typed JSON in, typed JSON out — instead of shelling out to the CLI and
//! parsing text.
//!
//! Every tool result carries the exact same flat JSON shape the CLI's
//! `--json` flag emits (see `crate::view`), so anything already written
//! against the CLI's JSON output recognizes MCP tool results too.

use crate::lease::SystemPidChecker;
use crate::store::Store;
use crate::view::{to_claim_view, to_view};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

/// Starts the MCP server on stdio and runs until the client disconnects.
///
/// Opens the same [`Store`] the CLI uses (respecting `PORTZILLA_DATA_DIR`
/// and the rest of the data-directory resolution order) — the MCP server is
/// a second front end onto the same on-disk state, not a separate one.
pub async fn run_stdio() -> anyhow::Result<()> {
    let store = Store::open(None)?;
    let server = PortzillaMcpServer::new(store);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
struct PortzillaMcpServer {
    store: Store,
    tool_router: ToolRouter<PortzillaMcpServer>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClaimParams {
    /// The port to claim.
    port: u16,
    /// Human-readable description of what this port is for (e.g. "next-dev", "vite-preview").
    tag: String,
    /// PID of the process that owns this port. If omitted, defaults to the
    /// portzilla MCP server's own PID — which is almost never the right
    /// owner, since the server process is not the process actually bound to
    /// the port. Prefer passing the PID of the process you started (or are
    /// about to start) on this port so other tools/agents can see the real
    /// owner. The result includes a `note` field when this default was used.
    #[serde(default)]
    pid: Option<u32>,
    /// Optional session identifier grouping related leases together.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PortParams {
    /// The port to look up.
    port: u16,
}

#[tool_router]
impl PortzillaMcpServer {
    fn new(store: Store) -> Self {
        Self {
            store,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Claim a local port for a process, recording who owns it and why. Use \
        this INSTEAD of killing whatever occupies a port. If the port is free (or its \
        previous owner has already exited), you get it directly. If it's genuinely held by a \
        live process, you are NOT allowed to steal it — you are given the next free port \
        instead, and the result says so. Call `who` first if you need to know who currently \
        holds a busy port before deciding what to do."
    )]
    async fn claim(
        &self,
        Parameters(params): Parameters<ClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        // Input validation surfaces as INVALID_PARAMS protocol errors (bad
        // tool arguments, per JSON-RPC), distinct from store failures
        // (INTERNAL errors, portzilla itself couldn't do its job). The store
        // re-validates defensively; this check exists so the error CODE is
        // right at the protocol level.
        if let Err(err) =
            crate::store::validate_claim_inputs(params.port, &params.tag, params.session.as_deref())
        {
            return Err(McpError::invalid_params(format!("{err:#}"), None));
        }

        let pid_was_given = params.pid.is_some();
        let pid = params.pid.unwrap_or_else(std::process::id);
        let store = self.store.clone();
        let requested_port = params.port;
        let tag = params.tag;
        let session = params.session;

        // `Store::claim` takes an fs4 file lock and does blocking file I/O;
        // running it directly on an async worker thread would tie that
        // thread up for the duration, starving any other tool call the
        // executor could otherwise be servicing. `spawn_blocking` moves it
        // onto tokio's dedicated blocking thread pool instead.
        let mut value = tokio::task::spawn_blocking(move || {
            store
                .claim(requested_port, pid, tag, session, &SystemPidChecker)
                .map(|outcome| {
                    serde_json::to_value(to_claim_view(&outcome, requested_port))
                        .expect("ClaimView always serializes")
                })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;

        if !pid_was_given {
            value["note"] = json!(
                "pid was not provided; defaulted to the portzilla MCP server's own PID, which \
                 is almost never the right owner. Pass the PID of the process you started (or \
                 are about to start) on this port instead."
            );
        }
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Show the lease recorded for a port: pid, tag, session, age in seconds, \
        and whether the owning process is currently alive. Use this before assuming a busy \
        port is safe to reuse, or before killing whatever is on it — it tells you who actually \
        owns the port and why. Returns a tool-level error (not a protocol error) if no lease \
        is recorded for the port; that is a normal, expected result, not a failure."
    )]
    async fn who(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();
        let port = params.port;

        // See the comment on `claim`: `Store::get` + the liveness check both
        // do blocking work (file lock/read, sysinfo process-table walk).
        let found = tokio::task::spawn_blocking(move || {
            store.get(port).map(|lease| {
                lease.map(|lease| {
                    serde_json::to_value(to_view(&lease, &SystemPidChecker))
                        .expect("LeaseView always serializes")
                })
            })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;

        match found {
            Some(value) => Ok(CallToolResult::structured(value)),
            None => Ok(not_found_error(port)),
        }
    }

    #[tool(
        description = "List every lease currently recorded, with port, pid, tag, session, age \
        in seconds, and alive/dead status for each."
    )]
    async fn ls(&self) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();

        let value = tokio::task::spawn_blocking(move || {
            store.list().map(|leases| {
                let views: Vec<_> = leases
                    .iter()
                    .map(|lease| to_view(lease, &SystemPidChecker))
                    .collect();
                serde_json::to_value(views).expect("Vec<LeaseView> always serializes")
            })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;

        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Remove the lease recorded for a port. If the owning process is still \
        alive, the lease is released anyway — this tool does not refuse or ask for \
        confirmation, since calling it is itself the explicit request. The result flags \
        whether the owning process was still alive at the moment of release. Returns a \
        tool-level error (not a protocol error) if no lease is recorded for the port."
    )]
    async fn release(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();
        let port = params.port;

        let released = tokio::task::spawn_blocking(move || {
            store.release(port, &SystemPidChecker).map(|outcome| {
                outcome.map(|outcome| {
                    let mut value =
                        serde_json::to_value(to_view(&outcome.lease, &SystemPidChecker))
                            .expect("LeaseView always serializes");
                    value["was_alive"] = json!(outcome.was_alive);
                    value
                })
            })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;

        match released {
            Some(value) => Ok(CallToolResult::structured(value)),
            None => Ok(not_found_error(port)),
        }
    }

    #[tool(
        description = "Remove every lease whose owning process is no longer alive, and return \
        the leases that were removed (an empty list if none were dead)."
    )]
    async fn prune(&self) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();

        let value = tokio::task::spawn_blocking(move || {
            store.prune(&SystemPidChecker).map(|pruned| {
                let views: Vec<_> = pruned
                    .iter()
                    .map(|lease| to_view(lease, &SystemPidChecker))
                    .collect();
                serde_json::to_value(views).expect("Vec<LeaseView> always serializes")
            })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;

        Ok(CallToolResult::structured(value))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PortzillaMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // Deliberately NOT `Implementation::from_build_env()`: that
            // function's `env!()` calls are compiled into the `rmcp` crate
            // itself, so it always reports the SDK's own name/version
            // ("rmcp") rather than ours. `env!("CARGO_PKG_VERSION")` here is
            // expanded in THIS crate, so it resolves to portzilla's version.
            .with_server_info(Implementation::new("portzilla", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "portzilla coordinates ownership of localhost ports across parallel AI coding \
                 agent sessions running on the same machine. Before killing whatever is on a \
                 busy port, call `who` to see who actually owns it and why — it may be a \
                 sibling session's dev server, not a stale process. Call `claim` before \
                 starting a server on a port so other sessions can see it's yours; if the port \
                 turns out to be genuinely busy, `claim` hands you the next free port instead \
                 of stealing the busy one."
                    .to_string(),
            )
    }
}

/// Converts a store failure (I/O error, corrupt state, lock failure) into a
/// protocol-level MCP error. This is deliberately different from the
/// "lease not found" case (a tool-level `CallToolResult::structured_error`,
/// mirroring the CLI's exit code 2): a missing lease is an expected,
/// well-formed outcome the caller should see in the tool result; a store
/// failure means portzilla itself could not do its job, which is what
/// protocol-level errors are for.
fn store_error(err: anyhow::Error) -> McpError {
    // `{err:#}` (alternate Display) walks anyhow's full `.context()` chain,
    // same as the CLI's `eprintln!("error: {err:#}")`. Plain `{err}` /
    // `.to_string()` shows only the outermost message and silently drops
    // every layer of context the store attached (e.g. which file, what
    // operation) — the caller would see "invalid JSON" with no idea what
    // was invalid or why.
    McpError::internal_error(format!("{err:#}"), None)
}

/// Converts a `tokio::task::spawn_blocking` join failure (the blocking task
/// panicked, or was cancelled) into a protocol-level MCP error. This should
/// only happen on a bug (a panic inside store/lease code) or shutdown race,
/// never in normal operation.
fn blocking_task_failed(err: tokio::task::JoinError) -> McpError {
    McpError::internal_error(
        format!("internal error: blocking store task failed: {err}"),
        None,
    )
}

/// Builds the tool-level error result used for "no lease on this port"
/// (`who`, `release`), mirroring the CLI's exit code 2.
fn not_found_error(port: u16) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": "not_found",
        "port": port,
        "message": format!("no lease found for port {port}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ErrorCode;

    fn server_with_tempdir() -> (PortzillaMcpServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path().to_path_buf())).unwrap();
        (PortzillaMcpServer::new(store), dir)
    }

    fn structured(result: &CallToolResult) -> &serde_json::Value {
        result
            .structured_content
            .as_ref()
            .expect("expected structured_content on the tool result")
    }

    // ---- tool descriptions ----

    #[test]
    fn claim_tool_description_says_to_use_it_instead_of_killing() {
        let (server, _dir) = server_with_tempdir();
        let claim_tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "claim")
            .expect("claim tool must be registered");
        let description = claim_tool.description.unwrap_or_default();
        assert!(
            description.contains("INSTEAD of killing"),
            "claim tool description must tell agents to use it instead of killing a port's \
             occupant, got: {description}"
        );
    }

    #[test]
    fn all_five_tools_are_registered_with_descriptions() {
        let (server, _dir) = server_with_tempdir();
        let tools = server.tool_router.list_all();
        let names: std::collections::HashSet<&str> =
            tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            ["claim", "who", "ls", "release", "prune"]
                .into_iter()
                .collect()
        );
        for tool in &tools {
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "tool {} must have a non-empty description",
                tool.name
            );
        }
    }

    // ---- claim ----

    #[tokio::test]
    async fn claim_on_a_free_port_returns_success_with_the_ls_plus_reassigned_shape() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .claim(Parameters(ClaimParams {
                port: 4000,
                tag: "server".to_string(),
                pid: Some(100),
                session: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["port"], 4000);
        assert_eq!(value["pid"], 100);
        assert_eq!(value["tag"], "server");
        assert_eq!(value["requested_port"], 4000);
        assert_eq!(value["reassigned"], false);
        assert!(value.get("created_at").is_some());
    }

    #[tokio::test]
    async fn claim_without_pid_defaults_to_the_server_pid_and_adds_a_note() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .claim(Parameters(ClaimParams {
                port: 4001,
                tag: "server".to_string(),
                pid: None,
                session: None,
            }))
            .await
            .unwrap();

        let value = structured(&result);
        assert_eq!(value["pid"], std::process::id());
        let note = value["note"]
            .as_str()
            .expect("note field must be present and a string");
        assert!(!note.is_empty());
    }

    #[tokio::test]
    async fn claim_with_explicit_pid_has_no_note_field() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .claim(Parameters(ClaimParams {
                port: 4002,
                tag: "server".to_string(),
                pid: Some(100),
                session: None,
            }))
            .await
            .unwrap();

        let value = structured(&result);
        assert!(
            value.get("note").is_none(),
            "note must be absent when pid was given explicitly"
        );
    }

    #[tokio::test]
    async fn claim_conflicting_with_a_live_pid_reassigns() {
        let (server, _dir) = server_with_tempdir();
        let own_pid = std::process::id();
        server
            .claim(Parameters(ClaimParams {
                port: 4003,
                tag: "first".to_string(),
                pid: Some(own_pid),
                session: None,
            }))
            .await
            .unwrap();

        let result = server
            .claim(Parameters(ClaimParams {
                port: 4003,
                tag: "second".to_string(),
                pid: Some(own_pid + 1),
                session: None,
            }))
            .await
            .unwrap();

        let value = structured(&result);
        assert_eq!(value["reassigned"], true);
        assert_eq!(value["port"], 4004);
        assert_eq!(value["requested_port"], 4003);
    }

    #[tokio::test]
    async fn claim_with_port_zero_is_an_invalid_params_protocol_error() {
        let (server, _dir) = server_with_tempdir();

        let err = server
            .claim(Parameters(ClaimParams {
                port: 0,
                tag: "server".to_string(),
                pid: Some(100),
                session: None,
            }))
            .await
            .expect_err("port 0 must be a protocol-level error");

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        let message = err.message.to_string();
        assert!(
            message.contains("port must be between 1 and 65535"),
            "error message must name the valid port range, got: {message}"
        );
    }

    #[tokio::test]
    async fn claim_with_an_oversized_tag_is_a_protocol_error() {
        let (server, _dir) = server_with_tempdir();

        let err = server
            .claim(Parameters(ClaimParams {
                port: 4010,
                tag: "a".repeat(crate::store::MAX_TAG_CHARS + 1),
                pid: Some(100),
                session: None,
            }))
            .await
            .expect_err("an oversized tag must be a protocol-level error");

        let message = err.message.to_string();
        assert!(
            message.contains("tag"),
            "error message must say which field is too long, got: {message}"
        );
    }

    #[tokio::test]
    async fn claim_with_an_oversized_session_is_a_protocol_error() {
        let (server, _dir) = server_with_tempdir();

        let err = server
            .claim(Parameters(ClaimParams {
                port: 4011,
                tag: "server".to_string(),
                pid: Some(100),
                session: Some("s".repeat(crate::store::MAX_SESSION_CHARS + 1)),
            }))
            .await
            .expect_err("an oversized session must be a protocol-level error");

        let message = err.message.to_string();
        assert!(
            message.contains("session"),
            "error message must say which field is too long, got: {message}"
        );
    }

    #[tokio::test]
    async fn claim_with_boundary_tag_and_session_lengths_succeeds() {
        let (server, _dir) = server_with_tempdir();

        let result = server
            .claim(Parameters(ClaimParams {
                port: 4012,
                tag: "a".repeat(crate::store::MAX_TAG_CHARS),
                pid: Some(100),
                session: Some("s".repeat(crate::store::MAX_SESSION_CHARS)),
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["port"], 4012);
    }

    // ---- who ----

    #[tokio::test]
    async fn who_found_returns_success_with_the_ls_shape() {
        let (server, _dir) = server_with_tempdir();
        server
            .claim(Parameters(ClaimParams {
                port: 4100,
                tag: "api".to_string(),
                pid: Some(101),
                session: Some("sess-1".to_string()),
            }))
            .await
            .unwrap();

        let result = server
            .who(Parameters(PortParams { port: 4100 }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["port"], 4100);
        assert_eq!(value["pid"], 101);
        assert_eq!(value["session"], "sess-1");
        assert!(value.get("alive").is_some());
        assert!(value.get("age_secs").is_some());
    }

    #[tokio::test]
    async fn who_not_found_returns_a_tool_level_error_not_a_protocol_error() {
        let (server, _dir) = server_with_tempdir();

        // `Ok(..)`, not `Err(..)`: this must be a tool-level error the caller
        // sees in the result, not a JSON-RPC protocol error.
        let result = server
            .who(Parameters(PortParams { port: 4199 }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        let value = structured(&result);
        assert_eq!(value["error"], "not_found");
        assert_eq!(value["port"], 4199);
        assert!(value["message"].as_str().unwrap().contains("4199"));
    }

    // ---- ls ----

    #[tokio::test]
    async fn ls_returns_every_claimed_lease() {
        let (server, _dir) = server_with_tempdir();
        server
            .claim(Parameters(ClaimParams {
                port: 4200,
                tag: "one".to_string(),
                pid: Some(102),
                session: None,
            }))
            .await
            .unwrap();
        server
            .claim(Parameters(ClaimParams {
                port: 4201,
                tag: "two".to_string(),
                pid: Some(103),
                session: None,
            }))
            .await
            .unwrap();

        let result = server.ls().await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        let leases = value.as_array().unwrap();
        assert_eq!(leases.len(), 2);
    }

    #[tokio::test]
    async fn ls_with_nothing_claimed_returns_an_empty_array() {
        let (server, _dir) = server_with_tempdir();
        let result = server.ls().await.unwrap();
        let value = structured(&result);
        assert_eq!(value.as_array().unwrap().len(), 0);
    }

    // ---- release ----

    #[tokio::test]
    async fn release_with_a_dead_pid_reports_was_alive_false() {
        let (server, _dir) = server_with_tempdir();
        server
            .claim(Parameters(ClaimParams {
                port: 4300,
                tag: "stale".to_string(),
                pid: Some(4_000_000_010),
                session: None,
            }))
            .await
            .unwrap();

        let result = server
            .release(Parameters(PortParams { port: 4300 }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["port"], 4300);
        assert_eq!(value["was_alive"], false);

        // Actually released: a follow-up `who` must not find it.
        let who_result = server
            .who(Parameters(PortParams { port: 4300 }))
            .await
            .unwrap();
        assert_eq!(who_result.is_error, Some(true));
    }

    #[tokio::test]
    async fn release_with_an_alive_pid_reports_was_alive_true() {
        let (server, _dir) = server_with_tempdir();
        let own_pid = std::process::id();
        server
            .claim(Parameters(ClaimParams {
                port: 4301,
                tag: "running".to_string(),
                pid: Some(own_pid),
                session: None,
            }))
            .await
            .unwrap();

        let result = server
            .release(Parameters(PortParams { port: 4301 }))
            .await
            .unwrap();
        let value = structured(&result);
        assert_eq!(value["was_alive"], true);
    }

    #[tokio::test]
    async fn release_not_found_returns_a_tool_level_error() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .release(Parameters(PortParams { port: 4399 }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        let value = structured(&result);
        assert_eq!(value["error"], "not_found");
    }

    // ---- prune ----

    #[tokio::test]
    async fn prune_removes_dead_leases_and_returns_them() {
        let (server, _dir) = server_with_tempdir();
        let own_pid = std::process::id();
        server
            .claim(Parameters(ClaimParams {
                port: 4400,
                tag: "alive".to_string(),
                pid: Some(own_pid),
                session: None,
            }))
            .await
            .unwrap();
        server
            .claim(Parameters(ClaimParams {
                port: 4401,
                tag: "dead".to_string(),
                pid: Some(4_000_000_011),
                session: None,
            }))
            .await
            .unwrap();

        let result = server.prune().await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        let pruned = value.as_array().unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0]["port"], 4401);
    }

    // ---- store error propagation ----

    #[tokio::test]
    async fn store_errors_are_protocol_errors_that_preserve_the_full_context_chain() {
        let (server, dir) = server_with_tempdir();
        // Corrupt the state file directly on disk, bypassing the store API.
        std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

        let err = server
            .ls()
            .await
            .expect_err("corrupt state must be a protocol-level error");

        // The CLI shows the full anyhow context chain via `{err:#}`
        // (e.g. "state file at <path> contains invalid JSON and was not
        // modified: <serde_json parse error>"). The MCP error message must
        // carry the same chain, not just the top-level `anyhow::Error`
        // Display (which `.to_string()` / `{err}` would truncate to).
        let message = err.message.to_string();
        assert!(
            message.contains("invalid JSON"),
            "error message must include the store's own context, got: {message}"
        );
        // serde_json's parse-error Display always mentions a line/column;
        // its presence proves the underlying cause was appended, not just
        // the top-level anyhow context on its own.
        assert!(
            message.contains("line") && message.contains("column"),
            "error message must include the underlying parse error, not just the top-level \
             context, got: {message}"
        );
    }

    #[tokio::test]
    async fn prune_with_nothing_dead_returns_an_empty_array() {
        let (server, _dir) = server_with_tempdir();
        let own_pid = std::process::id();
        server
            .claim(Parameters(ClaimParams {
                port: 4402,
                tag: "alive".to_string(),
                pid: Some(own_pid),
                session: None,
            }))
            .await
            .unwrap();

        let result = server.prune().await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value.as_array().unwrap().len(), 0);
    }
}
