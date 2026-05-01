/// injector/accessibility.rs — cross-platform auto-paste into a chat GUI.
///
/// Drives any chat app that accepts the platform's standard "send message"
/// keystroke after a paste. Used so webhook pipelines self-chain reliably
/// even when the AI agent is wired to summarise mid-pipeline.
///
/// Per stage:
///   1. Copy next prompt to system clipboard.
///   2. Activate the target app.
///   3. Send paste, then send.
///
/// Platform implementations:
///   • macOS   — pbcopy + osascript (System Events keystrokes)
///   • Linux   — xclip/xsel + xdotool on X11; wl-copy + ydotool/wtype on Wayland
///   • Windows — Set-Clipboard + PowerShell SendKeys via SetForegroundWindow
///
/// Defaults to "Claude" as target. Override with --target-app for Codex,
/// ChatGPT, Cursor, or any other chat GUI.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Default app name when the caller doesn't override.
pub const DEFAULT_TARGET_APP: &str = "Claude";

/// Copy `text` to the clipboard, activate `target_app`, paste, and send.
pub fn auto_paste(text: &str, target_app: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return macos::auto_paste(text, target_app);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::auto_paste(text, target_app);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::auto_paste(text, target_app);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (text, target_app);
        bail!("auto-paste is unsupported on this OS")
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn tool_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pipe_text_to(cmd: &mut Command, text: &str) -> Result<()> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Spawning {:?}", cmd))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("child stdin unavailable"))?
        .write_all(text.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "{:?} exited {}: {}",
            cmd,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ===========================================================================
// macOS
// ===========================================================================

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub fn auto_paste(text: &str, app: &str) -> Result<()> {
        pipe_text_to(&mut Command::new("pbcopy"), text)
            .context("pbcopy: writing prompt to clipboard")?;
        std::thread::sleep(Duration::from_millis(60));

        let script = format!(
            r#"tell application "{app}" to activate
delay 0.35
tell application "System Events"
    keystroke "v" using command down
    delay 0.12
    keystroke return using command down
end tell"#,
            app = escape_applescript(app)
        );
        let out = Command::new("osascript")
            .arg("-e").arg(&script)
            .output()
            .context("Running osascript")?;
        if !out.status.success() {
            bail!(
                "osascript failed (target '{}'): {}\n\
                 macOS may need Accessibility permission for this terminal — \
                 System Settings → Privacy & Security → Accessibility.",
                app,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn escape_applescript(s: &str) -> String {
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
}

// ===========================================================================
// Linux — X11 + Wayland
// ===========================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub fn auto_paste(text: &str, app: &str) -> Result<()> {
        if is_wayland() {
            wayland_paste(text, app)
        } else {
            x11_paste(text, app)
        }
    }

    fn is_wayland() -> bool {
        std::env::var("WAYLAND_DISPLAY").is_ok()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|s| s.eq_ignore_ascii_case("wayland"))
                .unwrap_or(false)
    }

    fn x11_paste(text: &str, app: &str) -> Result<()> {
        // Clipboard.
        if tool_exists("xclip") {
            pipe_text_to(
                Command::new("xclip").args(["-selection", "clipboard"]),
                text,
            )?;
        } else if tool_exists("xsel") {
            pipe_text_to(Command::new("xsel").args(["-b", "-i"]), text)?;
        } else {
            bail!("auto-paste needs xclip or xsel on X11. Install: apt install xclip");
        }

        if !tool_exists("xdotool") {
            bail!("auto-paste needs xdotool on X11. Install: apt install xdotool");
        }

        std::thread::sleep(Duration::from_millis(60));

        // Activate target window by title substring.
        let activate = Command::new("xdotool")
            .args(["search", "--name", app, "windowactivate"])
            .status()
            .context("xdotool windowactivate")?;
        if !activate.success() {
            bail!("xdotool could not find a window matching '{}'", app);
        }
        std::thread::sleep(Duration::from_millis(350));

        Command::new("xdotool").args(["key", "ctrl+v"]).status()?;
        std::thread::sleep(Duration::from_millis(120));
        Command::new("xdotool").args(["key", "ctrl+Return"]).status()?;
        Ok(())
    }

    fn wayland_paste(text: &str, _app: &str) -> Result<()> {
        if !tool_exists("wl-copy") {
            bail!("auto-paste needs wl-copy on Wayland. Install: apt install wl-clipboard");
        }
        pipe_text_to(&mut Command::new("wl-copy"), text)?;

        // Wayland forbids programmatic window activation by design.
        // The user has 1.5s to give the target app focus before we type.
        tracing::warn!(
            "Wayland: focus the target app within 1.5s — Wayland blocks \
             programmatic window activation"
        );
        std::thread::sleep(Duration::from_millis(1500));

        if tool_exists("wtype") {
            // wtype: -M holds modifier, -k presses a named key, -m releases mod.
            Command::new("wtype")
                .args(["-M", "ctrl", "v", "-m", "ctrl"])
                .status()
                .context("wtype paste")?;
            std::thread::sleep(Duration::from_millis(120));
            Command::new("wtype")
                .args(["-M", "ctrl", "-k", "Return", "-m", "ctrl"])
                .status()
                .context("wtype send")?;
            Ok(())
        } else if tool_exists("ydotool") {
            // Linux input event codes: ctrl=29, v=47, enter=28. ":1" press, ":0" release.
            Command::new("ydotool")
                .args(["key", "29:1", "47:1", "47:0", "29:0"])
                .status()
                .context("ydotool paste — needs ydotool daemon running")?;
            std::thread::sleep(Duration::from_millis(120));
            Command::new("ydotool")
                .args(["key", "29:1", "28:1", "28:0", "29:0"])
                .status()
                .context("ydotool send")?;
            Ok(())
        } else {
            bail!(
                "auto-paste on Wayland needs wtype or ydotool. \
                 Install: apt install wtype  (or set up the ydotool daemon)."
            )
        }
    }
}

// ===========================================================================
// Windows
// ===========================================================================

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    pub fn auto_paste(text: &str, app: &str) -> Result<()> {
        // PowerShell handles clipboard, window activation, and SendKeys all
        // in one shot. Pipe the prompt as input to avoid quoting hell.
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$txt = [Console]::In.ReadToEnd()
Set-Clipboard -Value $txt
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win {{
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
}}
"@
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like "*{app}*" }} | Select-Object -First 1
if (-not $proc) {{
    Write-Error "Window matching '{app}' not found"
    exit 1
}}
[Win]::ShowWindow($proc.MainWindowHandle, 9) | Out-Null
[Win]::SetForegroundWindow($proc.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 350
[System.Windows.Forms.SendKeys]::SendWait('^v')
Start-Sleep -Milliseconds 120
[System.Windows.Forms.SendKeys]::SendWait('^{{ENTER}}')
"#,
            app = app.replace('"', "`\"")
        );

        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Spawning PowerShell")?;

        // PowerShell reads the script body from -Command -, so first feed the
        // script, then the prompt text — but PowerShell can only read one
        // stream from stdin. Workaround: write script to a temp file, run it
        // with -File, and feed the prompt as the file's stdin.
        // Simpler approach below: encode the script to base64 and use
        // -EncodedCommand, leaving stdin free for the prompt.

        // Drop the open child — restart with the encoded path.
        drop(child);

        let mut wide: Vec<u16> = script.encode_utf16().collect();
        // PowerShell's -EncodedCommand expects UTF-16 LE -> base64.
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for w in wide.drain(..) {
            bytes.push((w & 0xff) as u8);
            bytes.push((w >> 8) as u8);
        }
        let encoded = base64_encode(&bytes);

        let mut ch = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Spawning PowerShell (encoded)")?;
        ch.stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("PowerShell stdin unavailable"))?
            .write_all(text.as_bytes())?;
        let out = ch.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "PowerShell auto-paste failed (target '{}'): {}",
                app,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Tiny base64 encoder so we don't pull in another dep just for this.
    fn base64_encode(input: &[u8]) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0];
            let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
            let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
            out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(CHARS[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}
