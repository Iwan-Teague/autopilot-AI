/// injector/api.rs — Generic API injector.
///
/// Supports two wire formats:
///   • Anthropic Messages API  (/v1/messages, x-api-key header)
///   • OpenAI Chat Completions (/v1/chat/completions, Bearer token)
///
/// The provider is detected automatically or read from pipeline.json.
/// Conversation history is kept in memory across stages for full context.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use crate::provider::{ApiFormat, ProviderConfig};
use super::PromptInjector;

pub struct ApiInjector {
    client: Client,
    provider: ProviderConfig,
}

impl ApiInjector {
    pub fn new(provider: ProviderConfig) -> Self {
        ApiInjector {
            client: Client::new(),
            provider,
        }
    }
}

#[async_trait]
impl PromptInjector for ApiInjector {
    async fn inject(&mut self, prompt: &str, model_override: Option<&str>) -> Result<()> {
        // Each stage is a fresh conversation — no history carried over.
        // This mirrors how a real human would open a new chat for each task.
        let completion_tx = crate::monitor::api_stream::arm().await;

        let model = model_override.unwrap_or(&self.provider.model);
        if model_override.is_some() {
            tracing::info!("Stage model override: {}", model);
        }

        let response = match self.provider.format {
            ApiFormat::Anthropic => self.send_anthropic(prompt, model).await?,
            ApiFormat::OpenAi    => self.send_openai(prompt, model).await?,
        };

        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Reading SSE chunk")?;
            let text = String::from_utf8_lossy(&chunk);
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" { break; }
                    if let Some(delta) = extract_delta(data, &self.provider.format) {
                        print!("{}", delta);
                    }
                }
            }
        }
        println!();

        let _ = completion_tx.send(());
        Ok(())
    }

    fn name(&self) -> &'static str { "API (auto-detected provider)" }
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

impl ApiInjector {
    async fn send_anthropic(&self, prompt: &str, model: &str) -> Result<reqwest::Response> {
        let url = format!("{}/v1/messages", self.provider.base_url.trim_end_matches('/'));
        let key = self.provider.api_key.as_deref().unwrap_or("");

        let body = json!({
            "model": model,
            "max_tokens": 8192,
            "stream": true,
            "messages": [{ "role": "user", "content": prompt }],
        });

        let resp = self.client
            .post(&url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;

        check_status(resp).await
    }

    async fn send_openai(&self, prompt: &str, model: &str) -> Result<reqwest::Response> {
        let url = format!("{}/v1/chat/completions", self.provider.base_url.trim_end_matches('/'));

        let mut req = self.client
            .post(&url)
            .header("content-type", "application/json");

        // Bearer token — omit header entirely if no key (e.g. Ollama, LM Studio).
        if let Some(key) = &self.provider.api_key {
            if !key.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", key));
            }
        }

        let body = json!({
            "model": model,
            "stream": true,
            "messages": [{ "role": "user", "content": prompt }],
        });

        let resp = req.json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;

        check_status(resp).await
    }
}

// ---------------------------------------------------------------------------
// Delta extraction — handles both SSE formats
// ---------------------------------------------------------------------------

fn extract_delta(data: &str, format: &ApiFormat) -> Option<String> {
    let v: Value = serde_json::from_str(data).ok()?;

    match format {
        ApiFormat::Anthropic => {
            // {"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}
            v.get("delta")?.get("text")?.as_str().map(|s| s.to_string())
        }
        ApiFormat::OpenAi => {
            // {"choices":[{"delta":{"content":"..."}}]}
            v.get("choices")?
                .get(0)?
                .get("delta")?
                .get("content")?
                .as_str()
                .map(|s| s.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("API error {}: {}", status, body)
}
