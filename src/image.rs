// Image/video generation tools. Talk to an OpenAI-style images endpoint at
// `{base_url}/images/generations` (+ `/images/edits`) and `{base_url}/videos/generations`.
// Many OpenAI-compatible providers don't implement these — so this module degrades
// gracefully: on any non-2xx it returns an honest "this backend may not support ..." note
// rather than an error. Successful outputs (url or b64) are saved under mimo_home()/assets/
// and the local path or url is returned to the model.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::ToolDef;
use crate::config::Config;

/// Tool schemas: image_gen{prompt,size?}, image_edit{prompt,image_path}, video_gen{prompt}.
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        def(
            "image_gen",
            "Generate an image from a text prompt via the provider's OpenAI-style images endpoint. Saves the result locally and returns the file path or URL. May be unsupported by some backends.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "What to generate"},
                    "size": {"type": "string", "description": "Optional WxH, e.g. 1024x1024"}
                },
                "required": ["prompt"]
            }),
        ),
        def(
            "image_edit",
            "Edit an existing local image given a text prompt, via the provider's OpenAI-style images/edits endpoint. Saves the result locally and returns the file path or URL. May be unsupported by some backends.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "How to edit the image"},
                    "image_path": {"type": "string", "description": "Path to the source image"}
                },
                "required": ["prompt", "image_path"]
            }),
        ),
        def(
            "video_gen",
            "Generate a short video from a text prompt via the provider's videos endpoint. Saves the result locally and returns the file path or URL. Most backends do not support this yet.",
            json!({
                "type": "object",
                "properties": {"prompt": {"type": "string", "description": "What to generate"}},
                "required": ["prompt"]
            }),
        ),
    ]
}

fn def(name: &str, desc: &str, params: Value) -> ToolDef {
    ToolDef {
        kind: "function".into(),
        function: json!({ "name": name, "description": desc, "parameters": params }),
    }
}

/// Image model to request. Honour MIMO_IMAGE_MODEL, else fall back to gpt-image-1.
fn image_model() -> String {
    std::env::var("MIMO_IMAGE_MODEL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "gpt-image-1".into())
}

fn video_model() -> String {
    std::env::var("MIMO_VIDEO_MODEL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "sora".into())
}

/// Dispatch an image/video tool call. Returns None if `name` is not one of ours.
pub async fn dispatch(name: &str, args: &Value, cfg: &Config) -> Option<String> {
    match name {
        "image_gen" => Some(image_gen(args, cfg).await),
        "image_edit" => Some(image_edit(args, cfg).await),
        "video_gen" => Some(video_gen(args, cfg).await),
        _ => None,
    }
}

fn client(cfg: &Config) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = &cfg.api_key {
        match format!("Bearer {key}").parse() {
            Ok(v) => {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
            Err(e) => return Err(format!("bad api key header: {e}")),
        }
    }
    if let Ok(v) = "0.2.11".parse() {
        headers.insert("x-mimo-client-version", v);
    }
    if let Ok(v) = "mimo-cli".parse() {
        headers.insert("x-mimo-client-identifier", v);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))
}

async fn image_gen(args: &Value, cfg: &Config) -> String {
    let prompt = match args["prompt"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return "image_gen: missing 'prompt'.".to_string(),
    };
    let mut body = json!({
        "model": image_model(),
        "prompt": prompt,
        "n": 1,
    });
    if let Some(size) = args["size"].as_str().filter(|s| !s.is_empty()) {
        body["size"] = json!(size);
    }

    let url = format!("{}/images/generations", cfg.base_url.trim_end_matches('/'));
    post_and_save("image_gen", &url, &body, cfg).await
}

async fn image_edit(args: &Value, cfg: &Config) -> String {
    let prompt = match args["prompt"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return "image_edit: missing 'prompt'.".to_string(),
    };
    let image_path = match args["image_path"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return "image_edit: missing 'image_path'.".to_string(),
    };
    // Read + base64-encode the source image so we can send a JSON body (keeps the
    // implementation dependency-free; many compatible backends accept image as b64).
    let bytes = match std::fs::read(image_path) {
        Ok(b) => b,
        Err(e) => return format!("image_edit: cannot read '{image_path}': {e}"),
    };
    let image_b64 = b64_encode(&bytes);
    let body = json!({
        "model": image_model(),
        "prompt": prompt,
        "image": image_b64,
        "n": 1,
    });

    let url = format!("{}/images/edits", cfg.base_url.trim_end_matches('/'));
    post_and_save("image_edit", &url, &body, cfg).await
}

async fn video_gen(args: &Value, cfg: &Config) -> String {
    let prompt = match args["prompt"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return "video_gen: missing 'prompt'.".to_string(),
    };
    let body = json!({
        "model": video_model(),
        "prompt": prompt,
        "n": 1,
    });
    let url = format!("{}/videos/generations", cfg.base_url.trim_end_matches('/'));
    post_and_save("video_gen", &url, &body, cfg).await
}

/// POST a JSON body, parse data[0].url / data[0].b64_json, save it, and return a
/// human-readable result. On any non-2xx, return an honest "unsupported" note.
async fn post_and_save(tool: &str, url: &str, body: &Value, cfg: &Config) -> String {
    let client = match client(cfg) {
        Ok(c) => c,
        Err(e) => return format!("{tool}: {e}"),
    };

    let resp = match client.post(url).json(body).send().await {
        Ok(r) => r,
        Err(e) => return format!("{tool}: request to {url} failed: {e}; this backend may not support it."),
    };

    let status = resp.status();
    if !status.is_success() {
        // Pull a short snippet of the body to aid debugging without flooding context.
        let mut detail = resp.text().await.unwrap_or_default();
        if detail.len() > 400 {
            detail.truncate(400);
            detail.push_str("…");
        }
        let detail = detail.trim();
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {detail}") };
        return format!(
            "{tool}: provider returned {status}; this backend may not support it.{suffix}"
        );
    }

    let json: Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => return format!("{tool}: provider returned 2xx but body was not JSON: {e}"),
    };

    let item = &json["data"][0];

    // Prefer a direct URL when present.
    if let Some(u) = item["url"].as_str().filter(|s| !s.is_empty()) {
        return format!("{tool}: generated -> {u}");
    }

    // Otherwise decode/save the base64 payload.
    if let Some(b64) = item["b64_json"].as_str().filter(|s| !s.is_empty()) {
        let ext = if tool == "video_gen" { "mp4" } else { "png" };
        match b64_decode(b64) {
            Some(bytes) => match save_asset(tool, &bytes, ext) {
                Ok(path) => format!("{tool}: saved -> {}", path.display()),
                Err(e) => format!("{tool}: decoded image but failed to save: {e}"),
            },
            // Couldn't decode without a base64 crate? Persist the raw b64 string so
            // nothing is lost; the caller can decode it later.
            None => match save_asset(tool, b64.as_bytes(), "b64") {
                Ok(path) => format!(
                    "{tool}: provider returned base64 but it could not be decoded in-process; raw payload saved -> {}",
                    path.display()
                ),
                Err(e) => format!("{tool}: failed to save raw base64 payload: {e}"),
            },
        }
    } else {
        format!("{tool}: provider returned 2xx but no url/b64_json in data[0].")
    }
}

/// Save bytes under mimo_home()/assets/ with a timestamped filename. Returns the path.
fn save_asset(tool: &str, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
    let dir = crate::config::mimo_home().join("assets");
    std::fs::create_dir_all(&dir)?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = dir.join(format!("{tool}_{ts}.{ext}"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

// ---- Minimal standard base64 (no external crate) ----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode standard base64 (ignoring whitespace). Returns None on invalid input.
fn b64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let mut n = 0u32;
        let mut bits = 0;
        for &c in chunk {
            n = (n << 6) | val(c)?;
            bits += 6;
        }
        // Left-align the accumulated bits and emit whole bytes.
        n <<= 24 - bits;
        let nbytes = bits / 8;
        let be = n.to_be_bytes();
        out.extend_from_slice(&be[1..1 + nbytes]);
    }
    Some(out)
}
