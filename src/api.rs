// OpenAI-compatible streaming chat client (the shape the real mimo proxy speaks).
// Streams content deltas to stdout as they arrive and assembles tool calls.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::event::Emitter;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(c.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(c.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn tool(call_id: impl Into<String>, c: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(c.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_fn_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_fn_type() -> String {
    "function".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: serde_json::Value,
}

#[derive(Default, Debug)]
pub struct Assistant {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

fn client(cfg: &Config) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = &cfg.api_key {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {key}").parse()?,
        );
    }
    headers.insert("x-mimo-client-version", "0.2.11".parse()?);
    headers.insert("x-mimo-client-identifier", "mimo-cli".parse()?);
    Ok(reqwest::Client::builder().default_headers(headers).build()?)
}

/// Stream a chat completion. Prints assistant text deltas live; returns the
/// assembled assistant message (text + any tool calls).
pub async fn stream_chat(
    cfg: &Config,
    messages: &[Message],
    tools: Vec<ToolDef>,
    emitter: &Emitter,
) -> Result<Assistant> {
    let req = ChatRequest {
        model: &cfg.model,
        messages,
        stream: true,
        tool_choice: if tools.is_empty() { None } else { Some("auto") },
        tools,
    };

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp = client(cfg)?.post(&url).json(&req).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {status} from {url}: {body}"));
    }

    let mut assistant = Assistant::default();
    // tool_calls accumulate by index across deltas.
    let mut tc_acc: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
    let mut printed_any = false;
    // Reasoning (MiMo/DeepSeek stream `reasoning_content`) → timed "Thought for Xs" blocks.
    let mut reason_start: Option<std::time::Instant> = None;
    let mut reason_text = String::new();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // SSE: events separated by newlines, each "data: {...}".
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim().to_string();
            buf.drain(..=pos);
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            let Some(choice) = json["choices"].get(0) else { continue };
            if let Some(reason) = choice["finish_reason"].as_str() {
                assistant.finish_reason = Some(reason.to_string());
            }
            let delta = &choice["delta"];
            if let Some(rc) = delta["reasoning_content"].as_str() {
                if !rc.is_empty() {
                    if reason_start.is_none() {
                        reason_start = Some(std::time::Instant::now());
                    }
                    reason_text.push_str(rc);
                }
            }
            // Reasoning ends when the model starts real output (content or a tool call).
            let starting_output = delta["content"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
                || delta["tool_calls"].is_array();
            if starting_output {
                if let Some(rs) = reason_start.take() {
                    emitter.thought(rs.elapsed().as_secs_f64(), &reason_text);
                    reason_text.clear();
                }
            }
            if let Some(text) = delta["content"].as_str() {
                if !text.is_empty() {
                    emitter.assistant_delta(text);
                    assistant.content.push_str(text);
                    printed_any = true;
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    while tc_acc.len() <= idx {
                        tc_acc.push((String::new(), String::new(), String::new()));
                    }
                    if let Some(id) = call["id"].as_str() {
                        if !id.is_empty() {
                            tc_acc[idx].0 = id.to_string();
                        }
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        if !name.is_empty() {
                            tc_acc[idx].1 = name.to_string();
                        }
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        tc_acc[idx].2.push_str(args);
                    }
                }
            }
        }
    }
    if let Some(rs) = reason_start.take() {
        emitter.thought(rs.elapsed().as_secs_f64(), &reason_text);
    }
    emitter.assistant_end(printed_any);

    for (i, (id, name, args)) in tc_acc.into_iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let id = if id.is_empty() { format!("call_{i}") } else { id };
        assistant.tool_calls.push(ToolCall {
            id,
            kind: "function".into(),
            function: FunctionCall { name, arguments: args },
        });
    }
    Ok(assistant)
}

pub async fn list_models(cfg: &Config) -> Result<Vec<String>> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let resp = client(cfg)?.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await?;
    let mut out = vec![];
    if let Some(arr) = v["data"].as_array() {
        for m in arr {
            if let Some(id) = m["id"].as_str() {
                out.push(id.to_string());
            }
        }
    }
    Ok(out)
}
