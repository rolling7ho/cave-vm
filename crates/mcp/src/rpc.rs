//! JSON-RPC 2.0 over newline-delimited stdio, and the MCP lifecycle
//! methods.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::tools::Tools;

/// MCP protocol versions this server speaks, newest first. A client asking
/// for one of them gets it; any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What the server tells the agent about itself in `initialize`.
const INSTRUCTIONS: &str = "These tools act on this project's cave VM, a disposable Linux VM. \
Run commands with exec, and read and write files with read_file and write_file; relative \
paths are under /workspace, the project folder in the VM. Take a snapshot before risky \
work and restore it if things go wrong. fork makes a copy of the VM to try something \
without touching it; pass the fork's name as `fork` to the other tools to use it.";

/// Serves requests from `input`, writing responses to `output`, until
/// `input` ends.
pub fn serve(input: impl BufRead, mut output: impl Write, tools: &Tools) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle(&line, tools) {
            let mut text = response.to_string();
            text.push('\n');
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The response to one message, or `None` for a notification.
fn handle(line: &str, tools: &Tools) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => return Some(error(Value::Null, PARSE_ERROR, format!("not JSON: {e}"))),
    };
    let Some(object) = message.as_object() else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            "expected one JSON-RPC object; batches are not supported",
        ));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A response from the client, or nonsense: nothing to answer.
        return id.map(|id| error(id, INVALID_REQUEST, "no \"method\""));
    };
    // Notifications, such as `notifications/initialized`, get no answer.
    let id = id?;
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => result(id, initialize(&params)),
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({ "tools": Tools::list() })),
        "tools/call" => match call(&params, tools) {
            Ok(value) => result(id, value),
            Err(message) => error(id, INVALID_PARAMS, message),
        },
        other => error(id, METHOD_NOT_FOUND, format!("no method {other:?}")),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "cave", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// `tools/call`. An unknown tool is a protocol error; a tool that fails
/// is a result with `isError`, so the agent sees why.
fn call(params: &Value, tools: &Tools) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs a \"name\"")?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let outcome = tools
        .call(name, &arguments)
        .ok_or(format!("no tool {name:?}"))?;
    let (text, is_error) = match outcome {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    }))
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}
