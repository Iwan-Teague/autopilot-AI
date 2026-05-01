/// injector/subprocess.rs — Inject prompts by spawning a local AI CLI.
///
/// Supports any CLI that accepts a prompt via `<cli> -p "<prompt>"`.
/// Currently detected: `claude`, `codex`.
///
/// The child handle is placed into ACTIVE_CHILD so SubprocessMonitor can
/// wait on it.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use crate::monitor::subprocess::ACTIVE_CHILD;
use super::PromptInjector;

pub struct SubprocessInjector {
    /// The binary to spawn — "claude", "codex", or any compatible CLI.
    bin: String,
}

impl SubprocessInjector {
    /// Use default auto-detected binary (falls back to "claude").
    pub fn new() -> Self {
        SubprocessInjector { bin: "claude".to_string() }
    }

    /// Explicitly specify which CLI binary to use.
    pub fn with_bin(bin: impl Into<String>) -> Self {
        SubprocessInjector { bin: bin.into() }
    }
}

#[async_trait]
impl PromptInjector for SubprocessInjector {
    async fn inject(&mut self, prompt: &str, model_override: Option<&str>) -> Result<()> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p").arg(prompt);

        // Both `claude` and `codex` accept `--model X` for model selection.
        if let Some(model) = model_override {
            tracing::info!("Spawning: {} --model {} -p <prompt>", self.bin, model);
            cmd.arg("--model").arg(model);
        } else {
            tracing::info!("Spawning: {} -p <prompt>", self.bin);
        }

        let child = cmd
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn `{}` — is it installed and in PATH?",
                    self.bin
                )
            })?;

        *ACTIVE_CHILD.lock().await = Some(child);
        Ok(())
    }

    fn name(&self) -> &'static str { "CLI subprocess" }
}
