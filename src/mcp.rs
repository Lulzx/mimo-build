// Minimal MCP (Model Context Protocol) client over stdio — JSON-RPC 2.0,
// newline-delimited. Servers are declared in `.mimo/mcp.json` or `~/.mimo/mcp.json`
// in the standard shape: { "mcpServers": { "<name>": { "command", "args", "env" } } }.
//
// We do the initialize handshake, list tools, and expose them as namespaced tools
// `<server>__<tool>`. I/O is blocking; on a multi-threaded tokio runtime that's fine
// for a single-user CLI.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::api::ToolDef;
use crate::config::mimo_home;

struct Server {
    name: String,
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    tools: Vec<McpTool>,
}

#[derive(Clone)]
struct McpTool {
    name: String,
    description: String,
    schema: Value,
}

impl Server {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}")?;
        self.stdin.flush()?;
        // Read until we get a response with our id (skip notifications/logs).
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(anyhow!("MCP server '{}' closed the connection", self.name));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(line) else { continue };
            if msg.get("id").and_then(|x| x.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(anyhow!("MCP error: {err}"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let req = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{req}")?;
        self.stdin.flush()?;
        Ok(())
    }
}

#[derive(Default)]
pub struct McpManager {
    servers: Vec<Server>,
}

impl McpManager {
    /// Spawn + initialize every configured server. Failures are logged, not fatal.
    pub fn init() -> Self {
        let mut mgr = McpManager::default();
        for (name, spec) in load_server_specs() {
            match spawn_server(&name, &spec) {
                Ok(srv) => {
                    eprintln!("\x1b[2m  mcp: {} ({} tools)\x1b[0m", srv.name, srv.tools.len());
                    mgr.servers.push(srv);
                }
                Err(e) => eprintln!("\x1b[33m  mcp: failed to start {name}: {e}\x1b[0m"),
            }
        }
        mgr
    }

    /// Tool definitions for all MCP tools, namespaced `<server>__<tool>`.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut defs = vec![];
        for srv in &self.servers {
            for t in &srv.tools {
                defs.push(ToolDef {
                    kind: "function".into(),
                    function: json!({
                        "name": format!("{}__{}", srv.name, t.name),
                        "description": format!("[MCP:{}] {}", srv.name, t.description),
                        "parameters": if t.schema.is_null() { json!({"type":"object","properties":{}}) } else { t.schema.clone() }
                    }),
                });
            }
        }
        defs
    }

    pub fn owns(&self, tool_name: &str) -> bool {
        tool_name.contains("__")
            && self.servers.iter().any(|s| tool_name.starts_with(&format!("{}__", s.name)))
    }

    pub fn call(&mut self, tool_name: &str, args: &Value) -> Result<String> {
        let (server, tool) = tool_name
            .split_once("__")
            .ok_or_else(|| anyhow!("not an MCP tool: {tool_name}"))?;
        let srv = self
            .servers
            .iter_mut()
            .find(|s| s.name == server)
            .ok_or_else(|| anyhow!("no MCP server '{server}'"))?;
        let result = srv.request("tools/call", json!({"name": tool, "arguments": args}))?;
        // Flatten content blocks to text.
        let mut out = String::new();
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        if out.is_empty() {
            out = result.to_string();
        }
        Ok(out)
    }
}

fn spawn_server(name: &str, spec: &Value) -> Result<Server> {
    let command = spec["command"].as_str().ok_or_else(|| anyhow!("missing command"))?;
    let mut cmd = Command::new(command);
    if let Some(args) = spec["args"].as_array() {
        for a in args {
            if let Some(s) = a.as_str() {
                cmd.arg(s);
            }
        }
    }
    if let Some(env) = spec["env"].as_object() {
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                cmd.env(k, s);
            }
        }
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?);

    let mut srv = Server {
        name: name.to_string(),
        child,
        stdin,
        stdout,
        next_id: 1,
        tools: vec![],
    };

    srv.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "mimo-rs", "version": "0.2.11"}
        }),
    )?;
    srv.notify("notifications/initialized", json!({}))?;

    let listed = srv.request("tools/list", json!({}))?;
    if let Some(arr) = listed.get("tools").and_then(|t| t.as_array()) {
        for t in arr {
            srv.tools.push(McpTool {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
            });
        }
    }
    Ok(srv)
}

/// Merge server specs from .mimo/mcp.json (project) over ~/.mimo/mcp.json (user).
fn load_server_specs() -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let paths = [mimo_home().join("mcp.json"), std::path::PathBuf::from(".mimo/mcp.json")];
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) {
            for (name, spec) in servers {
                out.insert(name.clone(), spec.clone());
            }
        }
    }
    out
}
