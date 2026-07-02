//! OpenAI provider (PRD §14: "OpenAI API for candidate selection, using the
//! user's key"). The key is used for the request and never logged.

use anyhow::{anyhow, Result};
use serde_json::json;

pub async fn complete(key: &str, model: &str, system: &str, user: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let body = json!({
        "model": model,
        "temperature": 0.3,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach OpenAI: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(map_error(status.as_u16(), &text, "OpenAI"));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| anyhow!("OpenAI returned an unreadable response. Retry the stage."))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("OpenAI response had no content. Retry the stage."))?;
    Ok(content.to_string())
}

pub async fn test(key: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.openai.com/v1/models")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach OpenAI: {}", e))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(map_error(status.as_u16(), &text, "OpenAI"))
    }
}

pub fn map_error(status: u16, body: &str, provider: &str) -> anyhow::Error {
    let snippet: String = body.chars().take(240).collect();
    match status {
        401 | 403 => anyhow!("{} rejected the API key ({}). Open AI connection and check the key.", provider, status),
        429 => anyhow!("{} rate limit reached (429). Wait a moment, then retry the stage.", provider),
        400 => anyhow!("{} rejected the request (400). The model name may be wrong. Details: {}", provider, snippet),
        404 => anyhow!("{} says the model was not found (404). Check the model name in AI connection.", provider),
        s if s >= 500 => anyhow!("{} had a server error ({}). Retry the stage shortly.", provider, s),
        s => anyhow!("{} returned an unexpected error ({}): {}", provider, s, snippet),
    }
}
