/// provider.rs — AI provider detection and configuration.
///
/// Supports any provider that speaks either the Anthropic Messages API or the
/// OpenAI-compatible Chat Completions API (which covers OpenAI, Ollama, Mistral,
/// Groq, Together, LM Studio, and most self-hosted models).
///
/// Detection priority (first match wins):
///   1. Explicit provider in pipeline.json
///   2. Environment variable API keys (checked in preference order)
///   3. Local services running on well-known ports (Ollama, LM Studio)
///   4. Available CLIs (`claude`, `openai`, etc.)

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Known providers
// ---------------------------------------------------------------------------

/// A fully-resolved provider configuration — everything the injector needs.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: &'static str,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub format: ApiFormat,
}

/// Which wire format to use when talking to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiFormat {
    /// Anthropic Messages API (/v1/messages, x-api-key header, SSE with content_block_delta).
    Anthropic,
    /// OpenAI Chat Completions API (/v1/chat/completions, Bearer token, SSE with choices[].delta).
    OpenAi,
}

// ---------------------------------------------------------------------------
// Provider hint (what the user puts in pipeline.json)
// ---------------------------------------------------------------------------

/// Optional explicit provider selection in the pipeline config.
/// Omit entirely to let autopilot auto-detect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHint {
    /// Detect automatically (recommended — leave this field out of pipeline.json).
    #[default]
    Auto,
    Anthropic,
    OpenAi,
    Ollama,
    Mistral,
    Groq,
    Together,
    LmStudio,
    /// Fully custom: set api_base_url, api_key_env, and model in the pipeline config.
    Custom,
}

// ---------------------------------------------------------------------------
// Auto-detection
// ---------------------------------------------------------------------------

/// Detect which provider to use and return a fully-resolved ProviderConfig.
///
/// `overrides` come from the pipeline config — any non-None field wins over
/// what auto-detection would choose.
pub async fn detect(
    hint: &ProviderHint,
    base_url_override: Option<&str>,
    key_env_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<ProviderConfig> {
    let mut cfg = match hint {
        ProviderHint::Anthropic => from_env("Anthropic",   "https://api.anthropic.com", "ANTHROPIC_API_KEY", "claude-opus-4-6",        ApiFormat::Anthropic)?,
        ProviderHint::OpenAi    => from_env("OpenAI",      "https://api.openai.com",    "OPENAI_API_KEY",    "gpt-4o",                  ApiFormat::OpenAi)?,
        ProviderHint::Ollama    => ollama_config(model_override),
        ProviderHint::Mistral   => from_env("Mistral",     "https://api.mistral.ai",    "MISTRAL_API_KEY",   "mistral-large-latest",    ApiFormat::OpenAi)?,
        ProviderHint::Groq      => from_env("Groq",        "https://api.groq.com/openai","GROQ_API_KEY",     "llama-3.3-70b-versatile", ApiFormat::OpenAi)?,
        ProviderHint::Together  => from_env("Together",    "https://api.together.xyz",  "TOGETHER_API_KEY",  "meta-llama/Llama-3-70b-chat-hf", ApiFormat::OpenAi)?,
        ProviderHint::LmStudio  => lm_studio_config(model_override),
        ProviderHint::Custom    => build_custom(base_url_override, key_env_override, model_override)?,
        ProviderHint::Auto      => auto_detect(model_override).await?,
    };

    // Apply overrides from pipeline config.
    if let Some(url) = base_url_override {
        cfg.base_url = url.to_string();
    }
    if let Some(env_name) = key_env_override {
        cfg.api_key = std::env::var(env_name).ok();
    }
    if let Some(model) = model_override {
        cfg.model = model.to_string();
    }

    tracing::info!(
        "Provider: {} | model: {} | format: {:?} | base: {}",
        cfg.name, cfg.model, cfg.format, cfg.base_url
    );

    Ok(cfg)
}

/// Full auto-detection — try everything in priority order.
async fn auto_detect(model_override: Option<&str>) -> Result<ProviderConfig> {
    // 1. Named API keys in preference order.
    let api_providers: &[(&str, &str, &str, &str, ApiFormat)] = &[
        ("Anthropic", "https://api.anthropic.com",     "ANTHROPIC_API_KEY", "claude-opus-4-6",               ApiFormat::Anthropic),
        ("OpenAI",    "https://api.openai.com",        "OPENAI_API_KEY",    "gpt-4o",                        ApiFormat::OpenAi),
        ("Mistral",   "https://api.mistral.ai",        "MISTRAL_API_KEY",   "mistral-large-latest",          ApiFormat::OpenAi),
        ("Groq",      "https://api.groq.com/openai",   "GROQ_API_KEY",      "llama-3.3-70b-versatile",       ApiFormat::OpenAi),
        ("Together",  "https://api.together.xyz",      "TOGETHER_API_KEY",  "meta-llama/Llama-3-70b-chat-hf",ApiFormat::OpenAi),
    ];

    for (name, url, env_key, default_model, fmt) in api_providers {
        if let Ok(key) = std::env::var(env_key) {
            tracing::info!("Auto-detected provider: {} (found {})", name, env_key);
            return Ok(ProviderConfig {
                name,
                base_url: url.to_string(),
                api_key: Some(key),
                model: model_override.unwrap_or(default_model).to_string(),
                format: fmt.clone(),
            });
        }
    }

    // 2. Local services — probe TCP ports.
    if port_open("127.0.0.1", 11434).await {
        tracing::info!("Auto-detected provider: Ollama (port 11434 open)");
        return Ok(ollama_config(model_override));
    }
    if port_open("127.0.0.1", 1234).await {
        tracing::info!("Auto-detected provider: LM Studio (port 1234 open)");
        return Ok(lm_studio_config(model_override));
    }

    // 3. Available local AI CLIs — checked in preference order.
    let local_clis: &[(&str, &str, &str)] = &[
        ("claude", "claude CLI",  "claude-opus-4-6"),
        ("codex",  "Codex CLI",   "o4-mini"),
    ];
    for (bin, label, default_model) in local_clis {
        if which(bin).await {
            tracing::info!("Auto-detected: {} (`{}` in PATH)", label, bin);
            return Ok(ProviderConfig {
                name: label,
                // "cli://<bin>" is a sentinel that tells the injector/monitor
                // to use SubprocessInjector/SubprocessMonitor instead of HTTP.
                base_url: format!("cli://{}", bin),
                api_key: None,
                model: model_override.unwrap_or(default_model).to_string(),
                format: ApiFormat::Anthropic,
            });
        }
    }

    bail!(
        "Could not detect an AI provider. Tried:\n\
         • API keys: ANTHROPIC_API_KEY, OPENAI_API_KEY, MISTRAL_API_KEY, GROQ_API_KEY, TOGETHER_API_KEY\n\
         • Local services: Ollama (port 11434), LM Studio (port 1234)\n\
         • Local CLIs: claude, codex\n\n\
         Fix: set an API key, start a local model server, or install a CLI.\n\
         Or add provider/api_base_url/model to pipeline.json for a custom endpoint."
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn from_env(
    name: &'static str,
    base_url: &str,
    env_key: &str,
    default_model: &str,
    format: ApiFormat,
) -> Result<ProviderConfig> {
    let key = std::env::var(env_key).ok();
    // Don't bail if missing — the caller may provide an override.
    Ok(ProviderConfig {
        name,
        base_url: base_url.to_string(),
        api_key: key,
        model: std::env::var("AUTOPILOT_MODEL").unwrap_or_else(|_| default_model.to_string()),
        format,
    })
}

fn ollama_config(model_override: Option<&str>) -> ProviderConfig {
    let model = model_override
        .or_else(|| std::env::var("OLLAMA_MODEL").ok().as_deref().map(|_| ""))
        .unwrap_or("llama3.2")
        .to_string();
    let model = if model.is_empty() {
        std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string())
    } else {
        model
    };
    let base = std::env::var("OLLAMA_HOST")
        .unwrap_or_else(|_| "http://localhost:11434".to_string());
    ProviderConfig {
        name: "Ollama",
        base_url: base,
        api_key: None,
        model,
        format: ApiFormat::OpenAi,
    }
}

fn lm_studio_config(model_override: Option<&str>) -> ProviderConfig {
    let model = model_override
        .unwrap_or("local-model")
        .to_string();
    ProviderConfig {
        name: "LM Studio",
        base_url: "http://localhost:1234".to_string(),
        api_key: None,
        model,
        format: ApiFormat::OpenAi,
    }
}

fn build_custom(
    base_url: Option<&str>,
    key_env: Option<&str>,
    model: Option<&str>,
) -> Result<ProviderConfig> {
    let base_url = base_url
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AUTOPILOT_API_BASE").ok())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let api_key = key_env
        .and_then(|e| std::env::var(e).ok())
        .or_else(|| std::env::var("AUTOPILOT_API_KEY").ok());
    let model = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AUTOPILOT_MODEL").ok())
        .unwrap_or_else(|| "local-model".to_string());
    Ok(ProviderConfig {
        name: "Custom",
        base_url,
        api_key,
        model,
        format: ApiFormat::OpenAi, // custom endpoints almost always speak OpenAI format
    })
}

async fn port_open(host: &str, port: u16) -> bool {
    let addr = format!("{}:{}", host, port);
    tokio::net::TcpStream::connect(&addr).await.is_ok()
}

async fn which(bin: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(bin)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
