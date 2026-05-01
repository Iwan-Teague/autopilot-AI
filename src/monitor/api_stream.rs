/// monitor/api_stream.rs — Completion monitor for the Anthropic REST API.
///
/// The ApiInjector sends a prompt, streams the SSE response, and when the
/// stream closes it sends a signal via a oneshot channel. This monitor
/// waits on that channel.
///
/// Because the injector and monitor both run in the same pipeline loop we
/// use a simple Mutex<Option<Receiver>> to hand the receiver from the
/// injector to the monitor.

use anyhow::{Context, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use tokio::sync::{oneshot, Mutex};
use super::CompletionMonitor;
#[allow(unused_imports)]
use anyhow::anyhow;

/// The injector places a new Receiver here before starting to stream.
/// The monitor takes it and awaits it.
pub static PENDING_RX: Lazy<Mutex<Option<oneshot::Receiver<()>>>> =
    Lazy::new(|| Mutex::new(None));

/// Called by ApiInjector before sending each request.
/// Returns a Sender the injector fires when its stream closes.
pub async fn arm() -> oneshot::Sender<()> {
    let (tx, rx) = oneshot::channel();
    *PENDING_RX.lock().await = Some(rx);
    tx
}

pub struct ApiMonitor;

impl ApiMonitor {
    pub fn new() -> Self { ApiMonitor }
}

#[async_trait]
impl CompletionMonitor for ApiMonitor {
    async fn wait_for_completion(&mut self) -> Result<()> {
        let rx = {
            let mut guard = PENDING_RX.lock().await;
            guard.take().context("No pending API response to wait on — was arm() called?")?
        };
        rx.await.context("API stream completion signal was dropped")?;
        tracing::info!("API SSE stream closed — response complete");
        Ok(())
    }

    fn name(&self) -> &'static str { "Anthropic API (SSE stream)" }
}
