/// injector/api.rs — Generic API injector.
///
/// Supports two wire formats:
///   • Anthropic Messages API  (/v1/messages, x-api-key header)
///   • OpenAI Chat Completions (/v1/chat/completions, Bearer token)
///
/// The provider is detected automatically or read from pipeline.json.
/// Each stage is a fresh conversation (no history carried over).
///
/// Two efficiency features:
///   1. Anthropic prompt caching — the `system` block (global rules) is
///      marked `cache_control: ephemeral`, giving a 90% discount on rules
///      tokens for every stage after the first.
///   2. Cost telemetry — we parse `usage` events from the SSE stream and
///      log a per-stage and running cumulative dollar estimate.

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
    /// Running total of estimated dollars spent across all stages this run.
    cumulative_cost_usd: f64,
}

impl ApiInjector {
    pub fn new(provider: ProviderConfig) -> Self {
        ApiInjector {
            client: Client::new(),
            provider,
            cumulative_cost_usd: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Token usage + cost estimation
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,  // Anthropic only — written to the cache
    cache_read_tokens: u64,      // Anthropic only — discounted reads
}

/// Hand-curated price table — USD per 1M tokens. Match by `model.starts_with`.
/// Order matters: longer prefixes first so `claude-3-5-haiku` doesn't match
/// the `claude-3` row. Keep this short, only the models people actually use.
const PRICES: &[(&str, f64, f64)] = &[
    // (prefix, input_per_mtok, output_per_mtok)
    ("claude-opus-4",      15.00, 75.00),
    ("claude-3-opus",      15.00, 75.00),
    ("claude-sonnet-4",     3.00, 15.00),
    ("claude-3-7-sonnet",   3.00, 15.00),
    ("claude-3-5-sonnet",   3.00, 15.00),
    ("claude-haiku-4",      1.00,  5.00),
    ("claude-3-5-haiku",    0.80,  4.00),
    ("claude-3-haiku",      0.25,  1.25),
    ("gpt-5-pro",          15.00, 60.00),
    ("gpt-5-mini",          0.25,  2.00),
    ("gpt-5",               2.00,  8.00),
    ("gpt-4o-mini",         0.15,  0.60),
    ("gpt-4o",              2.50, 10.00),
    ("o1-mini",             3.00, 12.00),
    ("o3-mini",             1.10,  4.40),
    ("o1",                 15.00, 60.00),
    ("o3",                  2.00,  8.00),
    ("mistral-large",       2.00,  6.00),
    ("mistral-medium",      0.40,  2.00),
    ("mistral-small",       0.20,  0.60),
];

fn price_for(model: &str) -> Option<(f64, f64)> {
    let m = model.to_lowercase();
    for (prefix, inp, out) in PRICES {
        if m.starts_with(prefix) {
            return Some((*inp, *out));
        }
    }
    None
}

/// Estimate USD cost for one stage. Cached reads bill at 10% of input price
/// on Anthropic; cache writes bill at 125% of input price.
fn estimate_cost(model: &str, u: Usage) -> Option<f64> {
    let (inp_per_mtok, out_per_mtok) = price_for(model)?;
    let inp = u.input_tokens as f64 * inp_per_mtok;
    let out = u.output_tokens as f64 * out_per_mtok;
    let cache_write = u.cache_creation_tokens as f64 * (inp_per_mtok * 1.25);
    let cache_read  = u.cache_read_tokens as f64 * (inp_per_mtok * 0.10);
    Some((inp + out + cache_write + cache_read) / 1_000_000.0)
}

// ---------------------------------------------------------------------------
// Inject
// ---------------------------------------------------------------------------

#[async_trait]
impl PromptInjector for ApiInjector {
    async fn inject(
        &mut self,
        system: Option<&str>,
        user: &str,
        model_override: Option<&str>,
    ) -> Result<()> {
        let completion_tx = crate::monitor::api_stream::arm().await;

        let model = model_override.unwrap_or(&self.provider.model);
        match model_override {
            Some(_) => tracing::info!("Stage model: {} (per-stage override)", model),
            None    => tracing::info!("Stage model: {}", model),
        }

        let response = match self.provider.format {
            ApiFormat::Anthropic => self.send_anthropic(system, user, model).await?,
            ApiFormat::OpenAi    => self.send_openai(system, user, model).await?,
        };

        let mut stream = response.bytes_stream();
        let mut usage = Usage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Reading SSE chunk")?;
            let text = String::from_utf8_lossy(&chunk);
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" { continue; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(delta) = extract_delta(&v, &self.provider.format) {
                            print!("{}", delta);
                        }
                        merge_usage(&mut usage, &v, &self.provider.format);
                    }
                }
            }
        }
        println!();

        // Telemetry — log per-stage and cumulative cost when we know the model.
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            let cached_str = if usage.cache_read_tokens > 0 || usage.cache_creation_tokens > 0 {
                format!(
                    " (cache: {} read, {} written)",
                    usage.cache_read_tokens, usage.cache_creation_tokens
                )
            } else {
                String::new()
            };

            match estimate_cost(model, usage) {
                Some(cost) => {
                    self.cumulative_cost_usd += cost;
                    tracing::info!(
                        "Tokens: {} in, {} out{} | stage: ${:.4} | total: ${:.4}",
                        usage.input_tokens, usage.output_tokens, cached_str,
                        cost, self.cumulative_cost_usd
                    );
                }
                None => {
                    tracing::info!(
                        "Tokens: {} in, {} out{} | cost: unknown model `{}`",
                        usage.input_tokens, usage.output_tokens, cached_str, model
                    );
                }
            }
        }

        let _ = completion_tx.send(());
        Ok(())
    }

    fn name(&self) -> &'static str { "API (auto-detected provider)" }
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

impl ApiInjector {
    async fn send_anthropic(
        &self,
        system: Option<&str>,
        user: &str,
        model: &str,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/v1/messages", self.provider.base_url.trim_end_matches('/'));
        let key = self.provider.api_key.as_deref().unwrap_or("");

        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "stream": true,
            "messages": [{ "role": "user", "content": user }],
        });

        // Mark the system block as cacheable. Stays stable across the whole
        // pipeline, so every stage after the first reads from cache at 10%.
        if let Some(sys) = system {
            body["system"] = json!([
                {
                    "type": "text",
                    "text": sys,
                    "cache_control": { "type": "ephemeral" }
                }
            ]);
        }

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

    async fn send_openai(
        &self,
        system: Option<&str>,
        user: &str,
        model: &str,
    ) -> Result<reqwest::Response> {
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

        let mut messages: Vec<Value> = Vec::with_capacity(2);
        if let Some(sys) = system {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        messages.push(json!({ "role": "user", "content": user }));

        let body = json!({
            "model": model,
            "stream": true,
            // Ask the server for a final usage event (OpenAI-only, harmless on
            // providers that ignore unknown options).
            "stream_options": { "include_usage": true },
            "messages": messages,
        });

        let resp = req.json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;

        check_status(resp).await
    }
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

fn extract_delta(v: &Value, format: &ApiFormat) -> Option<String> {
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

/// Pull token counts out of any SSE event that carries them, accumulating
/// into `usage`. Anthropic emits `usage` on `message_start` (input/cache
/// counters) and `message_delta` (output_tokens). OpenAI emits a final
/// chunk with `usage` when `stream_options.include_usage` is true.
fn merge_usage(usage: &mut Usage, v: &Value, format: &ApiFormat) {
    let u = match format {
        ApiFormat::Anthropic => {
            // message_start has the initial usage block; message_delta
            // updates output_tokens.
            v.get("message").and_then(|m| m.get("usage"))
                .or_else(|| v.get("usage"))
        }
        ApiFormat::OpenAi => v.get("usage"),
    };
    let Some(u) = u else { return };

    if let Some(n) = u.get("input_tokens").and_then(|n| n.as_u64()) {
        usage.input_tokens = usage.input_tokens.max(n);
    }
    if let Some(n) = u.get("prompt_tokens").and_then(|n| n.as_u64()) {
        usage.input_tokens = usage.input_tokens.max(n);
    }
    if let Some(n) = u.get("output_tokens").and_then(|n| n.as_u64()) {
        usage.output_tokens = usage.output_tokens.max(n);
    }
    if let Some(n) = u.get("completion_tokens").and_then(|n| n.as_u64()) {
        usage.output_tokens = usage.output_tokens.max(n);
    }
    if let Some(n) = u.get("cache_creation_input_tokens").and_then(|n| n.as_u64()) {
        usage.cache_creation_tokens = usage.cache_creation_tokens.max(n);
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(|n| n.as_u64()) {
        usage.cache_read_tokens = usage.cache_read_tokens.max(n);
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
