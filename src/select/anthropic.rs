//! Anthropic provider — optional alternative to OpenAI for editorial selection.

use super::openai::map_error;
use anyhow::{anyhow, Result};
use serde_json::json;

const VERSION: &str = "2023-06-01";

pub async fn complete(key: &str, model: &str, system: &str, user: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let body = json!({
        "model": model,
        "max_tokens": 4000,
        "temperature": 0.3,
        "system": system,
        "messages": [ { "role": "user", "content": user } ]
    });
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Anthropic: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(map_error(status.as_u16(), &text, "Anthropic"));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| anyhow!("Anthropic returned an unreadable response. Retry the stage."))?;
    let content = v["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow!("Anthropic response had no content. Retry the stage."))?;
    Ok(content.to_string())
}

pub async fn test(key: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Anthropic: {}", e))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(map_error(status.as_u16(), &text, "Anthropic"))
    }
}
