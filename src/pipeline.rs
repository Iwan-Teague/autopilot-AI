/// pipeline.rs — The main pipeline state machine.
///
/// Drives stages in order: all Foundation stages, then Implementation, then
/// Testing. For each stage it:
///   1. Assembles the full prompt (global_rules + stage.prompt).
///   2. Prints a progress header.
///   3. Calls injector.inject().
///   4. Calls monitor.wait_for_completion().
///   5. Persists state.
///   6. Moves to the next stage.

use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::config::{PipelineConfig, RunState, StageStatus};
use crate::monitor::CompletionMonitor;
use crate::injector::PromptInjector;

pub struct Pipeline {
    pub config: PipelineConfig,
    pub state: RunState,
    pub monitor: Box<dyn CompletionMonitor>,
    pub injector: Box<dyn PromptInjector>,
}

impl Pipeline {
    pub fn new(
        config: PipelineConfig,
        state: RunState,
        monitor: Box<dyn CompletionMonitor>,
        injector: Box<dyn PromptInjector>,
    ) -> Self {
        Pipeline { config, state, monitor, injector }
    }

    /// Run (or resume) the pipeline from the current stage.
    pub async fn run(&mut self) -> Result<()> {
        let total = self.config.stages.len();

        // Print the interface info once at start.
        tracing::info!(
            "Pipeline '{}' starting on {} | monitor: {} | injector: {}",
            self.config.name,
            self.config.interface.label(),
            self.monitor.name(),
            self.injector.name(),
        );

        for idx in self.state.current_stage..total {
            let stage = self.config.stages[idx].clone();

            // Skip already-completed stages (e.g. after a resume).
            if self.state.stages[idx].status == StageStatus::Completed {
                tracing::info!("Stage [{}/{}] '{}' already completed — skipping", idx + 1, total, stage.id);
                continue;
            }

            // --- Progress header ---
            println!("\n{}", "=".repeat(70));
            println!("  Stage [{}/{}] — {} — Phase: {}",
                idx + 1, total, stage.id, stage.phase);
            println!("  {}", stage.summary);
            println!("{}\n", "=".repeat(70));

            // Mark in-progress
            self.state.stages[idx].status = StageStatus::InProgress;
            self.state.current_stage = idx;
            self.persist_state()?;

            // Assemble full prompt — prepend completed stages' summaries for context.
            let completed: Vec<_> = self.state.stages[..idx]
                .iter()
                .zip(self.config.stages[..idx].iter())
                .collect();
            let prompt = self.assemble_prompt(&stage.prompt, &completed);

            // Inject
            self.injector.inject(&prompt).await
                .with_context(|| format!("Injecting stage '{}'", stage.id))?;

            // Wait for completion
            self.monitor.wait_for_completion().await
                .with_context(|| format!("Monitoring stage '{}'", stage.id))?;

            // Mark complete
            self.state.stages[idx].status = StageStatus::Completed;
            self.state.stages[idx].completed_at = Some(unix_now());
            self.state.current_stage = idx + 1;
            self.persist_state()?;

            tracing::info!("Stage '{}' complete", stage.id);

            // Short pause between stages so the AI isn't immediately hit again.
            if idx + 1 < total {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        println!("\n{}", "=".repeat(70));
        println!("  ✓ Pipeline '{}' complete — all {} stages finished", self.config.name, total);
        println!("{}\n", "=".repeat(70));

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Prepend global rules and a rolling progress block to the stage prompt.
    fn assemble_prompt(
        &self,
        stage_prompt: &str,
        completed: &[(&crate::config::StageState, &crate::config::Stage)],
    ) -> String {
        // Progress block — use AI summary if available (webhook mode), else config summary.
        let progress_block = if completed.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = completed
                .iter()
                .enumerate()
                .map(|(i, (st, cfg))| {
                    let summary = st.ai_summary.as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(&cfg.summary);
                    format!("{}. [{}] {}", i + 1, cfg.id, summary)
                })
                .collect();
            format!("## Progress so far\n{}\n\n---\n\n", lines.join("\n"))
        };

        let rules_block = if self.config.global_rules.is_empty() {
            String::new()
        } else {
            let rules = self.config.global_rules
                .iter()
                .enumerate()
                .map(|(i, r)| format!("{}. {}", i + 1, r))
                .collect::<Vec<_>>()
                .join("\n");
            format!("## Project Rules\n{}\n\n---\n\n", rules)
        };

        format!("{}{}{}", rules_block, progress_block, stage_prompt)
    }

    fn persist_state(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&self.config.state_path, json)
            .context("Writing pipeline state file")?;
        Ok(())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
