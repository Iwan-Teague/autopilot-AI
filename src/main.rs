/// main.rs — Entry point for the autopilot-ai binary.
///
/// Three interface modes:
///   • cli      (recommended) — spawns `claude -p <prompt>` (or `codex -p`) per
///                              stage. Uses your local AI CLI subscription.
///                              No GUI involvement; output streams to terminal.
///                              Best for long pipelines (no context drift).
///   • api      — calls the provider REST API directly. Needs API key.
///   • webhook  — local HTTP server; the AI self-chains via curl. Useful when
///                neither CLI nor API key is available.
///
/// Default interface auto-detection: cli if `claude`/`codex` is on PATH, else
/// api if a provider env key is set, else webhook.

mod config;
mod git;
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
                  RECOMMENDED: --interface cli (spawns `claude -p` per stage). Falls back to\n\
                  --interface api if you have an API key, or --interface webhook for self-chain."
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

    /// Use the legacy `POST /ready` bootstrap handshake in webhook mode.
    /// Default is to skip it — the kickoff prompt is stage 1 directly.
    #[arg(long, default_value_t = false)]
    bootstrap_check: bool,

    /// Require the repo to be on this branch before running. Overrides any
    /// `branch` field in pipeline.json. If the current branch differs, the
    /// binary refuses to start so you don't accidentally run on the wrong
    /// branch. Omit to operate on whatever branch is currently checked out.
    #[arg(long)]
    branch: Option<String>,

    /// Verbose logging.
    #[arg(short, long, default_value_t = false)]
    verbose: bool,
}

#[derive(ValueEnum, Clone, Debug)]
enum CliInterface {
    /// Spawn an AI CLI as a subprocess for each stage (`claude`, `codex`, etc.) — recommended.
    Cli,
    /// Call an AI provider API directly (auto-detects Anthropic, OpenAI, Ollama, etc.)
    Api,
    /// Local webhook server — AI self-chains via curl. Use only if cli/api unavailable.
    Webhook,
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

    // CLI flag turns the bootstrap handshake on for this run.
    if cli.bootstrap_check {
        config.bootstrap_check = true;
    }

    // CLI --branch overrides whatever the pipeline.json declared.
    if let Some(b) = cli.branch {
        config.branch = Some(b);
    }

    // Branch check — refuse to start if the repo isn't on the expected branch.
    // Skip silently when not in a git repo (someone running outside one).
    if git::in_repo() {
        let current = git::current_branch();
        match (&config.branch, &current) {
            (Some(want), Some(have)) if want != have => {
                anyhow::bail!(
                    "Pipeline expects branch '{}' but repo is on '{}'.\n\
                     Switch with: git checkout {}\n\
                     Or remove the branch pin from pipeline.json / drop --branch.",
                    want, have, want
                );
            }
            (Some(want), None) => {
                tracing::warn!(
                    "Pipeline expects branch '{}' but git HEAD is detached. Continuing.",
                    want
                );
            }
            _ => {}
        }
        if let Some(b) = &current {
            tracing::info!("Branch: {}", b);
        }
    } else if config.branch.is_some() {
        tracing::warn!("Pipeline declares a branch but cwd is not a git repo. Skipping check.");
    }

    // Auto-detect interface: prefer CLI when a local AI CLI is available,
    // then API when a provider key is set, then webhook as final fallback.
    if config.interface == InterfaceHint::Auto {
        config.interface = autodetect_interface().await;
        tracing::info!(
            "Auto-detected interface: {} (use --interface to override)",
            config.interface.label()
        );
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

/// Pick the best interface based on what's locally available.
/// Order: CLI tool in PATH → API key in env → webhook.
async fn autodetect_interface() -> InterfaceHint {
    let has_cli = which("claude").await || which("codex").await;
    if has_cli {
        return InterfaceHint::Cli;
    }
    let has_api_key = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "MISTRAL_API_KEY",
                       "GROQ_API_KEY", "TOGETHER_API_KEY"]
        .iter()
        .any(|k| std::env::var(k).is_ok());
    if has_api_key {
        return InterfaceHint::Api;
    }
    InterfaceHint::Webhook
}

async fn which(bin: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(bin)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Webhook mode — AI self-chains through the pipeline via curl
// ---------------------------------------------------------------------------

async fn run_webhook(config: PipelineConfig, state: RunState, port: u16) -> Result<()> {
    let total = config.stages.len();
    let current = state.current_stage;

    if current >= total {
        println!("Pipeline already complete ({} stages done).", total);
        return Ok(());
    }

    print_summary(&config);

    // Build the assembled stage-1 prompt now (server hasn't moved past it yet).
    let first_stage = &config.stages[current];
    let first_completed: Vec<(&config::StageState, &config::Stage)> = state.stages[..current]
        .iter()
        .zip(config.stages[..current].iter())
        .collect();
    let first_prompt = server::assemble_prompt(
        &config.global_rules,
        &first_stage.prompt,
        &first_completed,
        first_stage.model.as_deref(),
        first_stage.skip_progress,
    );

    // Start the local HTTP server.
    let (addr, done_rx) = server::start(config.clone(), state, port).await
        .context("Starting webhook server")?;

    let bootstrap = server::build_bootstrap_prompt(
        &addr,
        port,
        &config.name,
        total,
        &first_prompt,
        config.bootstrap_check,
    );

    println!("{}", "━".repeat(70));
    if config.bootstrap_check {
        println!("  PASTE THIS INTO YOUR AI AGENT TO BEGIN  (bootstrap-check ON)");
    } else {
        println!("  PASTE THIS INTO YOUR AI AGENT TO BEGIN  (auto-start, no /ready handshake)");
    }
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
// Direct mode — binary injects prompts and monitors completion (cli / api)
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
    if let Some(branch) = git::current_branch() {
        let pinned = config.branch.as_deref()
            .map(|b| if b == branch { " (pinned)" } else { " (mismatch)" })
            .unwrap_or("");
        println!("  Branch:    {}{}", branch, pinned);
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
