/// config.rs — Pipeline configuration types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::provider::ProviderHint;

// ---------------------------------------------------------------------------
// Top-level pipeline config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Human-readable name for this pipeline run.
    pub name: String,

    /// Rules prepended to every prompt.
    pub global_rules: Vec<String>,

    /// Ordered list of stages to execute.
    pub stages: Vec<Stage>,

    /// Which interface to use. Auto-detected when omitted.
    #[serde(default)]
    pub interface: InterfaceHint,

    /// Which AI provider to use for API mode. Auto-detected when omitted.
    #[serde(default)]
    pub provider: ProviderHint,

    /// Override the provider's API base URL.
    #[serde(default)]
    pub api_base_url: Option<String>,

    /// Name of the env var holding the API key (e.g. "MY_KEY").
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Model name override (e.g. "llama3.2", "gpt-4o").
    #[serde(default)]
    pub model: Option<String>,

    /// Watchdog timeout in seconds. Warns if a stage takes longer than this.
    /// Default: 1800 (30 min). Set to 0 to disable.
    #[serde(default = "default_stage_timeout")]
    pub stage_timeout_secs: u64,

    /// When true, the kickoff prompt is a `POST /ready` handshake — the AI
    /// pings the server first to verify connectivity, gets stage 1 in the
    /// response, and starts executing. Useful the first time you wire up an
    /// agent. When false (default), the kickoff prompt IS stage 1 directly,
    /// saving the round-trip and the tokens spent on the ceremony.
    #[serde(default)]
    pub bootstrap_check: bool,

    /// Where runtime state is persisted for resume support.
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,
}

fn default_stage_timeout() -> u64 { 1800 }
fn default_state_path() -> PathBuf { PathBuf::from("autopilot-state.json") }

// ---------------------------------------------------------------------------
// Stage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub id: String,
    pub phase: Phase,
    /// One-line description shown in the progress header and summary file.
    pub summary: String,
    /// Full prompt text. Global rules are prepended by the server.
    pub prompt: String,
    /// Optional per-stage model override. When set, API/CLI modes use this
    /// model for this stage; webhook mode prepends a "Suggested model" hint
    /// to the prompt so the AI can switch via /model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Optional per-stage watchdog timeout in seconds. Overrides the
    /// pipeline-level `stage_timeout_secs` for this stage only. Useful for
    /// short stages (lint, format) where 30 min is wasteful, or for long
    /// stages (large refactors) where 30 min isn't enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    /// If true, the rolling "Progress so far" block is omitted from this
    /// stage's prompt. Use for stages that don't depend on prior work
    /// (independent feature implementations, parallel modules). Default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_progress: bool,
}

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Foundation,
    Implementation,
    Testing,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Foundation     => write!(f, "Foundation"),
            Phase::Implementation => write!(f, "Implementation"),
            Phase::Testing        => write!(f, "Testing"),
        }
    }
}

// ---------------------------------------------------------------------------
// InterfaceHint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceHint {
    #[default]
    Auto,
    /// AI self-chains via POST /ready → /stage-complete (recommended).
    Webhook,
    /// Binary calls provider REST API directly.
    Api,
    /// Binary spawns `claude`, `codex`, etc. as a subprocess per stage.
    Cli,
}

impl InterfaceHint {
    pub fn label(&self) -> &'static str {
        match self {
            InterfaceHint::Auto    => "auto-detected",
            InterfaceHint::Webhook => "webhook (AI self-chain via localhost)",
            InterfaceHint::Api     => "API (provider auto-detected)",
            InterfaceHint::Cli     => "CLI subprocess",
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub current_stage: usize,
    pub stages: Vec<StageState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub id: String,
    pub status: StageStatus,
    pub completed_at: Option<u64>,
    /// Duration in seconds from stage start to completion.
    pub duration_secs: Option<u64>,
    /// One-line summary provided by the AI when it completed this stage.
    /// Used to build the "progress so far" block in subsequent prompts.
    #[serde(default)]
    pub ai_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl RunState {
    pub fn new(stages: &[Stage]) -> Self {
        RunState {
            current_stage: 0,
            stages: stages
                .iter()
                .map(|s| StageState {
                    id: s.id.clone(),
                    status: StageStatus::Pending,
                    completed_at: None,
                    duration_secs: None,
                    ai_summary: None,
                })
                .collect(),
        }
    }
}
