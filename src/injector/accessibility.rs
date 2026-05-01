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
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Duration;

/// Default app name when the caller doesn't override.
pub const DEFAULT_TARGET_APP: &str = "Claude";

/// Per-paste tuning. `focus_key` is a keystroke sent right after the app
/// activates and before the paste — used to land focus on the chat input
/// when the previously-focused element was something else (editor pane,
/// file tree, terminal). Format: `"mod+key"` (e.g. `"cmd+l"`, `"ctrl+/"`).
/// None = rely on AX-based focus only.
#[derive(Debug, Clone, Default)]
pub struct PasteOptions {
    pub focus_key: Option<String>,
}

/// Copy `text` to the clipboard, activate `target_app`, focus the input,
/// paste, and send.
pub fn auto_paste(text: &str, target_app: &str, opts: &PasteOptions) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return macos::auto_paste(text, target_app, opts);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::auto_paste(text, target_app, opts);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::auto_paste(text, target_app, opts);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (text, target_app, opts);
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

    pub fn auto_paste(text: &str, app: &str, opts: &PasteOptions) -> Result<()> {
        pipe_text_to(&mut Command::new("pbcopy"), text)
            .context("pbcopy: writing prompt to clipboard")?;
        std::thread::sleep(Duration::from_millis(60));

        // Build the keystroke prefix that lands focus on the chat input
        // BEFORE pasting. Two-stage strategy:
        //   1. AX-poke: walk window 1's role hierarchy and call
        //      `set focused of` on the first text-area / text-field we find.
        //      Works for most native macOS chat apps. Wrapped in `try` so a
        //      missing element doesn't blow up the whole script.
        //   2. Optional user-supplied focus_key (e.g. "cmd+l") for apps
        //      that have a dedicated focus-chat shortcut, or to recover
        //      when the AX walk found the wrong element.
        let focus_keystroke = opts.focus_key.as_deref()
            .map(translate_focus_key)
            .transpose()?
            .map(|k| format!(r#"
delay 0.05
tell application "System Events"
    {k}
end tell"#))
            .unwrap_or_default();

        let app_esc = escape_applescript(app);
        let script = format!(
            r#"tell application "{app_esc}" to activate
delay 0.35
tell application "System Events"
    tell process "{app_esc}"
        try
            -- Walk a few common element types and focus the first hit.
            try
                set focused of (first text area of window 1) to true
            on error
                try
                    set focused of (first text field of window 1) to true
                on error
                    try
                        set focused of (first scroll area of window 1 whose role description contains "text") to true
                    end try
                end try
            end try
        end try
    end tell
end tell{focus_keystroke}
delay 0.08
tell application "System Events"
    keystroke "v" using command down
    delay 0.12
    keystroke return using command down
end tell"#
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

    /// Convert "mod+key" (e.g. "cmd+l", "shift+ctrl+a") into an AppleScript
    /// keystroke line.
    fn translate_focus_key(spec: &str) -> Result<String> {
        let parts: Vec<&str> = spec.split('+').map(|s| s.trim()).collect();
        if parts.is_empty() {
            bail!("--focus-key is empty");
        }
        let key = parts.last().unwrap();
        let mods: Vec<&str> = parts[..parts.len() - 1].to_vec();

        let mod_clause = if mods.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = mods.iter().map(|m| match m.to_lowercase().as_str() {
                "cmd" | "command" | "meta" => "command down".to_string(),
                "ctrl" | "control" => "control down".to_string(),
                "shift" => "shift down".to_string(),
                "alt" | "option" => "option down".to_string(),
                other => format!("?? unknown modifier '{other}' ??"),
            }).collect();
            format!(" using {{{}}}", names.join(", "))
        };

        // Named keys → key code; printable single chars → keystroke "x".
        let lower = key.to_lowercase();
        let line = match lower.as_str() {
            "return" | "enter" => format!("key code 36{mod_clause}"),
            "escape" | "esc"   => format!("key code 53{mod_clause}"),
            "tab"              => format!("key code 48{mod_clause}"),
            "space"            => format!("key code 49{mod_clause}"),
            "delete" | "backspace" => format!("key code 51{mod_clause}"),
            _ if key.chars().count() == 1 => {
                let c = key.chars().next().unwrap();
                format!("keystroke \"{c}\"{mod_clause}")
            }
            _ => bail!("unsupported focus-key '{}': use mod+single-char or mod+return/escape/tab/space", spec),
        };
        Ok(line)
    }
}

// ===========================================================================
// Linux — X11 + Wayland
// ===========================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub fn auto_paste(text: &str, app: &str, opts: &PasteOptions) -> Result<()> {
        if is_wayland() {
            wayland_paste(text, app, opts)
        } else {
            x11_paste(text, app, opts)
        }
    }

    fn is_wayland() -> bool {
        std::env::var("WAYLAND_DISPLAY").is_ok()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|s| s.eq_ignore_ascii_case("wayland"))
                .unwrap_or(false)
    }

    fn x11_paste(text: &str, app: &str, opts: &PasteOptions) -> Result<()> {
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

        // Optional focus-chat keystroke before paste (e.g. "ctrl+l").
        if let Some(spec) = opts.focus_key.as_deref() {
            let key = translate_xdotool_key(spec)?;
            Command::new("xdotool").args(["key", &key]).status()
                .context("xdotool focus-key")?;
            std::thread::sleep(Duration::from_millis(80));
        }

        Command::new("xdotool").args(["key", "ctrl+v"]).status()?;
        std::thread::sleep(Duration::from_millis(120));
        Command::new("xdotool").args(["key", "ctrl+Return"]).status()?;
        Ok(())
    }

    fn translate_xdotool_key(spec: &str) -> Result<String> {
        // xdotool already speaks "mod+key" — we just normalise common
        // names so the user can use the same syntax across OSes.
        let parts: Vec<&str> = spec.split('+').map(|s| s.trim()).collect();
        let mut out = Vec::with_capacity(parts.len());
        for p in &parts {
            out.push(match p.to_lowercase().as_str() {
                "cmd" | "command" | "meta" => "ctrl".to_string(), // map cmd→ctrl on Linux
                "control" => "ctrl".to_string(),
                "alt" | "option" => "alt".to_string(),
                "return" | "enter" => "Return".to_string(),
                "escape" | "esc"   => "Escape".to_string(),
                _ => p.to_string(),
            });
        }
        Ok(out.join("+"))
    }

    fn wayland_paste(text: &str, _app: &str, opts: &PasteOptions) -> Result<()> {
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
            // Optional focus key — best-effort, single-modifier "mod+key" only.
            if let Some(spec) = opts.focus_key.as_deref() {
                if let Some((m, k)) = spec.rsplit_once('+') {
                    let m = match m.to_lowercase().as_str() {
                        "cmd" | "command" | "meta" | "ctrl" | "control" => "ctrl",
                        "alt" | "option" => "alt",
                        "shift" => "shift",
                        _ => "ctrl",
                    };
                    let _ = Command::new("wtype")
                        .args(["-M", m, k, "-m", m])
                        .status();
                    std::thread::sleep(Duration::from_millis(80));
                }
            }
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
            // ydotool focus_key not supported here — too many keysym→evdev
            // codes to maintain. Users on Wayland+ydotool: prefer wtype, or
            // manually focus the chat input.
            let _ = &opts.focus_key;
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

    pub fn auto_paste(text: &str, app: &str, opts: &PasteOptions) -> Result<()> {
        // SendKeys focus shortcut — optional, applied between activation and
        // the actual paste. Map "mod+key" → SendKeys notation: ^=ctrl,
        // +=shift, %=alt. Single-letter keys go literal; named keys wrap in {}.
        let focus_block = match opts.focus_key.as_deref() {
            None => String::new(),
            Some(spec) => {
                let sk = focus_key_to_sendkeys(spec);
                format!(
                    "Start-Sleep -Milliseconds 80\n[System.Windows.Forms.SendKeys]::SendWait('{sk}')\nStart-Sleep -Milliseconds 80\n"
                )
            }
        };

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
{focus_block}[System.Windows.Forms.SendKeys]::SendWait('^v')
Start-Sleep -Milliseconds 120
[System.Windows.Forms.SendKeys]::SendWait('^{{ENTER}}')
"#,
            app = app.replace('"', "`\""),
            focus_block = focus_block,
        );

        // PowerShell can read either the script body or stdin from a single
        // stream — to leave stdin free for the prompt text, we encode the
        // script as UTF-16 LE base64 and pass it via -EncodedCommand.
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

    /// Convert "mod+key" into a SendKeys-compatible string.
    fn focus_key_to_sendkeys(spec: &str) -> String {
        let parts: Vec<&str> = spec.split('+').map(|s| s.trim()).collect();
        if parts.is_empty() { return String::new(); }
        let key = *parts.last().unwrap();
        let mods = &parts[..parts.len() - 1];
        let mut out = String::new();
        for m in mods {
            match m.to_lowercase().as_str() {
                "cmd" | "command" | "meta" | "ctrl" | "control" => out.push('^'),
                "shift" => out.push('+'),
                "alt" | "option" => out.push('%'),
                _ => {}
            }
        }
        let lower = key.to_lowercase();
        match lower.as_str() {
            "return" | "enter" => out.push_str("{ENTER}"),
            "escape" | "esc"   => out.push_str("{ESC}"),
            "tab"              => out.push_str("{TAB}"),
            "space"            => out.push_str(" "),
            _ if key.chars().count() == 1 => {
                let c = key.chars().next().unwrap();
                // SendKeys reserved chars need wrapping
                if "+^%~(){}[]".contains(c) {
                    out.push('{');
                    out.push(c);
                    out.push('}');
                } else {
                    out.push(c);
                }
            }
            _ => out.push_str(key),
        }
        out
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
