/// injector/accessibility.rs — macOS auto-paste into the Claude desktop app
/// (or any text-input GUI app) so webhook pipelines can self-chain reliably
/// even when the AI agent is wired to summarise mid-pipeline.
///
/// How it works:
///   1. Binary copies the next stage prompt to the system clipboard via `pbcopy`.
///   2. AppleScript activates the target app (default "Claude").
///   3. AppleScript sends `cmd+v` then `cmd+return`, posting a new chat turn.
///
/// The AI sees a fresh user message — same effect as the user pasting it
/// manually — so the "milestone summary" offramp doesn't kick in.
///
/// macOS only. On Linux/Windows this returns an unsupported error.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Default app name AppleScript targets if the caller doesn't override.
pub const DEFAULT_TARGET_APP: &str = "Claude";

/// Copy `text` to the system clipboard, activate `target_app`, then post the
/// pasted text as a new message (cmd+v, cmd+return).
pub fn auto_paste(text: &str, target_app: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("auto-paste is currently macOS-only (uses pbcopy + osascript)");
    }

    copy_to_clipboard(text).context("Writing prompt to clipboard")?;

    // Brief settle so the clipboard write commits before the paste.
    std::thread::sleep(Duration::from_millis(60));

    let script = format!(
        r#"tell application "{app}" to activate
delay 0.35
tell application "System Events"
    keystroke "v" using command down
    delay 0.12
    keystroke return using command down
end tell"#,
        app = escape_applescript_string(target_app)
    );

    let out = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("Running osascript")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "osascript failed (target app '{}'): {}\n\
             If this is the first run, macOS needs you to grant Accessibility \
             permission to your terminal (System Settings → Privacy & Security \
             → Accessibility).",
            target_app, stderr.trim()
        );
    }
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("Spawning pbcopy")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("pbcopy stdin unavailable"))?
        .write_all(text.as_bytes())
        .context("Writing to pbcopy stdin")?;
    let status = child.wait().context("Waiting for pbcopy")?;
    if !status.success() {
        bail!("pbcopy exited non-zero: {}", status);
    }
    Ok(())
}

/// Escape a string for use inside an AppleScript double-quoted literal.
/// Only `"` and `\` need escaping.
fn escape_applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}
