// `mimo login` — OAuth2 OIDC Device Authorization Grant (RFC 8628), the same flow the
// real CLI runs against auth.x.ai. Parameterized by the env vars the official binary
// itself reads (MIMO_OIDC_ISSUER / MIMO_OIDC_CLIENT_ID / MIMO_OIDC_SCOPES), so it works
// when pointed at a real issuer + client. Writes ~/.mimo/auth.json in the real format.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::mimo_home;

const DEFAULT_ISSUER: &str = "https://auth.x.ai";
// OIDC scope key the real CLI stores the token under in auth.json (see re/FINDINGS.md).
const DEFAULT_SCOPE_KEY: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

pub async fn login() -> Result<()> {
    let issuer = std::env::var("MIMO_OIDC_ISSUER").unwrap_or(DEFAULT_ISSUER.to_string());
    let client_id = std::env::var("MIMO_OIDC_CLIENT_ID").ok();
    let scopes = std::env::var("MIMO_OIDC_SCOPES").unwrap_or("openid profile email offline_access".into());
    let scope_key = std::env::var("MIMO_OIDC_SCOPE_KEY").unwrap_or(DEFAULT_SCOPE_KEY.into());

    let Some(client_id) = client_id else {
        println!("Mimo uses an OAuth2 OIDC device flow against {issuer}.");
        println!("This build implements that flow but needs the public client id:");
        println!("  export MIMO_OIDC_CLIENT_ID=<client-id>   # and optionally MIMO_OIDC_ISSUER");
        println!("  mimo login");
        println!();
        println!("Or skip OAuth entirely and use an API key:");
        println!("  export XAI_API_KEY=xai-...");
        return Ok(());
    };

    let client = reqwest::Client::builder().user_agent("mimo-rs/0.2.11").build()?;

    // 1) Discover endpoints.
    let disco_url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let disco: Value = client.get(&disco_url).send().await?.json().await?;
    let device_url = disco["device_authorization_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow!("issuer has no device_authorization_endpoint"))?;
    let token_url = disco["token_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow!("issuer has no token_endpoint"))?;

    // 2) Request a device + user code.
    let da: Value = client
        .post(device_url)
        .form(&[("client_id", client_id.as_str()), ("scope", scopes.as_str())])
        .send()
        .await?
        .json()
        .await?;
    let device_code = da["device_code"].as_str().ok_or_else(|| anyhow!("no device_code"))?;
    let user_code = da["user_code"].as_str().unwrap_or("");
    let verify = da["verification_uri_complete"]
        .as_str()
        .or(da["verification_uri"].as_str())
        .unwrap_or("");
    let mut interval = da["interval"].as_u64().unwrap_or(5);

    println!("\nTo sign in, open:\n  \x1b[36m{verify}\x1b[0m");
    if !user_code.is_empty() {
        println!("and enter code: \x1b[1m{user_code}\x1b[0m");
    }
    println!("\nWaiting for authorization...");

    // 3) Poll the token endpoint.
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = client
            .post(token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", client_id.as_str()),
            ])
            .send()
            .await?;
        let body: Value = resp.json().await?;
        if body["access_token"].is_string() {
            write_auth(&scope_key, &body)?;
            println!("\x1b[32m✓ Signed in. Token saved to {}\x1b[0m", mimo_home().join("auth.json").display());
            return Ok(());
        }
        match body["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("expired_token") | Some("access_denied") => {
                return Err(anyhow!("login failed: {}", body["error"].as_str().unwrap_or("?")));
            }
            Some(other) => return Err(anyhow!("login error: {other}")),
            None => continue,
        }
    }
}

fn write_auth(scope_key: &str, token: &Value) -> Result<()> {
    let path = mimo_home().join("auth.json");
    std::fs::create_dir_all(mimo_home())?;
    // Merge with any existing scopes.
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    root[scope_key] = json!({
        "key": token["access_token"],
        "refresh": token["refresh_token"],
        "expires_in": token["expires_in"],
        "token_type": token["token_type"],
    });
    std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}
