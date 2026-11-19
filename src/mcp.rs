//! MCP (Model Context Protocol) server exposing portzilla's lease operations
//! as tools over stdio, so an AI agent with MCP access calls `claim`/`who`/
//! `ls`/`release`/`prune` the same way it calls any other structured tool —
//! typed JSON in, typed JSON out — instead of shelling out to the CLI and
//! parsing text.
//!
//! Every tool result carries the exact same flat JSON shape the CLI's
//! `--json` flag emits (see `crate::view`), so anything already written
//! against the CLI's JSON output recognizes MCP tool results too.

use crate::audit::{AuditActor, AuditEventType, AuditSource, HistoryQuery};
use crate::intent::{self, IntentEventKind, IntentHistoryQuery, IntentStore};
use crate::lease::SystemPidChecker;
use crate::store::Store;
use crate::view::{to_claim_view, to_intent_view, to_view};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
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
    let intents = IntentStore::open(None)?;
    let server = PortzillaMcpServer::new(store, intents);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
struct PortzillaMcpServer {
    store: Store,
    intents: IntentStore,
    tool_router: ToolRouter<PortzillaMcpServer>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClaimParams {
    /// The port to claim.
    port: u16,
    /// Human-readable description of what this port is for (e.g. "next-dev", "vite-preview").
    tag: String,
    /// PID of an already-running process that owns this port. If omitted,
    /// defaults to the portzilla MCP server's own PID — which is almost never
    /// the right owner, since the server process is not the process actually
    /// bound to the port. A future or nonexistent PID is recorded as an
    /// unverified, dead lease and must not be treated as ownership. The result
    /// includes a `note` field when this default was used.
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReleaseParams {
    /// The port to release.
    port: u16,
    /// Session identifying the agent requesting the release.
    #[serde(default)]
    actor_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IntentClaimParams {
    /// File to claim: an absolute path, or a path relative to the MCP server's working directory.
    path: String,
    /// Stable session identifier. Use the same string your harness passes to
    /// portzilla's hooks, or your own claims will look foreign to the guard.
    session: String,
    /// Claim lifetime in seconds, between 1 and 86400. Defaults to 1800.
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IntentPathParams {
    /// File to look up: an absolute path, or a path relative to the MCP server's working directory.
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IntentReleaseParams {
    /// File to release: an absolute path, or a path relative to the MCP server's working directory.
    path: String,
    /// Session that owns the claim.
    session: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct PruneParams {
    /// Session identifying the agent requesting the prune.
    #[serde(default)]
    actor_session: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct HistoryParams {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    event: Option<AuditEventType>,
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct IntentHistoryParams {
    /// Only events where this session claimed, refreshed, released, or lost
    /// the key to a takeover.
    #[serde(default)]
    session: Option<String>,
    /// Only events of this kind: claim_created, claim_refreshed,
    /// claim_taken_over, or released.
    #[serde(default)]
    event: Option<IntentEventKind>,
    /// Only events with a sequence below this cursor (exclusive).
    #[serde(default)]
    before: Option<u64>,
    /// Maximum number of events to return (1-1000, default 100).
    #[serde(default)]
    limit: Option<usize>,
}

#[tool_router]
impl PortzillaMcpServer {
    fn new(store: Store, intents: IntentStore) -> Self {
        Self {
            store,
            intents,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Claim a local port for a process, recording who owns it and why. Use \
         this INSTEAD of killing whatever occupies a port. Pass the PID of an already-running \
         process; a future or nonexistent PID creates an unverified, dead lease and does not \
         establish ownership. If the port is free (or its \
        previous owner has already exited), you get it directly. If it's genuinely held by a \
        live process, you are NOT allowed to steal it — you are given the next free port \
        instead, and the result says so. Call `who` first if you need to know who currently \
        holds a busy port before deciding what to do.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
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
                .claim_with_actor(
                    requested_port,
                    pid,
                    tag,
                    session.clone(),
                    AuditActor::new(AuditSource::Mcp, None, session),
                    &SystemPidChecker,
                )
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
                  is almost never the right owner. Pass the PID of an already-running process \
                  that owns this port instead; a future or nonexistent PID is unverified/dead."
            );
        }
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Show the lease recorded for a port: pid, tag, session, age in seconds, \
        and whether the owning process is currently alive. Use this before assuming a busy \
        port is safe to reuse, or before killing whatever is on it — it tells you who actually \
        owns the port and why. Returns a tool-level error (not a protocol error) if no lease \
        is recorded for the port; that is a normal, expected result, not a failure.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
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
        in seconds, and alive/dead status for each.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
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
        tool-level error (not a protocol error) if no lease is recorded for the port.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn release(
        &self,
        Parameters(params): Parameters<ReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let actor_session = params.actor_session;
        if let Err(err) = AuditActor::new(AuditSource::Mcp, None, actor_session.clone()).validate()
        {
            return Err(McpError::invalid_params(format!("{err:#}"), None));
        }
        let store = self.store.clone();
        let port = params.port;

        let released = tokio::task::spawn_blocking(move || {
            store
                .release(
                    port,
                    AuditActor::new(AuditSource::Mcp, None, actor_session),
                    &SystemPidChecker,
                )
                .map(|outcome| {
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
        the leases that were removed (an empty list if none were dead).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn prune(
        &self,
        Parameters(params): Parameters<PruneParams>,
    ) -> Result<CallToolResult, McpError> {
        let actor_session = params.actor_session;
        if let Err(err) = AuditActor::new(AuditSource::Mcp, None, actor_session.clone()).validate()
        {
            return Err(McpError::invalid_params(format!("{err:#}"), None));
        }
        let store = self.store.clone();

        let value = tokio::task::spawn_blocking(move || {
            store
                .prune(
                    AuditActor::new(AuditSource::Mcp, None, actor_session),
                    &SystemPidChecker,
                )
                .map(|pruned| {
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

    #[tool(
        description = "Read the bounded lease and guard event history, newest first. Filters are combined with AND; use before as an exclusive sequence cursor. This tool is read-only and cannot clear history.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn history(
        &self,
        Parameters(params): Parameters<HistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = HistoryQuery::try_new(
            params.session,
            params.port,
            params.event,
            params.before,
            params.limit.unwrap_or(crate::audit::DEFAULT_HISTORY_LIMIT),
        )
        .map_err(|err| McpError::invalid_params(format!("{err:#}"), None))?;
        let store = self.store.clone();
        let value = tokio::task::spawn_blocking(move || {
            store
                .history(&query)
                .map(|page| serde_json::to_value(page).expect("HistoryPage always serializes"))
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;
        Ok(CallToolResult::structured(value))
    }

    // ---- file intents ----

    #[tool(
        description = "Claim a file for this session before editing it, so another live session \
        that tries to edit the same file is checked against your version instead of clobbering \
        it. Use the same session string your harness gives your hooks; a different string makes \
        your own claims look foreign. Returns a tool-level error naming the holder when another \
        live session already owns the file. Prefer the harness hook when one exists: it does \
        this automatically before every write.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn intent_claim(
        &self,
        Parameters(params): Parameters<IntentClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        let intents = self.intents.clone();
        let path = std::path::PathBuf::from(&params.path);
        let session = params.session;
        let ttl = params.ttl_secs.unwrap_or(intent::DEFAULT_TTL_SECS);
        if !(intent::MIN_TTL_SECS..=intent::MAX_TTL_SECS).contains(&ttl) {
            return Err(McpError::invalid_params(
                format!(
                    "ttl_secs must be between {} and {}",
                    intent::MIN_TTL_SECS,
                    intent::MAX_TTL_SECS
                ),
                None,
            ));
        }

        tokio::task::spawn_blocking(move || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (key, location) = intent::locate_file(&cwd, &path);
            let now = crate::lease::current_unix_timestamp();
            match intents.claim(&key, &session, now, now + ttl, Some(location)) {
                Ok(intent) => {
                    let mut value = serde_json::to_value(to_intent_view(&intent, now))
                        .expect("IntentView always serializes");
                    value["ttl_secs"] = json!(ttl);
                    Ok(CallToolResult::structured(value))
                }
                Err(err) => intent_tool_error(err),
            }
        })
        .await
        .map_err(blocking_task_failed)?
    }

    #[tool(
        description = "Show which session holds a file, with the holder's working copy, age and \
        expiry. Returns a tool-level error (not a protocol error) when no live claim exists. \
        Check this before editing a file another agent may be working on.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn intent_who(
        &self,
        Parameters(params): Parameters<IntentPathParams>,
    ) -> Result<CallToolResult, McpError> {
        let intents = self.intents.clone();
        let path = std::path::PathBuf::from(&params.path);

        tokio::task::spawn_blocking(move || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (key, _) = intent::locate_file(&cwd, &path);
            let now = crate::lease::current_unix_timestamp();
            let found = intents
                .get(&key)
                .map_err(intent_store_protocol_error)?
                .filter(|intent| intent.is_live_at(now));
            match found {
                Some(intent) => Ok(CallToolResult::structured(
                    serde_json::to_value(to_intent_view(&intent, now))
                        .expect("IntentView always serializes"),
                )),
                None => Ok(intent_not_found_error(&key)),
            }
        })
        .await
        .map_err(blocking_task_failed)?
    }

    #[tool(
        description = "List every live file claim across all worktrees, with the holder's working \
        copy, session, age and expiry. Expired claims are omitted.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn intent_ls(&self) -> Result<CallToolResult, McpError> {
        let intents = self.intents.clone();
        tokio::task::spawn_blocking(move || {
            let now = crate::lease::current_unix_timestamp();
            let intents = intents.list(now).map_err(intent_store_protocol_error)?;
            let views: Vec<_> = intents.iter().map(|i| to_intent_view(i, now)).collect();
            Ok(CallToolResult::structured(
                serde_json::to_value(views).expect("IntentView always serializes"),
            ))
        })
        .await
        .map_err(blocking_task_failed)?
    }

    #[tool(
        description = "Read the bounded file-intent journal, newest first. Records claim_created, \
        claim_refreshed, claim_taken_over and released events; reads, refused claims, absent \
        releases and capacity refusals record nothing. Filters are combined with AND; use before \
        as an exclusive sequence cursor. This tool is read-only and cannot clear history.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn intent_history(
        &self,
        Parameters(params): Parameters<IntentHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = IntentHistoryQuery::try_new(
            params.session,
            params.event,
            params.before,
            params.limit.unwrap_or(crate::audit::DEFAULT_HISTORY_LIMIT),
        )
        .map_err(|err| McpError::invalid_params(format!("{err:#}"), None))?;
        let intents = self.intents.clone();
        let value = tokio::task::spawn_blocking(move || {
            intents.history(&query).map(|page| {
                serde_json::to_value(page).expect("IntentHistoryPage always serializes")
            })
        })
        .await
        .map_err(blocking_task_failed)?
        .map_err(store_error)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Release this session's file claim before it expires. Only the owning \
        session can release it; a different session gets a tool-level error naming the holder. \
        Releasing a file nobody claims is a no-op success, so retrying after a crash is safe.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn intent_release(
        &self,
        Parameters(params): Parameters<IntentReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let intents = self.intents.clone();
        let path = std::path::PathBuf::from(&params.path);
        let session = params.session;

        tokio::task::spawn_blocking(move || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (key, _) = intent::locate_file(&cwd, &path);
            let now = crate::lease::current_unix_timestamp();
            match intents.release(&key, &session) {
                Ok(Some(intent)) => {
                    let mut value = serde_json::to_value(to_intent_view(&intent, now))
                        .expect("IntentView always serializes");
                    value["released"] = json!(true);
                    Ok(CallToolResult::structured(value))
                }
                Ok(None) => Ok(CallToolResult::structured(json!({
                    "released": false,
                    "key": key,
                    "file": null,
                }))),
                Err(err) => intent_tool_error(err),
            }
        })
        .await
        .map_err(blocking_task_failed)?
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PortzillaMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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

/// Maps a typed intent error to the surface it belongs on: ownership outcomes
/// are well-formed tool results the caller should see (mirroring the CLI's exit
/// codes), while store failures mean portzilla itself could not do its job.
fn intent_tool_error(err: crate::intent::IntentError) -> Result<CallToolResult, McpError> {
    use crate::intent::IntentError;
    match err {
        IntentError::HeldBy {
            key,
            holder,
            expires_at,
            ..
        } => Ok(CallToolResult::structured_error(json!({
            "error": "held_by",
            "key": key,
            "session": holder,
            "expires_at": expires_at,
            "message": format!("file is held by session {holder} until {expires_at}"),
        }))),
        IntentError::NotOwner {
            key,
            holder,
            requester,
            ..
        } => Ok(CallToolResult::structured_error(json!({
            "error": "not_owner",
            "key": key,
            "session": holder,
            "requester": requester,
            "message": format!("file is owned by session {holder}, not by session {requester}"),
        }))),
        IntentError::Capacity { max } => Ok(CallToolResult::structured_error(json!({
            "error": "capacity",
            "max": max,
            "message": format!("the intent store already holds its maximum of {max} records"),
        }))),
        IntentError::Invalid(message) => Err(McpError::invalid_params(message, None)),
        IntentError::Store(err) => Err(store_error(err)),
    }
}

/// Store-level failures while reading intents are protocol errors, the same
/// rule the lease tools follow.
fn intent_store_protocol_error(err: crate::intent::IntentError) -> McpError {
    match err {
        crate::intent::IntentError::Store(err) => store_error(err),
        other => McpError::internal_error(format!("{other}"), None),
    }
}

/// Tool-level error for "no live claim on this file", mirroring the CLI's
/// exit code 2 for `intent who`.
fn intent_not_found_error(key: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": "not_found",
        "key": key,
        "message": "no live file intent found for this path",
    }))
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
    // No test here holds a lock across `.await`: claims run against an
    // injected probe, so no real port has to stay free for the test body.
    // The allowance stays so a future socket-level test can hold the test
    // port lock across an await without tripping the lint; `#[tokio::test]`
    // uses a current-thread runtime here, so the guard never crosses a
    // thread and cannot deadlock.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::store::PortProbe;
    use crate::store::probe_test_support::{AllPortsFree, OccupiedPorts};
    use rmcp::model::ErrorCode;
    use serde_json::Value;
    use std::sync::Arc;

    fn server_with_probe_tempdir(
        probe: Arc<dyn PortProbe>,
    ) -> (PortzillaMcpServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Claims run against the injected probe so this binary never binds
        // real sockets; the production OsPortProbe is covered by the store
        // unit tests and the integration suite.
        let store = Store::open_with_probe(Some(dir.path().to_path_buf()), probe).unwrap();
        let intents = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
        (PortzillaMcpServer::new(store, intents), dir)
    }

    fn server_with_tempdir() -> (PortzillaMcpServer, tempfile::TempDir) {
        server_with_probe_tempdir(Arc::new(AllPortsFree))
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
    fn all_tools_are_registered_with_descriptions() {
        let (server, _dir) = server_with_tempdir();
        let tools = server.tool_router.list_all();
        let names: std::collections::HashSet<&str> =
            tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "claim",
                "who",
                "ls",
                "release",
                "prune",
                "history",
                "intent_claim",
                "intent_who",
                "intent_ls",
                "intent_history",
                "intent_release",
            ]
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

    // ---- tool annotations ----

    /// Every tool must declare all four behaviour hints explicitly, because
    /// OpenAI's MCP directory rejects tools where any hint is missing, and
    /// hosts use them to warn users before invoking. The values are pinned
    /// per tool and must match the handler's actual behaviour:
    /// - read-only: `who`, `ls`, `history`, `intent_history` never mutate the
    ///   store.
    /// - destructive: `release` deletes a lease even while its owner is
    ///   alive (no confirmation), `prune` deletes leases whose owner died,
    ///   and `claim` is purely additive — it never displaces a live lease,
    ///   so it is the only non-destructive mutator.
    /// - idempotent: repeating any call with the same args has no further
    ///   effect (`claim` on a live own lease returns the same port;
    ///   `release`/`prune` become no-ops once the leases are gone).
    /// - open-world: false everywhere — portzilla only touches local ports,
    ///   processes, and its own store; there is no external domain.
    #[test]
    fn every_tool_declares_all_four_behaviour_hints() {
        let (server, _dir) = server_with_tempdir();
        let tools = server.tool_router.list_all();
        let expected: [(&str, bool, bool, bool, bool); 7] = [
            // name       read-only  destructive  idempotent  open-world
            ("claim", false, false, true, false),
            ("who", true, false, true, false),
            ("ls", true, false, true, false),
            ("release", false, true, true, false),
            ("prune", false, true, true, false),
            ("history", true, false, true, false),
            ("intent_history", true, false, true, false),
        ];
        for (name, read_only, destructive, idempotent, open_world) in expected {
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} tool must be registered"));
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{name} must declare tool annotations"));
            assert_eq!(
                annotations.read_only_hint,
                Some(read_only),
                "{name} read_only_hint"
            );
            assert_eq!(
                annotations.destructive_hint,
                Some(destructive),
                "{name} destructive_hint"
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(idempotent),
                "{name} idempotent_hint"
            );
            assert_eq!(
                annotations.open_world_hint,
                Some(open_world),
                "{name} open_world_hint"
            );
        }
    }

    // ---- claim ----

    #[tokio::test]
    async fn claim_on_a_free_port_returns_success_with_the_ls_plus_reassigned_shape() {
        let (server, _dir) = server_with_tempdir();
        let port: u16 = 4210;
        let result = server
            .claim(Parameters(ClaimParams {
                port,
                tag: "server".to_string(),
                pid: Some(100),
                session: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["port"], port);
        assert_eq!(value["pid"], 100);
        assert_eq!(value["tag"], "server");
        assert_eq!(value["requested_port"], port);
        assert_eq!(value["reassigned"], false);
        assert!(value.get("created_at").is_some());
    }

    #[tokio::test]
    async fn claim_without_pid_defaults_to_the_server_pid_and_adds_a_note() {
        let (server, _dir) = server_with_tempdir();
        let port: u16 = 4211;
        let result = server
            .claim(Parameters(ClaimParams {
                port,
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
        let port: u16 = 4212;
        let result = server
            .claim(Parameters(ClaimParams {
                port,
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
        let port: u16 = 4213;
        let own_pid = std::process::id();
        server
            .claim(Parameters(ClaimParams {
                port,
                tag: "first".to_string(),
                pid: Some(own_pid),
                session: None,
            }))
            .await
            .unwrap();

        let result = server
            .claim(Parameters(ClaimParams {
                port,
                tag: "second".to_string(),
                pid: Some(own_pid + 1),
                session: None,
            }))
            .await
            .unwrap();

        let value = structured(&result);
        assert_eq!(value["reassigned"], true);
        assert!(value["port"].as_u64().unwrap() > port as u64);
        assert_eq!(value["requested_port"], port);
        assert_eq!(value["reassignment_reason"], "lease_conflict");
    }

    #[tokio::test]
    async fn claim_on_an_os_occupied_unregistered_port_reports_os_occupied() {
        // Script the OS side: the injected probe reports `base` occupied by
        // an unregistered process, so the claim must walk forward.
        let base: u16 = 4214;
        let (server, _dir) = server_with_probe_tempdir(Arc::new(OccupiedPorts([base].into())));

        let result = server
            .claim(Parameters(ClaimParams {
                port: base,
                tag: "server".to_string(),
                pid: Some(100),
                session: None,
            }))
            .await
            .unwrap();

        let value = structured(&result);
        assert!(value["port"].as_u64().unwrap() > base as u64);
        assert_eq!(value["reassigned"], true);
        assert_eq!(value["reassignment_reason"], "os_occupied");
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
            .release(Parameters(ReleaseParams {
                port: 4300,
                actor_session: None,
            }))
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
            .release(Parameters(ReleaseParams {
                port: 4301,
                actor_session: None,
            }))
            .await
            .unwrap();
        let value = structured(&result);
        assert_eq!(value["was_alive"], true);
    }

    #[tokio::test]
    async fn release_not_found_returns_a_tool_level_error() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .release(Parameters(ReleaseParams {
                port: 4399,
                actor_session: None,
            }))
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

        let result = server
            .prune(Parameters(PruneParams::default()))
            .await
            .unwrap();
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

        let result = server
            .prune(Parameters(PruneParams::default()))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn history_returns_stable_envelope() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .history(Parameters(HistoryParams::default()))
            .await
            .unwrap();
        let value = structured(&result);
        assert_eq!(
            value,
            &json!({"events": [], "has_more": false, "next_before": null})
        );
    }

    #[tokio::test]
    async fn release_actor_session_is_recorded_separately() {
        let (server, dir) = server_with_tempdir();
        server
            .claim(Parameters(ClaimParams {
                port: 4403,
                tag: "owned".into(),
                pid: Some(100),
                session: Some("owner".into()),
            }))
            .await
            .unwrap();
        server
            .release(Parameters(ReleaseParams {
                port: 4403,
                actor_session: Some("actor".into()),
            }))
            .await
            .unwrap();
        let state: crate::store::StoreState = server.store.read_state().unwrap();
        assert_eq!(state.events[1].actor.session.as_deref(), Some("actor"));
        assert_eq!(
            state.events[1].kind.event_type(),
            AuditEventType::LeaseReleased
        );
        assert!(dir.path().join("leases.json").exists());
    }

    #[tokio::test]
    async fn release_rejects_oversized_actor_session_without_mutating() {
        let (server, _dir) = server_with_tempdir();
        let err = server
            .release(Parameters(ReleaseParams {
                port: 4404,
                actor_session: Some("x".repeat(crate::store::MAX_SESSION_CHARS + 1)),
            }))
            .await
            .expect_err("oversized actor session must be invalid params");
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    // ---- file intents ----

    fn intent_path(dir: &tempfile::TempDir) -> String {
        dir.path()
            .join("src/auth.rs")
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn intent_claim_records_the_file_and_its_location() {
        let (server, dir) = server_with_tempdir();
        let path = intent_path(&dir);
        let result = server
            .intent_claim(Parameters(IntentClaimParams {
                path,
                session: "sess-a".to_string(),
                ttl_secs: Some(120),
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let value = structured(&result);
        assert_eq!(value["session"], "sess-a");
        assert_eq!(value["live"], true);
        assert_eq!(value["ttl_secs"], 120);
        // Build the tail with join() so it matches the platform separator the
        // record actually holds (Windows: "src\\auth.rs").
        let tail = std::path::Path::new("src").join("auth.rs");
        let tail = tail.to_string_lossy();
        assert!(
            value["file"].as_str().unwrap().ends_with(tail.as_ref()),
            "the claim must record the holder's working copy: {value}"
        );
        assert!(value["key"].as_str().unwrap().contains(tail.as_ref()));
    }

    #[tokio::test]
    async fn intent_claim_conflict_is_a_tool_error_naming_the_holder() {
        let (server, dir) = server_with_tempdir();
        let path = intent_path(&dir);
        server
            .intent_claim(Parameters(IntentClaimParams {
                path: path.clone(),
                session: "sess-a".to_string(),
                ttl_secs: None,
            }))
            .await
            .unwrap();

        let conflict = server
            .intent_claim(Parameters(IntentClaimParams {
                path,
                session: "sess-b".to_string(),
                ttl_secs: None,
            }))
            .await
            .unwrap();

        assert_eq!(conflict.is_error, Some(true));
        let value = structured(&conflict);
        assert_eq!(value["error"], "held_by");
        assert_eq!(value["session"], "sess-a");
    }

    #[tokio::test]
    async fn intent_claim_rejects_a_ttl_outside_the_bounds() {
        let (server, dir) = server_with_tempdir();
        let err = server
            .intent_claim(Parameters(IntentClaimParams {
                path: intent_path(&dir),
                session: "sess-a".to_string(),
                ttl_secs: Some(0),
            }))
            .await
            .expect_err("zero ttl must be invalid params");
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn intent_who_finds_the_holder_and_reports_missing_claims() {
        let (server, dir) = server_with_tempdir();
        let path = intent_path(&dir);
        server
            .intent_claim(Parameters(IntentClaimParams {
                path: path.clone(),
                session: "sess-a".to_string(),
                ttl_secs: None,
            }))
            .await
            .unwrap();

        let found = server
            .intent_who(Parameters(IntentPathParams { path: path.clone() }))
            .await
            .unwrap();
        assert_eq!(found.is_error, Some(false));
        assert_eq!(structured(&found)["session"], "sess-a");

        let missing = server
            .intent_who(Parameters(IntentPathParams {
                path: dir.path().join("other.rs").to_string_lossy().into_owned(),
            }))
            .await
            .unwrap();
        assert_eq!(missing.is_error, Some(true));
        assert_eq!(structured(&missing)["error"], "not_found");
    }

    #[tokio::test]
    async fn intent_who_treats_an_expired_claim_as_absent() {
        let (server, dir) = server_with_tempdir();
        let path = intent_path(&dir);
        let now = crate::lease::current_unix_timestamp();
        let (key, location) = crate::intent::locate_file(dir.path(), std::path::Path::new(&path));
        server
            .intents
            .claim(&key, "sess-a", now - 100, now - 1, Some(location))
            .unwrap();

        let missing = server
            .intent_who(Parameters(IntentPathParams { path }))
            .await
            .unwrap();
        assert_eq!(missing.is_error, Some(true));
        assert_eq!(structured(&missing)["error"], "not_found");
    }

    #[tokio::test]
    async fn intent_ls_lists_only_live_claims() {
        let (server, dir) = server_with_tempdir();
        let now = crate::lease::current_unix_timestamp();
        let live = dir.path().join("live.rs");
        let dead = dir.path().join("dead.rs");
        let (live_key, live_location) = crate::intent::locate_file(dir.path(), &live);
        let (dead_key, dead_location) = crate::intent::locate_file(dir.path(), &dead);
        server
            .intents
            .claim(&live_key, "sess-a", now, now + 60, Some(live_location))
            .unwrap();
        server
            .intents
            .claim(&dead_key, "sess-b", now - 100, now - 1, Some(dead_location))
            .unwrap();

        let listed = server.intent_ls().await.unwrap();
        let value = structured(&listed);
        let array = value.as_array().unwrap();
        assert_eq!(array.len(), 1, "expired claims must be omitted: {value}");
        assert!(array[0]["file"].as_str().unwrap().ends_with("live.rs"));
    }

    #[tokio::test]
    async fn intent_release_is_owner_only_and_idempotent() {
        let (server, dir) = server_with_tempdir();
        let path = intent_path(&dir);
        server
            .intent_claim(Parameters(IntentClaimParams {
                path: path.clone(),
                session: "sess-a".to_string(),
                ttl_secs: None,
            }))
            .await
            .unwrap();

        let foreign = server
            .intent_release(Parameters(IntentReleaseParams {
                path: path.clone(),
                session: "sess-b".to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(foreign.is_error, Some(true));
        assert_eq!(structured(&foreign)["error"], "not_owner");

        let released = server
            .intent_release(Parameters(IntentReleaseParams {
                path: path.clone(),
                session: "sess-a".to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(released.is_error, Some(false));
        assert_eq!(structured(&released)["released"], true);

        let again = server
            .intent_release(Parameters(IntentReleaseParams {
                path,
                session: "sess-a".to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(again.is_error, Some(false));
        assert_eq!(structured(&again)["released"], false);
    }

    #[tokio::test]
    async fn intent_history_on_empty_store_returns_stable_envelope() {
        let (server, _dir) = server_with_tempdir();
        let result = server
            .intent_history(Parameters(IntentHistoryParams::default()))
            .await
            .unwrap();
        assert_eq!(
            structured(&result),
            &json!({"events": [], "next_before": null})
        );
    }

    #[tokio::test]
    async fn intent_history_passes_filters_through_and_returns_the_page_shape() {
        let (server, dir) = server_with_tempdir();
        let first = intent_path(&dir);
        let second = dir.path().join("other.rs").to_string_lossy().into_owned();

        for (path, session) in [(&first, "sess-a"), (&second, "sess-b")] {
            server
                .intent_claim(Parameters(IntentClaimParams {
                    path: path.clone(),
                    session: session.to_string(),
                    ttl_secs: None,
                }))
                .await
                .unwrap();
        }
        server
            .intent_release(Parameters(IntentReleaseParams {
                path: second,
                session: "sess-b".to_string(),
            }))
            .await
            .unwrap();

        let all = server
            .intent_history(Parameters(IntentHistoryParams::default()))
            .await
            .unwrap();
        let value = structured(&all);
        let events = value["events"].as_array().unwrap();
        assert_eq!(events.len(), 3, "created + created + released: {value}");
        assert_eq!(events[0]["event"], "released");
        assert_eq!(events[0]["seq"], 3);
        assert!(events[0]["at"].is_u64());
        assert!(events[2]["key"].as_str().unwrap().contains("auth.rs"));
        assert_eq!(value["next_before"], Value::Null);

        let by_session = server
            .intent_history(Parameters(IntentHistoryParams {
                session: Some("sess-a".to_string()),
                ..Default::default()
            }))
            .await
            .unwrap();
        let by_session = structured(&by_session);
        assert_eq!(by_session["events"].as_array().unwrap().len(), 1);
        assert_eq!(by_session["events"][0]["event"], "claim_created");
        assert_eq!(by_session["events"][0]["session"], "sess-a");

        let by_event = server
            .intent_history(Parameters(IntentHistoryParams {
                event: Some(IntentEventKind::Released),
                ..Default::default()
            }))
            .await
            .unwrap();
        let by_event = structured(&by_event);
        assert_eq!(by_event["events"].as_array().unwrap().len(), 1);
        assert_eq!(by_event["events"][0]["event"], "released");

        let first_page = server
            .intent_history(Parameters(IntentHistoryParams {
                limit: Some(1),
                ..Default::default()
            }))
            .await
            .unwrap();
        let first_page = structured(&first_page);
        assert_eq!(first_page["events"].as_array().unwrap().len(), 1);
        assert_eq!(first_page["events"][0]["seq"], 3);
        assert_eq!(first_page["next_before"], 3);

        let older = server
            .intent_history(Parameters(IntentHistoryParams {
                before: Some(3),
                limit: Some(10),
                ..Default::default()
            }))
            .await
            .unwrap();
        let older = structured(&older);
        let seqs: Vec<u64> = older["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [2, 1]);
        assert_eq!(older["next_before"], Value::Null);
    }

    #[tokio::test]
    async fn intent_history_rejects_an_out_of_range_limit_as_invalid_params() {
        let (server, _dir) = server_with_tempdir();
        let err = server
            .intent_history(Parameters(IntentHistoryParams {
                limit: Some(0),
                ..Default::default()
            }))
            .await
            .expect_err("limit 0 must be invalid params");
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }
}
