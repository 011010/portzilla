//! Integration test for `portzilla serve --mcp`: spawns the real binary and
//! drives a minimal MCP session over stdio using raw, hand-written JSON-RPC
//! 2.0 messages (newline-delimited, per the MCP stdio transport). This does
//! not use the `rmcp` client SDK — it is a from-scratch client, so a pass
//! here demonstrates the wire protocol actually works end to end, not just
//! that the in-process tool handlers return the right Rust values.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

struct McpSession {
    // `stdin` is dropped first (struct fields drop in declaration order),
    // closing the pipe so the server sees EOF and exits on its own before
    // `child` is waited on in `Drop`.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    child: Child,
    next_id: u64,
}

impl McpSession {
    fn start(data_dir: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_portzilla"))
            .args(["serve", "--mcp"])
            .env("PORTZILLA_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn portzilla serve --mcp");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            stdin: Some(stdin),
            stdout,
            child,
            next_id: 1,
        }
    }

    /// Sends a JSON-RPC request (with an auto-incrementing id) and returns
    /// the parsed response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let response = self.recv();
        assert_eq!(
            response["id"], id,
            "response id must match the request id, got: {response}"
        );
        response
    }

    /// Sends a JSON-RPC notification (no id, no response expected).
    fn notify(&mut self, method: &str) {
        self.send(json!({"jsonrpc": "2.0", "method": method}));
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        writeln!(stdin, "{message}").expect("failed to write to child stdin");
        stdin.flush().expect("failed to flush child stdin");
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .expect("failed to read from child stdout");
        assert_ne!(n, 0, "child closed stdout without responding");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON-RPC line: {e}\nline: {line}"))
    }

    /// Performs the standard MCP handshake: `initialize` request followed by
    /// the `notifications/initialized` notification.
    fn initialize(&mut self) -> Value {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "portzilla-integration-test", "version": "0.0.0"},
            }),
        );
        self.notify("notifications/initialized");
        response
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        // Close stdin first so the server sees EOF and can exit on its own.
        self.stdin.take();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

#[test]
fn initialize_tools_list_and_tools_call_over_real_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = McpSession::start(dir.path());

    let init_response = session.initialize();
    assert!(
        init_response.get("result").is_some(),
        "initialize must succeed: {init_response}"
    );
    // Server identity must be portzilla's own, not the `rmcp` SDK's build
    // metadata — `Implementation::from_build_env()` would report "rmcp"
    // because it expands `env!()` inside the rmcp crate, not ours.
    assert_eq!(init_response["result"]["serverInfo"]["name"], "portzilla");
    assert_eq!(
        init_response["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let list_response = session.request("tools/list", json!({}));
    let tools = list_response["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tools array");
    let tool_names: std::collections::HashSet<&str> =
        tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        tool_names,
        ["claim", "who", "ls", "release", "prune"].into_iter().collect(),
        "tools/list must expose exactly the five portzilla tools"
    );
    let claim_tool = tools.iter().find(|t| t["name"] == "claim").unwrap();
    assert!(
        claim_tool["description"]
            .as_str()
            .unwrap()
            .contains("INSTEAD of killing"),
        "claim tool description must be visible over the real wire protocol"
    );

    let claim_response = session.call_tool("claim", json!({"port": 9600, "tag": "wire-test", "pid": 42}));
    assert!(
        claim_response.get("error").is_none(),
        "tools/call for claim must not be a JSON-RPC protocol error: {claim_response}"
    );
    let claim_result = &claim_response["result"];
    assert_eq!(claim_result["isError"], false);
    assert_eq!(claim_result["structuredContent"]["port"], 9600);
    assert_eq!(claim_result["structuredContent"]["pid"], 42);
    assert_eq!(claim_result["structuredContent"]["reassigned"], false);

    // A missing lease must come back as a JSON-RPC *success* envelope whose
    // tool result is flagged `isError: true` — a tool-level error, not a
    // protocol-level one. Confirmed here at the actual wire level.
    let who_response = session.call_tool("who", json!({"port": 9601}));
    assert!(
        who_response.get("error").is_none(),
        "not-found must not be a protocol error: {who_response}"
    );
    let who_result = &who_response["result"];
    assert_eq!(who_result["isError"], true);
    assert_eq!(who_result["structuredContent"]["error"], "not_found");
    assert_eq!(who_result["structuredContent"]["port"], 9601);
}

/// A store failure (corrupt state file) must surface as a real JSON-RPC
/// protocol-level error (a top-level `"error"` member, per the JSON-RPC 2.0
/// spec), NOT as a successful tool result flagged `isError: true`. This is
/// the opposite case from the not-found assertions above: not-found is an
/// expected outcome the tool ran and reported; a corrupt state file means
/// portzilla itself could not do its job.
#[test]
fn corrupt_state_file_surfaces_as_a_real_json_rpc_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("leases.json"), b"{ not valid json").unwrap();

    let mut session = McpSession::start(dir.path());
    session.initialize();

    let ls_response = session.call_tool("ls", json!({}));

    assert!(
        ls_response.get("result").is_none(),
        "a store failure must not be reported as a successful tool result: {ls_response}"
    );
    let error = ls_response
        .get("error")
        .expect("a store failure must be a top-level JSON-RPC error object");
    assert!(error.get("code").is_some(), "JSON-RPC error object must carry a code: {error}");
    let message = error["message"].as_str().expect("error message must be a string");
    assert!(
        message.contains("invalid JSON"),
        "error message must preserve the store's context, got: {message}"
    );
}
