/// main.rs — Entry point for the autopilot-ai binary.
///
/// Usage:
///   autopilot --pipeline pipeline.json
///   autopilot --pipeline pipeline.json --interface webhook   # recommended for Claude Code
///   autopilot --pipeline pipeline.json --interface api       # direct Anthropic API
///   autopilot --pipeline pipeline.json --interface cli       # claude CLI subprocess
///   autopilot --pipeline pipeline.json --resume              # resume from state file
///   autopilot --pipeline pipeline.json --port 7432           # custom webhook port

mod config;
mod provider;
mod monitor;
mod injector;
mod pipeline;
mod server;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use config::{InterfaceHint, PipelineConfig, RunState};
use pipeline::Pipeline;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "autopilot",
    about = "Automated AI prompt pipeline runner",
    long_about = "Reads a JSON pipeline config and drives an AI through all stages automatically.\n\n\
                  RECOMMENDED: Use --interface webhook for Claude Code desktop. The binary starts\n\
                  a local server; you paste the first prompt and Claude self-chains through the rest."
)]
struct Cli {
    /// Path to the pipeline JSON file (generated from your project spec).
    #[arg(short, long)]
    pipeline: std::path::PathBuf,

    /// Override the AI interface to use.
    #[arg(short, long, value_enum)]
    interface: Option<CliInterface>,

    /// Port for the webhook server (default: 7432).
    #[arg(long, default_value_t = 7432)]
    port: u16,

    /// Resume an interrupted run from the state file instead of starting fresh.
    #[arg(long, default_value_t = false)]
    resume: bool,

    /// Override the model for the entire run (e.g. claude-haiku-4-5, gpt-4o-mini).
    /// Per-stage `model` fields in pipeline.json still win over this.
    #[arg(long)]
    model: Option<String>,

    /// Verbose logging.
    #[arg(short, long, default_value_t = false)]
    verbose: bool,
}

#[derive(ValueEnum, Clone, Debug)]
enum CliInterface {
    /// Local webhook server — AI self-chains via curl (recommended for Claude Code, Codex, etc.)
    Webhook,
    /// Call an AI provider API directly (auto-detects Anthropic, OpenAI, Ollama, etc.)
    Api,
    /// Spawn an AI CLI as a subprocess for each stage (`claude`, `codex`, etc.)
    Cli,
    /// Auto-detect the best available mode.
    Auto,
}

impl From<CliInterface> for InterfaceHint {
    fn from(c: CliInterface) -> Self {
        match c {
            CliInterface::Webhook => InterfaceHint::Webhook,
            CliInterface::Api     => InterfaceHint::Api,
            CliInterface::Cli     => InterfaceHint::Cli,
            CliInterface::Auto    => InterfaceHint::Auto,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .compact()
        .init();

    tracing::info!("autopilot-ai starting");

    // Load pipeline config.
    let raw = std::fs::read_to_string(&cli.pipeline)
        .with_context(|| format!("Reading pipeline file: {}", cli.pipeline.display()))?;
    let mut config: PipelineConfig = serde_json::from_str(&raw)
        .context("Parsing pipeline JSON")?;

    // CLI interface override.
    if let Some(iface) = cli.interface {
        config.interface = iface.into();
    }

    // CLI model override applies to the whole pipeline, but per-stage
    // `model` fields in pipeline.json take precedence.
    if let Some(model) = cli.model {
        tracing::info!("Pipeline-level model override from --model: {}", model);
        config.model = Some(model);
    }

    // Auto-detection: webhook is now the preferred mode for desktop use.
    if config.interface == InterfaceHint::Auto {
        // Prefer webhook (works everywhere Claude can run shell commands).
        // Fall back to API if key is set and user seems to want headless operation.
        // The webhook mode is almost always what you want on a local machine.
        config.interface = InterfaceHint::Webhook;
        tracing::info!("Auto-detected interface: webhook (use --interface to override)");
    }

    // Load or create run state.
    let state = if cli.resume && config.state_path.exists() {
        let raw = std::fs::read_to_string(&config.state_path)?;
        let s: RunState = serde_json::from_str(&raw).context("Parsing state file")?;
        tracing::info!("Resuming from stage {}/{}", s.current_stage + 1, config.stages.len());
        s
    } else {
        RunState::new(&config.stages)
    };

    match &config.interface.clone() {
        InterfaceHint::Webhook => run_webhook(config, state, cli.port).await,
        _ => run_direct(config, state).await,
    }
}

// ---------------------------------------------------------------------------
// Webhook mode — Claude self-chains through the pipeline
// ---------------------------------------------------------------------------

async fn run_webhook(config: PipelineConfig, state: RunState, port: u16) -> Result<()> {
    let total = config.stages.len();
    let current = state.current_stage;

    if current >= total {
        println!("Pipeline already complete ({} stages done).", total);
        return Ok(());
    }

    print_summary(&config);

    // Start the local HTTP server.
    let (addr, done_rx) = server::start(config.clone(), state, port).await
        .context("Starting webhook server")?;

    // Print the bootstrap prompt — user pastes this once to kick things off.
    // The AI POSTs to /ready, which confirms connectivity and returns stage 1.
    let bootstrap = server::build_bootstrap_prompt(&addr, port, &config.name);

    println!("{}", "━".repeat(70));
    println!("  PASTE THIS INTO YOUR AI AGENT TO BEGIN");
    println!("{}", "━".repeat(70));
    println!();
    println!("{}", bootstrap);
    println!();
    println!("{}", "━".repeat(70));
    if config.stage_timeout_secs > 0 {
        println!("  Watchdog: {}min timeout per stage", config.stage_timeout_secs / 60);
    }
    println!("  Waiting for {} stage(s) to complete...", total - current);
    println!("  Progress:  GET http://localhost:{}/status", port);
    println!("  Summary:   ./autopilot-summary.md (written as stages complete)");
    println!("{}", "━".repeat(70));
    println!();

    // Block until all stages complete (server signals via done_rx).
    done_rx.await.ok();

    println!();
    println!("{}", "━".repeat(70));
    println!("  ✓ Pipeline complete — all {} stages finished!", total);
    println!("{}", "━".repeat(70));

    Ok(())
}

// ---------------------------------------------------------------------------
// Direct mode — binary injects prompts and monitors completion
// ---------------------------------------------------------------------------

async fn run_direct(config: PipelineConfig, state: RunState) -> Result<()> {
    print_summary(&config);

    let monitor  = monitor::detect(&config).await?;
    let injector = injector::detect(&config).await?;

    tracing::info!("Monitor:  {}", monitor.name());
    tracing::info!("Injector: {}", injector.name());

    let mut pipeline = Pipeline::new(config.clone(), state, monitor, injector);
    pipeline.run().await
}

// ---------------------------------------------------------------------------
// Summary printer
// ---------------------------------------------------------------------------

fn print_summary(config: &PipelineConfig) {
    println!("\n{}", "━".repeat(70));
    println!("  autopilot-ai  —  {}", config.name);
    println!("{}", "━".repeat(70));
    println!("  Stages:    {}", config.stages.len());
    println!("  Interface: {}", config.interface.label());
    if !config.global_rules.is_empty() {
        println!("  Rules:     {} rule(s) prepended to every prompt", config.global_rules.len());
    }
    println!();

    let mut last_phase = None;
    for (i, stage) in config.stages.iter().enumerate() {
        if Some(&stage.phase) != last_phase.as_ref() {
            println!("  ── {} ──", stage.phase);
            last_phase = Some(stage.phase.clone());
        }
        println!("  {:>2}. [{}]  {}", i + 1, stage.id, stage.summary);
    }
    println!("{}", "━".repeat(70));
    println!();
}
