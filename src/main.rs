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
                  RECOMMENDED: Use --interface webhook for Claude Code desktop. The binary starts\n\
                  a local server; you paste the first prompt and Claude self-chains through the rest."
)]
struct Cli {
    /// Path to the pipeline JSON file (generated from your project spec).
    /// Required for normal runs. Optional for `--test-paste`.
    #[arg(short, long)]
    pipeline: Option<std::path::PathBuf>,

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

    /// Use the legacy `POST /ready` bootstrap handshake. Default is to skip it
    /// — the kickoff prompt is stage 1 directly, saving a round trip.
    #[arg(long, default_value_t = false)]
    bootstrap_check: bool,

    /// Require the repo to be on this branch before running. Overrides any
    /// `branch` field in pipeline.json. If the current branch differs, the
    /// binary refuses to start so you don't accidentally run on the wrong
    /// branch. Omit to operate on whatever branch is currently checked out.
    #[arg(long)]
    branch: Option<String>,

    /// After each /stage-complete, auto-paste the next prompt into the target
    /// GUI app (default: Claude desktop) using clipboard + simulated keys.
    /// Reliably drives long webhook pipelines without relying on the agent
    /// voluntarily self-chaining. macOS uses pbcopy + osascript; Linux uses
    /// xclip/xsel + xdotool (X11) or wl-copy + wtype/ydotool (Wayland);
    /// Windows uses Set-Clipboard + PowerShell SendKeys.
    #[arg(long, default_value_t = false)]
    auto_paste: bool,

    /// App name to activate for --auto-paste (default "Claude").
    #[arg(long)]
    target_app: Option<String>,

    /// Test the auto-paste path end-to-end without running a pipeline. Pastes
    /// a unique probe string ("AUTOPILOT_PASTE_PROBE_<id>") into the target
    /// app and exits. Use this to verify Accessibility permission, focus
    /// handling, and send-key behaviour before kicking off a real run.
    #[arg(long, default_value_t = false)]
    test_paste: bool,

    /// Keystroke to send right after the target app activates and before the
    /// paste — used to land focus on the chat input box if it wasn't already.
    /// Format: "mod+key" (e.g. "cmd+l", "ctrl+/", "cmd+shift+m"). Most users
    /// don't need this — the binary's built-in AX walk locates the chat input
    /// in most native apps automatically.
    #[arg(long)]
    focus_key: Option<String>,

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

    // --test-paste: short-circuit before requiring a pipeline.
    if cli.test_paste {
        return run_test_paste(
            cli.target_app.as_deref()
                .unwrap_or(injector::accessibility::DEFAULT_TARGET_APP),
            cli.focus_key.as_deref(),
        );
    }

    let pipeline_path = cli.pipeline
        .ok_or_else(|| anyhow::anyhow!("--pipeline is required (or use --test-paste to verify auto-paste only)"))?;

    // Load pipeline config.
    let raw = std::fs::read_to_string(&pipeline_path)
        .with_context(|| format!("Reading pipeline file: {}", pipeline_path.display()))?;
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

    // CLI auto-paste flags.
    if cli.auto_paste {
        config.auto_paste = true;
    }
    if let Some(app) = cli.target_app {
        config.target_app = Some(app);
    }
    if let Some(fk) = cli.focus_key {
        config.focus_key = Some(fk);
    }
    if config.auto_paste {
        let support = if cfg!(target_os = "macos") {
            "macOS — uses pbcopy + osascript"
        } else if cfg!(target_os = "linux") {
            "Linux — uses xclip/xsel + xdotool on X11, wl-copy + wtype/ydotool on Wayland"
        } else if cfg!(target_os = "windows") {
            "Windows — uses Set-Clipboard + PowerShell SendKeys"
        } else {
            anyhow::bail!("--auto-paste is unsupported on this OS");
        };
        tracing::info!("Auto-paste: {}", support);
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
// --test-paste — verify auto-paste end-to-end before running a real pipeline
// ---------------------------------------------------------------------------

fn run_test_paste(target_app: &str, focus_key: Option<&str>) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let probe_id = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let probe = format!("AUTOPILOT_PASTE_PROBE_{}", probe_id);

    println!("{}", "━".repeat(70));
    println!("  AUTO-PASTE TEST — target app: '{}'", target_app);
    println!("{}", "━".repeat(70));
    println!();
    println!("  This will:");
    println!("    1. Activate the target app");
    println!("    2. Paste a unique probe string");
    println!("    3. Press the configured 'send' key (Cmd/Ctrl+Return)");
    println!();
    println!("  Probe text: {}", probe);
    println!();
    println!("  WHAT TO LOOK FOR after this exits:");
    println!("    ✓ Probe appears as a SENT message in '{}'   → all good", target_app);
    println!("    ⚠ Probe appears in chat input (not sent)      → wrong send-key. Try a different chat app, or open an issue.");
    println!("    ⚠ Probe appears in editor / file / wrong area → focus landed wrong. Try --focus-key (e.g. cmd+l).");
    println!("    ✗ Nothing appears anywhere                    → permission denied. Grant Accessibility (System Settings → Privacy & Security → Accessibility) to whichever app launched this binary.");
    println!();

    let opts = injector::accessibility::PasteOptions {
        focus_key: focus_key.map(|s| s.to_string()),
    };
    let result = injector::accessibility::auto_paste(&probe, target_app, &opts);

    println!("{}", "━".repeat(70));
    match result {
        Ok(()) => {
            println!("  ✓ Paste call returned OK. Check the target app now.");
            println!();
            println!("  If the probe doesn't appear correctly, the paste call");
            println!("  ran but the keystrokes didn't land where expected —");
            println!("  see the troubleshooting list above.");
        }
        Err(e) => {
            println!("  ✗ Paste call FAILED: {}", e);
        }
    }
    println!("{}", "━".repeat(70));

    Ok(())
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

    // Build the assembled stage-1 prompt now (server hasn't moved past it yet).
    // This is what the AI actually executes — embedded in the kickoff when
    // bootstrap_check is off, or fetched via POST /ready when on.
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

    // Decide kickoff delivery mode and report it cleanly to whoever is
    // watching the binary's stdout (typically the agent's bash tool).
    if config.auto_paste && !config.bootstrap_check {
        let app = config.target_app.clone()
            .unwrap_or_else(|| injector::accessibility::DEFAULT_TARGET_APP.to_string());
        let opts = injector::accessibility::PasteOptions {
            focus_key: config.focus_key.clone(),
        };

        println!("{}", "━".repeat(70));
        println!("  AUTO-PASTE — delivering kickoff to '{}'", app);
        println!("{}", "━".repeat(70));

        // Synchronous paste so success/failure is visible immediately.
        // Per-stage continuations (handled in server.rs) stay async — only
        // stage 1 is critical to verify before we hand control to the user.
        match injector::accessibility::auto_paste(&bootstrap, &app, &opts) {
            Ok(()) => {
                println!("  ✓ Stage 1 prompt pasted and submitted to '{}'.", app);
                println!("  Pipeline running. Subsequent stages will paste automatically.");
                println!();
                println!("  Watchdog: {}min/stage  ·  Status: GET http://localhost:{}/status",
                    if config.stage_timeout_secs > 0 { config.stage_timeout_secs / 60 } else { 0 },
                    port);
                println!("  Summary file: ./autopilot-summary.md");
                println!("{}", "━".repeat(70));
                println!();
            }
            Err(e) => {
                println!("  ✗ Auto-paste FAILED: {}", e);
                println!();
                println!("  Most likely cause: the app that launched this binary doesn't have");
                println!("  Accessibility permission. macOS: System Settings → Privacy &");
                println!("  Security → Accessibility → enable the launching app (Terminal /");
                println!("  iTerm / Claude / Cursor / VS Code etc).");
                println!();
                println!("  Falling back to manual paste — copy the prompt below into your");
                println!("  AI agent to begin. Subsequent stages will retry auto-paste; if");
                println!("  the perm is still missing, those will fail too — fix it now and");
                println!("  resume with --resume to recover the run.");
                println!("{}", "━".repeat(70));
                println!();
                println!("{}", bootstrap);
                println!();
                println!("{}", "━".repeat(70));
            }
        }
    } else {
        // No auto-paste — print bootstrap for manual paste.
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
    }

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
