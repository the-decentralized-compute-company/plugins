//! A real MCP server, for the tests.
//!
//! The unit tests elsewhere pin the pure parts — naming, filtering, schema
//! forwarding, the child environment — by calling them directly. This module
//! exists so the tests can also drive the *whole* path over a real JSON-RPC
//! wire: initialize, `tools/list`, discovery, `tools/call`, the result coming
//! back, and the connection dropping underneath a call.
//!
//! It speaks newline-delimited JSON-RPC over a [`tokio::io::DuplexStream`],
//! which is the same framing an MCP stdio server uses over its stdin and
//! stdout. Using an in-process pipe rather than a child process is what makes
//! the tests deterministic and dependency-free; what it does not exercise is
//! process launch and the child environment, which have their own tests in
//! [`crate::childenv`] and are named as untested-here in the README.
//!
//! It is deliberately hand-written rather than built on `rmcp`'s server side:
//! several of these tests need a server that misbehaves — no schema, an
//! enormous result, a name full of punctuation, a connection that vanishes —
//! and a correct server implementation is exactly the wrong tool for that.

#![cfg(test)]

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// What one tool on the fake server does when it is called.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// Answer with the arguments it was given, as text and as
    /// `structuredContent`. Lets a test prove arguments arrived unchanged.
    Echo,
    /// Answer with `isError: true`, the way a well-behaved MCP server reports
    /// a tool-level failure.
    ToolError(String),
    /// Answer with roughly this many bytes of text.
    Bytes(usize),
    /// Never answer. Used to prove the call timeout.
    Hang,
    /// Close the connection instead of answering.
    Disconnect,
}

/// One tool the fake server publishes.
#[derive(Debug, Clone)]
pub struct FakeTool {
    pub name: String,
    pub description: Option<String>,
    /// Raw `inputSchema` JSON, so a test can publish a broken one.
    pub input_schema: Value,
    pub behaviour: Behaviour,
}

impl FakeTool {
    pub fn new(name: &str, behaviour: Behaviour) -> Self {
        Self {
            name: name.to_string(),
            description: Some(format!("The {name} tool")),
            input_schema: json!({
                "type": "object",
                "properties": { "value": { "type": "string", "description": "Anything" } },
                "required": ["value"]
            }),
            behaviour,
        }
    }

    pub fn with_schema(mut self, schema: Value) -> Self {
        self.input_schema = schema;
        self
    }

    pub fn without_description(mut self) -> Self {
        self.description = None;
        self
    }
}

/// Serve MCP on `stream` until the client goes away.
///
/// Spawn it with `tokio::spawn(serve(server_side, tools))` and hand the other
/// half of the duplex to `().serve(...)`.
pub async fn serve(stream: DuplexStream, tools: Vec<FakeTool>) {
    let by_name: BTreeMap<String, FakeTool> = tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.clone()))
        .collect();
    let listing: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut entry = json!({
                "name": tool.name,
                "inputSchema": tool.input_schema,
            });
            if let Some(description) = &tool.description {
                entry["description"] = json!(description);
            }
            entry
        })
        .collect();

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(request): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        // A notification has no id and takes no reply.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let result = match method {
            "initialize" => json!({
                // Echo the client's protocol version so this server is never
                // the reason a handshake fails.
                "protocolVersion": request
                    .get("params")
                    .and_then(|params| params.get("protocolVersion"))
                    .cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-mcp-server", "version": "9.9.9" }
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": listing }),
            "tools/call" => {
                let name = request
                    .get("params")
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = request
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                match by_name.get(&name).map(|tool| tool.behaviour.clone()) {
                    Some(Behaviour::Echo) => json!({
                        "content": [{ "type": "text", "text": format!("called {name}") }],
                        "structuredContent": { "tool": name, "arguments": arguments },
                        "isError": false
                    }),
                    Some(Behaviour::ToolError(message)) => json!({
                        "content": [{ "type": "text", "text": message }],
                        "isError": true
                    }),
                    Some(Behaviour::Bytes(count)) => json!({
                        "content": [{ "type": "text", "text": "x".repeat(count) }],
                        "isError": false
                    }),
                    Some(Behaviour::Hang) => {
                        // Answer nothing, for ever, but stay connected.
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                    Some(Behaviour::Disconnect) => return,
                    None => {
                        let error = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32602, "message": format!("unknown tool {name}") }
                        });
                        if write
                            .write_all(format!("{error}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                }
            }
            _ => json!({}),
        };

        let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        if write.flush().await.is_err() {
            return;
        }
    }
}
