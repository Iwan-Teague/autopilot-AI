/// git.rs — Tiny helpers to detect git context.
///
/// Used to surface the active branch in the kickoff summary so the user (and
/// the AI agent) sees which branch the pipeline is operating on. The binary
/// never mutates git state — switching branches is left to the user.

use std::path::Path;
use std::process::Command;

/// Current branch name in `cwd`, or None if cwd is not a git repo or HEAD is
/// detached. Best-effort; failure is silent.
pub fn current_branch() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `git rev-parse --abbrev-ref HEAD` returns "HEAD" when detached.
    if name.is_empty() || name == "HEAD" {
        return None;
    }
    Some(name)
}

/// True if cwd (or any ancestor) is inside a git work tree.
pub fn in_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Path to the repo root, or None if not in one.
#[allow(dead_code)]
pub fn repo_root() -> Option<std::path::PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(Path::new(&p).to_path_buf())
    }
}
