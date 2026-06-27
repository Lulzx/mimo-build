// Agent Client Protocol (ACP) server over stdio — the editor-integration mode
// (`mimo agent stdio` in the real CLI). Speaks newline-delimited JSON-RPC 2.0 on
// stdin/stdout: editors send `initialize` / `session/new` / `session/prompt` /
// `session/cancel`, and assistant text streams back as `session/update`
// notifications while a turn runs.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::config::Config;
use crate::event::{Emitter, UiEvent};

/// A live session: its agent (built with a channel emitter) plus the receiver
/// end of that channel, which we drain for assistant deltas during each prompt.
struct Session {
    agent: Agent,
    rx: mpsc::UnboundedReceiver<UiEvent>,
}

/// Run the ACP server: read JSON-RPC requests line by line from stdin, dispatch
/// them, and write responses/notifications to stdout. Returns when stdin closes.
pub async fn serve_stdio(cfg: &Config) -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut sessions: HashMap<String, Session> = HashMap::new();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                write_msg(&mut stdout, &error_response(&Value::Null, -32700, "parse error")).await?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                let result = json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "promptCapabilities": {} },
                    "authMethods": [],
                });
                write_msg(&mut stdout, &ok_response(&id, result)).await?;
            }
            "session/new" => {
                // Build the agent with a channel emitter; keep the receiver so each
                // turn's assistant deltas can be forwarded as notifications.
                let (tx, rx) = mpsc::unbounded_channel::<UiEvent>();
                let agent = Agent::new_with(cfg.clone(), Emitter::Channel(tx));
                let session_id = crate::session::new_id();
                sessions.insert(session_id.clone(), Session { agent, rx });
                write_msg(&mut stdout, &ok_response(&id, json!({ "sessionId": session_id }))).await?;
            }
            "session/prompt" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let prompt = collect_prompt_text(&params);

                if !sessions.contains_key(&session_id) {
                    write_msg(&mut stdout, &error_response(&id, -32602, "unknown sessionId")).await?;
                    continue;
                }

                // Drive the turn on a task while forwarding assistant deltas as
                // session/update notifications. The agent is moved into the task and
                // returned so it lives on for the next prompt. The agent holds the
                // sender (via its emitter), so the channel never closes on its own —
                // we stop forwarding once the turn future completes, then drain any
                // chunks still buffered, and treat a TurnDone event as an early stop.
                let mut session = sessions.remove(&session_id).unwrap();
                let mut agent = session.agent;
                let mut turn = tokio::spawn(async move {
                    let r = agent.run_turn(&prompt).await;
                    (agent, r)
                });

                let mut done = false;
                let (agent, result) = loop {
                    tokio::select! {
                        joined = &mut turn => {
                            break joined?;
                        }
                        ev = session.rx.recv(), if !done => {
                            match ev {
                                Some(UiEvent::AssistantDelta(text)) => {
                                    let note = session_update_chunk(&session_id, &text);
                                    write_msg(&mut stdout, &note).await?;
                                }
                                Some(UiEvent::TurnDone { .. }) | None => done = true,
                                Some(_) => {}
                            }
                        }
                    }
                };

                // Flush any deltas that were buffered before the task finished.
                while let Ok(ev) = session.rx.try_recv() {
                    if let UiEvent::AssistantDelta(text) = ev {
                        let note = session_update_chunk(&session_id, &text);
                        write_msg(&mut stdout, &note).await?;
                    }
                }

                session.agent = agent;
                sessions.insert(session_id.clone(), session);

                match result {
                    Ok(_) => {
                        write_msg(&mut stdout, &ok_response(&id, json!({ "stopReason": "end_turn" }))).await?;
                    }
                    Err(e) => {
                        write_msg(&mut stdout, &error_response(&id, -32603, &format!("turn failed: {e}"))).await?;
                    }
                }
            }
            "session/cancel" => {
                write_msg(&mut stdout, &ok_response(&id, Value::Null)).await?;
            }
            _ => {
                write_msg(&mut stdout, &error_response(&id, -32601, "method not found")).await?;
            }
        }
    }

    Ok(())
}

/// Concatenate the text parts of a `session/prompt` params object.
fn collect_prompt_text(params: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = params.get("prompt").and_then(|p| p.as_array()) {
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// A `session/update` notification carrying one assistant message chunk.
fn session_update_chunk(session_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text },
            },
        },
    })
}

fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Write one JSON message as a single newline-terminated line and flush.
async fn write_msg(stdout: &mut tokio::io::Stdout, msg: &Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
