/// server.rs — Local HTTP server for webhook self-chain mode.
///
/// Flow:
///   1. Binary starts server, prints a tiny bootstrap prompt.
///   2. User pastes bootstrap prompt into their AI agent.
///   3. AI POSTs to /ready → server responds with stage 1 prompt.
///      This confirms the connection works before any real work begins.
///   4. AI completes the stage, POSTs to /stage-complete with its stage_id
///      and an optional one-line summary of what was done.
///   5. Server validates, records, and responds with the next prompt (or done).
///   6. AI continues until it receives {"status": "complete"}.
///
/// Watchdog:
///   After each response the server starts a countdown. If no POST arrives
///   within stage_timeout_secs the watchdog logs a warning and marks the
///   summary file. The pipeline keeps waiting — it does not kill itself.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use anyhow::Result;

use crate::config::{PipelineConfig, RunState, StageStatus};

// ---------------------------------------------------------------------------
// Shared server state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ServerState {
    inner: Arc<Mutex<ServerInner>>,
}

struct ServerInner {
    config: PipelineConfig,
    run_state: RunState,
    done_tx: Option<oneshot::Sender<()>>,
    /// Tracks when the last stage POST was received — used by the watchdog.
    last_activity: Instant,
    /// Monotonic start time of the current stage (reset after each POST).
    stage_started_at: Instant,
}

impl ServerState {
    pub fn new(config: PipelineConfig, run_state: RunState, done_tx: oneshot::Sender<()>) -> Self {
        let now = Instant::now();
        ServerState {
            inner: Arc::new(Mutex::new(ServerInner {
                config,
                run_state,
                done_tx: Some(done_tx),
                last_activity: now,
                stage_started_at: now,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ReadyResponse {
    pub status: &'static str,
    pub message: String,
    pub first_stage: StageInfo,
}

#[derive(Deserialize)]
pub struct StageCompleteRequest {
    pub stage_id: String,
    /// Optional one-line summary of what was accomplished. Keep it brief —
    /// the full response is in the AI's chat history.
    pub summary: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StageCompleteResponse {
    Continue { completed: String, next_stage: StageInfo },
    Complete { message: String, total_stages: usize },
    Error   { message: String },
}

#[derive(Serialize, Clone)]
pub struct StageInfo {
    pub id: String,
    pub phase: String,
    pub summary: String,
    pub prompt: String,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub pipeline: String,
    pub total_stages: usize,
    pub current_stage: usize,
    pub elapsed_secs: u64,
    pub stages: Vec<StageSummary>,
}

#[derive(Serialize)]
pub struct StageSummary {
    pub id: String,
    pub phase: String,
    pub summary: String,
    pub status: String,
    pub duration_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /ready — health check + delivers the first stage prompt.
/// The bootstrap prompt instructs the AI to POST here first, confirming
/// that the connection works and the AI can receive + execute prompts.
async fn ready(
    State(state): State<ServerState>,
    // Accept an optional body (e.g. {"ai": "claude-code"}) for logging.
    body: Option<Json<serde_json::Value>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut inner = state.inner.lock().await;

    if let Some(Json(info)) = body {
        if let Some(ai) = info.get("ai").and_then(|v| v.as_str()) {
            tracing::info!("AI agent connected: {}", ai);
        }
    }

    let total = inner.config.stages.len();
    let current = inner.run_state.current_stage;

    if current >= total {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "complete",
                "message": "Pipeline already complete."
            })),
        );
    }

    // Use a block so all borrows of `inner` are released before the mutable assignments below.
    let (prompt, stage_id, stage_phase, stage_summary, stage_model) = {
        let stage = &inner.config.stages[current];
        let completed: Vec<_> = inner.run_state.stages[..current]
            .iter()
            .zip(inner.config.stages[..current].iter())
            .collect();
        let prompt = assemble_prompt(
            &inner.config.global_rules,
            &stage.prompt,
            &completed,
            stage.model.as_deref(),
            stage.skip_progress,
        );
        (
            prompt,
            stage.id.clone(),
            format!("{}", stage.phase),
            stage.summary.clone(),
            stage.model.clone(),
        )
    };

    inner.last_activity = Instant::now();
    inner.stage_started_at = Instant::now();

    match &stage_model {
        Some(m) => tracing::info!("Health check OK — serving stage 1/{}: '{}' (model: {})", total, stage_id, m),
        None    => tracing::info!("Health check OK — serving stage 1/{}: '{}'", total, stage_id),
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "message": format!("Pipeline connected. Starting stage 1 of {}.", total),
            "first_stage": {
                "id": stage_id,
                "phase": stage_phase,
                "summary": stage_summary,
                "prompt": prompt,
            }
        })),
    )
}

/// POST /stage-complete — AI calls this when it finishes a stage.
async fn stage_complete(
    State(state): State<ServerState>,
    Json(req): Json<StageCompleteRequest>,
) -> (StatusCode, Json<StageCompleteResponse>) {
    let mut inner = state.inner.lock().await;

    let current_idx = inner.run_state.current_stage;
    let total = inner.config.stages.len();

    if current_idx >= total {
        return (StatusCode::BAD_REQUEST, Json(StageCompleteResponse::Error {
            message: "Pipeline already complete.".into(),
        }));
    }

    let expected_id = inner.config.stages[current_idx].id.clone();
    if req.stage_id != expected_id {
        return (StatusCode::BAD_REQUEST, Json(StageCompleteResponse::Error {
            message: format!(
                "Expected stage '{}' but got '{}'. Complete stages in order.",
                expected_id, req.stage_id
            ),
        }));
    }

    let now = Instant::now();
    let duration_secs = now.duration_since(inner.stage_started_at).as_secs();

    // Update run state — including the AI's summary for use in future prompts.
    inner.run_state.stages[current_idx].status = StageStatus::Completed;
    inner.run_state.stages[current_idx].completed_at = Some(unix_now());
    inner.run_state.stages[current_idx].duration_secs = Some(duration_secs);
    // Truncate verbose summaries, and dedup against the stage's pre-written
    // summary — if the AI just echoed our own one-liner, store None and
    // fall back to the config summary in the progress block.
    let stage_pre_summary = inner.config.stages[current_idx].summary.clone();
    inner.run_state.stages[current_idx].ai_summary = req.summary.clone()
        .filter(|s| !s.trim().is_empty())
        .map(|s| truncate_summary(&s))
        .filter(|s| !is_near_duplicate(s, &stage_pre_summary));
    inner.run_state.current_stage = current_idx + 1;
    inner.last_activity = now;
    inner.stage_started_at = now;

    // Persist state.
    if let Err(e) = persist_state(&inner.run_state, &inner.config.state_path) {
        tracing::warn!("State persist failed: {}", e);
    }

    // Append to summary file.
    let stage_cfg = &inner.config.stages[current_idx];
    append_summary(
        current_idx + 1,
        total,
        &stage_cfg.id,
        &format!("{}", stage_cfg.phase),
        &stage_cfg.summary,
        req.summary.as_deref(),
        duration_secs,
        stage_cfg.model.as_deref(),
    );

    tracing::info!(
        "Stage {}/{} '{}' complete in {}s",
        current_idx + 1, total, req.stage_id, duration_secs
    );

    let next_idx = current_idx + 1;

    if next_idx >= total {
        if let Some(tx) = inner.done_tx.take() { let _ = tx.send(()); }
        return (StatusCode::OK, Json(StageCompleteResponse::Complete {
            message: format!("All {} stages complete! The pipeline is finished.", total),
            total_stages: total,
        }));
    }

    let next = &inner.config.stages[next_idx];
    // All stages up to (but not including) next_idx are now complete.
    let completed: Vec<_> = inner.run_state.stages[..next_idx]
        .iter()
        .zip(inner.config.stages[..next_idx].iter())
        .collect();
    let prompt = assemble_prompt(
        &inner.config.global_rules,
        &next.prompt,
        &completed,
        next.model.as_deref(),
        next.skip_progress,
    );

    match &next.model {
        Some(m) => tracing::info!("Serving stage {}/{}: '{}' (model: {})", next_idx + 1, total, next.id, m),
        None    => tracing::info!("Serving stage {}/{}: '{}'", next_idx + 1, total, next.id),
    }

    // Auto-paste: drive the GUI directly so the next stage arrives as a
    // brand-new chat turn, eliminating the agent's "milestone offramp".
    if inner.config.auto_paste {
        let app = inner.config.target_app.clone()
            .unwrap_or_else(|| crate::injector::accessibility::DEFAULT_TARGET_APP.to_string());
        let prompt_for_paste = prompt.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = crate::injector::accessibility::auto_paste(&prompt_for_paste, &app) {
                tracing::error!("Auto-paste failed: {}", e);
            } else {
                tracing::info!("Auto-pasted next stage prompt into '{}'", app);
            }
        });
    }

    (StatusCode::OK, Json(StageCompleteResponse::Continue {
        completed: req.stage_id,
        next_stage: StageInfo {
            id: next.id.clone(),
            phase: format!("{}", next.phase),
            summary: next.summary.clone(),
            prompt,
        },
    }))
}

/// GET /status — human-readable progress check.
async fn pipeline_status(State(state): State<ServerState>) -> Json<StatusResponse> {
    let inner = state.inner.lock().await;
    let elapsed = unix_now().saturating_sub(
        inner.run_state.stages
            .first()
            .and_then(|s| s.completed_at.map(|t| t - inner.run_state.stages[0].duration_secs.unwrap_or(0)))
            .unwrap_or(unix_now())
    );

    let stages = inner.config.stages.iter()
        .zip(inner.run_state.stages.iter())
        .map(|(cfg, st)| StageSummary {
            id: cfg.id.clone(),
            phase: format!("{}", cfg.phase),
            summary: cfg.summary.clone(),
            status: format!("{:?}", st.status).to_lowercase(),
            duration_secs: st.duration_secs,
        })
        .collect();

    Json(StatusResponse {
        pipeline: inner.config.name.clone(),
        total_stages: inner.config.stages.len(),
        current_stage: inner.run_state.current_stage,
        elapsed_secs: elapsed,
        stages,
    })
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

pub async fn start(
    config: PipelineConfig,
    run_state: RunState,
    port: u16,
) -> Result<(String, oneshot::Receiver<()>)> {
    let (done_tx, done_rx) = oneshot::channel();
    let timeout_secs = config.stage_timeout_secs;
    let state = ServerState::new(config, run_state, done_tx);

    let app = Router::new()
        .route("/ready",          post(ready))
        .route("/stage-complete", post(stage_complete))
        .route("/status",         get(pipeline_status))
        .with_state(state.clone());

    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let actual_addr = listener.local_addr()?.to_string();

    tracing::info!("Webhook server listening on http://{}", actual_addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Start watchdog if timeout is enabled.
    if timeout_secs > 0 {
        spawn_watchdog(state, timeout_secs);
    }

    Ok((actual_addr, done_rx))
}

// ---------------------------------------------------------------------------
// Watchdog
// ---------------------------------------------------------------------------

fn spawn_watchdog(state: ServerState, default_timeout_secs: u64) {
    tokio::spawn(async move {
        // Poll once a minute (or twice per timeout window for very short
        // stages — picks the shorter of the two).
        let poll_interval = Duration::from_secs(60.min(default_timeout_secs / 2).max(1));

        loop {
            tokio::time::sleep(poll_interval).await;

            let inner = state.inner.lock().await;

            // Pipeline complete — stop watching.
            if inner.run_state.current_stage >= inner.config.stages.len() {
                break;
            }

            // Per-stage override wins over pipeline default.
            let active_stage = &inner.config.stages[inner.run_state.current_stage];
            let timeout_secs = active_stage.timeout_secs.unwrap_or(default_timeout_secs);
            let timeout = Duration::from_secs(timeout_secs);

            let since_last = Instant::now().duration_since(inner.last_activity);

            if since_last >= timeout {
                tracing::warn!(
                    "Watchdog: no activity for {}s on stage '{}' (timeout {}s — {}). \
                     Still waiting — check your AI agent's chat window.",
                    since_last.as_secs(),
                    active_stage.id,
                    timeout_secs,
                    if active_stage.timeout_secs.is_some() { "per-stage" } else { "pipeline default" }
                );

                // Write a visible warning to the summary file.
                append_watchdog_warning(&active_stage.id, since_last.as_secs());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Summary file
// ---------------------------------------------------------------------------

const SUMMARY_FILE: &str = "autopilot-summary.md";

fn append_summary(
    stage_num: usize,
    total: usize,
    id: &str,
    phase: &str,
    config_summary: &str,
    ai_summary: Option<&str>,
    duration_secs: u64,
    model: Option<&str>,
) {
    let mins = duration_secs / 60;
    let secs = duration_secs % 60;
    let duration_str = if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    };

    let ai_line = match ai_summary {
        Some(s) if !s.trim().is_empty() => format!("\n{}", s.trim()),
        _ => String::new(),
    };

    let model_str = match model {
        Some(m) => format!(" · `{}`", m),
        None    => String::new(),
    };

    let entry = format!(
        "## Stage {}/{}: {} ({})\n*{} · {}{}*{}\n\n---\n\n",
        stage_num, total, id, phase,
        config_summary, duration_str, model_str,
        ai_line
    );

    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(SUMMARY_FILE)
        .and_then(|mut f| { use std::io::Write; f.write_all(entry.as_bytes()) })
    {
        tracing::warn!("Could not write summary file: {}", e);
    }
}

fn append_watchdog_warning(stage_id: &str, elapsed_secs: u64) {
    let entry = format!(
        "> ⚠️ Watchdog: stage '{}' has been running for {}s with no response. \
         Check your AI agent.\n\n",
        stage_id, elapsed_secs
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(SUMMARY_FILE)
        .and_then(|mut f| { use std::io::Write; f.write_all(entry.as_bytes()) });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Cap on how many recent stages appear verbatim in the "Progress so far"
/// block. Older stages collapse into "... and N earlier stages completed."
/// Keeps token cost flat as pipelines grow.
const PROGRESS_TAIL: usize = 5;

/// Hard cap on per-stage AI summaries. Models occasionally return paragraph-
/// long summaries that bloat the progress block on every subsequent stage.
const SUMMARY_MAX_CHARS: usize = 120;

fn truncate_summary(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= SUMMARY_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(SUMMARY_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// True if the AI-supplied summary is just a re-phrased echo of the stage's
/// pre-written summary. Compared after lowercasing and stripping non-alnum.
fn is_near_duplicate(a: &str, b: &str) -> bool {
    let normalize = |s: &str| -> String {
        s.chars().filter_map(|c| {
            if c.is_alphanumeric() { Some(c.to_ascii_lowercase()) } else { None }
        }).collect()
    };
    let na = normalize(a);
    let nb = normalize(b);
    !na.is_empty() && !nb.is_empty() && (na == nb || na.starts_with(&nb) || nb.starts_with(&na))
}

/// Build the compact "Progress so far" block from completed stages.
/// Each entry uses the AI-provided summary if available, falling back to the
/// config summary. Kept deliberately short to minimise token cost.
fn build_progress_block(completed: &[(&crate::config::StageState, &crate::config::Stage)]) -> Option<String> {
    if completed.is_empty() {
        return None;
    }
    let total = completed.len();
    let tail_start = total.saturating_sub(PROGRESS_TAIL);
    let elided = tail_start;
    let lines: Vec<String> = completed
        .iter()
        .skip(tail_start)
        .map(|(st, cfg)| {
            let summary = st.ai_summary.as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&cfg.summary);
            // Compact format — drop bracket noise, save ~5 chars/line × N stages.
            format!("- {}: {}", cfg.id, summary)
        })
        .collect();

    let header = if elided > 0 {
        format!("## Progress so far\n*... {} earlier stage{} completed*\n", elided, if elided == 1 { "" } else { "s" })
    } else {
        "## Progress so far\n".to_string()
    };
    Some(format!("{}{}\n\n---\n\n", header, lines.join("\n")))
}

/// Resend the full global_rules block every Nth stage (in webhook mode the
/// AI is in one chat session — repeating rules every stage wastes tokens, but
/// dropping them entirely risks drift if the chat context gets pruned).
/// Pattern: include on stages where index % 3 == 0 (i.e. 1, 4, 7, ...).
const RULES_RESEND_EVERY: usize = 3;

pub fn assemble_prompt(
    global_rules: &[String],
    stage_prompt: &str,
    completed: &[(&crate::config::StageState, &crate::config::Stage)],
    stage_model: Option<&str>,
    skip_progress: bool,
) -> String {
    // Continuation directive — only on stages 2+ (when delivered via
    // /stage-complete in webhook mode). Reminds the agent that this prompt is
    // a NEW TASK to execute, not a status report to summarise to the user.
    let continuation_header = if completed.is_empty() {
        String::new()
    } else {
        "# AUTOPILOT CONTINUATION — execute immediately, do not pause for user\n\n\
         This text is your next task in the running pipeline. Begin work now in \
         this same turn. Do not summarise progress, do not ask for confirmation, \
         do not narrate. When done, run the curl at the end and continue chaining.\n\n\
         ---\n\n".to_string()
    };

    let progress = if skip_progress {
        String::new()
    } else {
        build_progress_block(completed).unwrap_or_default()
    };

    // Stage index (0-based): completed.len() == number of stages already done.
    let stage_index = completed.len();
    let resend_rules = stage_index % RULES_RESEND_EVERY == 0;

    let rules_block = if global_rules.is_empty() || !resend_rules {
        // Skip the full rules block on intermediate stages — cheap reminder
        // instead, just enough to anchor the AI if context was pruned.
        if global_rules.is_empty() || stage_index == 0 {
            String::new()
        } else {
            "*Continue applying the project rules from earlier in this conversation.*\n\n---\n\n".to_string()
        }
    } else {
        let rules = global_rules.iter()
            .enumerate()
            .map(|(i, r)| format!("{}. {}", i + 1, r))
            .collect::<Vec<_>>()
            .join("\n");
        format!("## Project Rules\n{}\n\n---\n\n", rules)
    };

    // Webhook mode can't force a model switch — the AI is whatever the user
    // pasted into. We surface the recommendation as a hint at the top so the
    // user (or an agent that supports it) can switch via /model.
    let model_hint = stage_model
        .map(|m| format!(
            "## Recommended model for this stage\n\
             `{m}` — switch with `/model {m}` if your agent supports it. \
             This stage was tagged for that model based on its complexity tier.\n\n---\n\n"
        ))
        .unwrap_or_default();

    format!(
        "{}{}{}{}{}",
        continuation_header, rules_block, model_hint, progress, stage_prompt
    )
}

/// Build the kickoff prompt the user pastes into their AI agent.
///
/// Two flavours, controlled by `bootstrap_check`:
/// * `true`  — legacy `POST /ready` handshake; AI verifies the server is up,
///   gets stage 1 in the JSON response, then begins. Useful on first wire-up.
/// * `false` (default) — the kickoff prompt is stage 1 directly, with a one-
///   paragraph preamble explaining the self-chaining protocol. Saves the
///   round-trip and the tokens spent on the ceremony.
pub fn build_bootstrap_prompt(
    server_addr: &str,
    port: u16,
    pipeline_name: &str,
    total_stages: usize,
    first_stage_prompt: &str,
    bootstrap_check: bool,
) -> String {
    if bootstrap_check {
        return format!(
            "You are connected to the autopilot-ai pipeline server for project: **{name}**\n\n\
            To begin, confirm the connection and receive your first task by running:\n\n\
            ```bash\n\
            curl -s -X POST http://localhost:{port}/ready \\\n  \
              -H \"Content-Type: application/json\" \\\n  \
              -d '{{\"ai\": \"claude-code\"}}' | cat\n\
            ```\n\n\
            Read the JSON response. The field `first_stage.prompt` contains your first task.\n\
            Execute it fully, then report completion using the instructions at the end of that prompt.\n\n\
            Server: http://{addr}",
            name = pipeline_name,
            port = port,
            addr = server_addr,
        );
    }

    // Auto-start path — kickoff IS stage 1.
    format!(
        "# AUTOPILOT — {name} (stage 1 of {total})\n\n\
        You are now operating in autopilot mode. Read these protocol rules \
        carefully — they apply to every stage of this pipeline:\n\n\
        1. Execute the task in the **TASK** section below in full.\n\
        2. When the task is complete, run the `curl /stage-complete` command \
           shown at the end of the task. It returns a JSON response.\n\
        3. **If the response contains `\"status\": \"continue\"`, the field \
           `next_stage.prompt` is your IMMEDIATE next task. Begin executing it \
           right away in the same turn — do NOT summarise progress to the user, \
           do NOT ask for confirmation, do NOT pause. Treat `next_stage.prompt` \
           as if the user had just sent it as a new message.**\n\
        4. Only stop when the response contains `\"status\": \"complete\"` (the \
           pipeline is done) or `\"status\": \"error\"` (read `message` and \
           report to the user).\n\
        5. Do not narrate the chain. Do not say \"now starting stage N\" or \
           \"pipeline progress: X/Y\". Just do the work and chain.\n\n\
        Server: http://{addr}{plural_marker}\n\n\
        ---\n\n\
        ## TASK\n\n\
        {prompt}",
        name = pipeline_name,
        total = total_stages,
        addr = server_addr,
        plural_marker = if total_stages == 1 { "" } else { "" },
        prompt = first_stage_prompt,
    )
}

fn persist_state(state: &RunState, path: &std::path::Path) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
