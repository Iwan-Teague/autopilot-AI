/// monitor/subprocess.rs — Completion monitor for the `claude` CLI.
///
/// The SubprocessInjector spawns `claude -p <prompt>` and stores the child
/// handle here. This monitor awaits that child exiting, which means Claude
/// has finished and printed its full response to stdout.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::process::Child;
use once_cell::sync::Lazy;
use super::CompletionMonitor;

/// Shared handle to the currently-running claude subprocess.
/// Set by SubprocessInjector before handing control back to the pipeline.
pub static ACTIVE_CHILD: Lazy<Arc<Mutex<Option<Child>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));

pub struct SubprocessMonitor;

impl SubprocessMonitor {
    pub fn new() -> Self { SubprocessMonitor }
}

#[async_trait]
impl CompletionMonitor for SubprocessMonitor {
    async fn wait_for_completion(&mut self) -> Result<()> {
        let mut child = {
            let mut guard = ACTIVE_CHILD.lock().await;
            guard.take().context("No active claude subprocess to monitor")?
        };

        let status = child.wait().await.context("Waiting for claude subprocess")?;

        if status.success() {
            tracing::info!("claude subprocess exited successfully");
        } else {
            tracing::warn!("claude subprocess exited with status: {}", status);
        }
        Ok(())
    }

    fn name(&self) -> &'static str { "claude CLI (subprocess stdout)" }
}
