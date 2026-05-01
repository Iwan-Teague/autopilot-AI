/// monitor/mod.rs — CompletionMonitor trait and detection logic.
///
/// Only two monitors are needed for local AI agents:
///   • ApiMonitor   — waits for an SSE stream to close (API mode)
///   • SubprocessMonitor — waits for a CLI child process to exit (CLI mode)
///
/// Webhook mode does not use a monitor — completion is signalled by the AI
/// itself POSTing to the local server.

pub mod api_stream;
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
pub trait CompletionMonitor: Send + Sync {
    async fn wait_for_completion(&mut self) -> Result<()>;
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub async fn detect(config: &PipelineConfig) -> Result<Box<dyn CompletionMonitor>> {
    match &config.interface {
        InterfaceHint::Webhook => {
            anyhow::bail!(
                "Webhook mode does not use a CompletionMonitor — \
                 completion is signalled by the AI POSTing to the local server."
            )
        }
        InterfaceHint::Cli => {
            tracing::info!("Monitor: CLI subprocess");
            Ok(Box::new(subprocess::SubprocessMonitor::new()))
        }
        // API and Auto both go through provider detection.
        InterfaceHint::Api | InterfaceHint::Auto => {
            let provider = provider::detect(
                &config.provider,
                config.api_base_url.as_deref(),
                config.api_key_env.as_deref(),
                config.model.as_deref(),
            ).await?;

            // Provider detection may have resolved a local CLI (e.g. "cli://codex").
            if provider.base_url.starts_with("cli://") {
                tracing::info!("Monitor: CLI subprocess (resolved via provider detection)");
                return Ok(Box::new(subprocess::SubprocessMonitor::new()));
            }

            tracing::info!("Monitor: API stream ({})", provider.name);
            Ok(Box::new(api_stream::ApiMonitor::new()))
        }
    }
}
