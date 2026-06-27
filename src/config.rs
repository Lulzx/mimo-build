// Config + auth resolution. Mirrors the real CLI's ~/.mimo layout and precedence:
//   MIMO_DEPLOYMENT_KEY > XAI_API_KEY > OIDC token in ~/.mimo/auth.json.
// Endpoint: api.x.ai/v1 for API-key auth; cli-chat-proxy.mimo.com/v1 for OIDC.

use anyhow::Result;
use std::path::PathBuf;

pub fn mimo_home() -> PathBuf {
    if let Ok(h) = std::env::var("MIMO_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir().unwrap_or_default().join(".mimo")
}

#[derive(Clone, Debug)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth_source: String,
    pub always_approve: bool,
    pub plan_mode: bool,
    pub web_search: bool,
    pub max_turns: u32,
    pub permission_mode: String,
    pub extra_rules: Option<String>,
    pub system_prompt_override: Option<String>,
    pub subagents: bool,
    pub subagent_depth: u32,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub agent_override: Option<String>,
    pub sandbox: Option<String>,
    /// Color theme name from `[ui] theme` (None → the default groknight).
    pub theme: Option<String>,
}

const XAI_PUBLIC_BASE: &str = "https://api.x.ai/v1";
const CLI_CHAT_PROXY: &str = "https://cli-chat-proxy.mimo.com/v1";

impl Config {
    pub fn load() -> Result<Self> {
        // ---- config.toml ----
        let toml_path = mimo_home().join("config.toml");
        let mut model = "mimo-v2.5-pro".to_string();
        let mut permission_mode = "default".to_string();
        let mut theme = None;
        if let Ok(text) = std::fs::read_to_string(&toml_path) {
            if let Ok(v) = text.parse::<toml::Value>() {
                if let Some(ui) = v.get("ui") {
                    if let Some(pm) = ui.get("permission_mode").and_then(|x| x.as_str()) {
                        permission_mode = pm.to_string();
                    }
                    if let Some(fm) = ui.get("fork_secondary_model").and_then(|x| x.as_str()) {
                        model = fm.to_string();
                    }
                    if let Some(t) = ui.get("theme").and_then(|x| x.as_str()) {
                        theme = Some(t.to_string());
                    }
                }
            }
        }

        // ---- provider precedence: a custom OpenAI-compatible provider (env or
        // ~/.mimo/mimo-rs.toml [provider]) overrides the built-in xAI auth path. ----
        let (api_key, base_url, auth_source) = match resolve_provider() {
            Some(p) => {
                if let Some(m) = p.model {
                    model = m;
                }
                (Some(p.api_key), p.base_url, format!("provider: {}", p.source))
            }
            None => resolve_auth(),
        };

        Ok(Config {
            model: std::env::var("MIMO_MODEL").unwrap_or(model),
            base_url,
            api_key,
            auth_source,
            always_approve: permission_mode == "always-approve",
            plan_mode: true,
            web_search: std::env::var("MIMO_WEB_SEARCH").map(|v| v != "0").unwrap_or(true),
            max_turns: 50,
            permission_mode,
            extra_rules: None,
            system_prompt_override: None,
            subagents: true,
            subagent_depth: 0,
            allowed_tools: None,
            disallowed_tools: vec![],
            agent_override: None,
            sandbox: None,
            theme,
        })
    }

    /// Persist the chosen `[ui] theme` to ~/.mimo/config.toml so it survives restarts
    /// (mirrors the real CLI, which saves the theme to config). Best-effort; ignores errors.
    pub fn persist_theme(name: &str) {
        let path = mimo_home().join("config.toml");
        let mut root = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.parse::<toml::Value>().ok())
            .and_then(|v| v.as_table().cloned())
            .unwrap_or_default();
        let ui = root
            .entry("ui".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if let Some(tbl) = ui.as_table_mut() {
            tbl.insert("theme".to_string(), toml::Value::String(name.to_string()));
        }
        if let Ok(s) = toml::to_string(&toml::Value::Table(root)) {
            let _ = std::fs::create_dir_all(mimo_home());
            let _ = std::fs::write(&path, s);
        }
    }

    /// Short mode label for the TUI input-box title (mirrors the real CLI).
    pub fn mode_label(&self) -> String {
        if self.always_approve {
            "always-approve".into()
        } else if self.plan_mode {
            "plan".into()
        } else {
            "default".into()
        }
    }

    pub fn print_inspect(&self) {
        println!("Mimo configuration for {}", std::env::current_dir().unwrap_or_default().display());
        println!("  model          : {}", self.model);
        println!("  base_url       : {}", self.base_url);
        println!("  auth           : {}", self.auth_source);
        println!("  permission_mode: {}", self.permission_mode);
        println!("  plan_mode      : {}", self.plan_mode);
        println!("  web_search     : {}", self.web_search);
        println!("  mimo_home      : {}", mimo_home().display());
    }
}

fn resolve_auth() -> (Option<String>, String, String) {
    if let Ok(k) = std::env::var("MIMO_DEPLOYMENT_KEY") {
        if !k.is_empty() {
            return (Some(k), proxy_base(), "deployment key".into());
        }
    }
    for var in ["XAI_API_KEY", "MIMO_CODE_XAI_API_KEY"] {
        if let Ok(k) = std::env::var(var) {
            if !k.is_empty() {
                let base = std::env::var("XAI_API_BASE_URL").unwrap_or(XAI_PUBLIC_BASE.into());
                return (Some(k), base, format!("{var}"));
            }
        }
    }
    // OIDC token from ~/.mimo/auth.json
    if let Some(tok) = read_oidc_token() {
        return (Some(tok), proxy_base(), "auth.json (oidc)".into());
    }
    (None, proxy_base(), "none".into())
}

fn proxy_base() -> String {
    std::env::var("MIMO_CLI_CHAT_PROXY_BASE_URL").unwrap_or(CLI_CHAT_PROXY.into())
}

/// A custom OpenAI-compatible provider (e.g. self-hosted, MiMo, OpenRouter).
struct Provider {
    base_url: String,
    api_key: String,
    model: Option<String>,
    source: String,
}

/// Resolve a custom provider from env (MIMO_BASE_URL + MIMO_API_KEY [+ MIMO_MODEL])
/// or ~/.mimo/mimo-rs.toml `[provider]` (base_url, api_key, model).
fn resolve_provider() -> Option<Provider> {
    if let (Ok(base), Ok(key)) = (std::env::var("MIMO_BASE_URL"), std::env::var("MIMO_API_KEY")) {
        if !base.is_empty() && !key.is_empty() {
            return Some(Provider {
                base_url: base,
                api_key: key,
                model: std::env::var("MIMO_MODEL").ok().filter(|s| !s.is_empty()),
                source: "env".into(),
            });
        }
    }
    let path = mimo_home().join("mimo-rs.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let v = text.parse::<toml::Value>().ok()?;
    let p = v.get("provider")?;
    let base_url = p.get("base_url")?.as_str()?.to_string();
    let api_key = p.get("api_key")?.as_str()?.to_string();
    if base_url.is_empty() || api_key.is_empty() {
        return None;
    }
    Some(Provider {
        base_url,
        api_key,
        model: p.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()),
        source: "mimo-rs.toml".into(),
    })
}

/// Read the OIDC bearer token from ~/.mimo/auth.json.
/// Format: { "<scope>": { "key": "<token>", ... }, ... }
fn read_oidc_token() -> Option<String> {
    let path = mimo_home().join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let obj = v.as_object()?;
    // Prefer the auth.x.ai OIDC scope, else any entry with a "key".
    let oidc_scope = obj
        .keys()
        .find(|k| k.contains("auth.x.ai"))
        .cloned()
        .or_else(|| obj.keys().next().cloned())?;
    obj.get(&oidc_scope)?
        .get("key")?
        .as_str()
        .map(|s| s.to_string())
}
