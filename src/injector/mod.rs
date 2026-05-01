/// injector/mod.rs — PromptInjector trait and detection logic.
///
/// Only two injectors are needed for local AI agents:
///   • ApiInjector        — sends prompts to a provider REST API (API mode)
///   • SubprocessInjector — spawns a CLI process for each stage (CLI mode)
///
/// Webhook mode does not use an injector — the next prompt is delivered as
/// the HTTP response body when the AI POSTs to /stage-complete.

pub mod api;
pub mod subprocess;

// Stub — kept so the file exists but contains no active code.
pub mod accessibility;

use anyhow::Result;
use async_trait::async_trait;
use crate::config::{InterfaceHint, PipelineConfig};
use crate::provider;

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PromptInjector: Send + Sync {
    /// Inject a prompt. `model_override` (when Some) takes precedence over
    /// the injector's default model — used for per-stage model switching.
    async fn inject(&mut self, prompt: &str, model_override: Option<&str>) -> Result<()>;
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub async fn detect(config: &PipelineConfig) -> Result<Box<dyn PromptInjector>> {
    match &config.interface {
        InterfaceHint::Webhook => {
            anyhow::bail!(
                "Webhook mode does not use a PromptInjector — \
                 prompts are delivered via the HTTP server response body."
            )
        }
        InterfaceHint::Cli => {
            tracing::info!("Injector: CLI subprocess");
            Ok(Box::new(subprocess::SubprocessInjector::new()))
        }
        // API and Auto both resolve via provider detection.
        InterfaceHint::Api | InterfaceHint::Auto => {
            let provider = provider::detect(
                &config.provider,
                config.api_base_url.as_deref(),
                config.api_key_env.as_deref(),
                config.model.as_deref(),
            ).await?;

            // Provider detection may have resolved a local CLI (e.g. "cli://claude").
            if let Some(bin) = provider.base_url.strip_prefix("cli://") {
                tracing::info!("Injector: {} CLI subprocess (resolved via provider detection)", bin);
                return Ok(Box::new(subprocess::SubprocessInjector::with_bin(bin)));
            }

            tracing::info!("Injector: {} API ({})", provider.name, provider.base_url);
            Ok(Box::new(api::ApiInjector::new(provider)))
        }
    }
}
