//! OS-specific launchers behind one API. Each function tries the native tool
//! first and falls back to something generic. Only the Windows paths have
//! been exercised so far; macOS and Linux branches follow each platform's
//! documented CLI and need a real machine to certify.

use std::path::Path;
use std::process::Command;

fn spawn(mut cmd: Command) -> Result<(), String> {
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Open a terminal in `dir`.
/// Windows: Windows Terminal, then PowerShell, then cmd.
/// macOS: Terminal.app. Linux: $TERMINAL, then x-terminal-emulator, gnome-terminal, konsole, alacritty, kitty.
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        if spawn(cmd_with("wt.exe", &["-d"], dir)).is_ok() {
            return Ok(());
        }
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "powershell", "-NoExit"]).current_dir(dir);
        if spawn(c).is_ok() {
            return Ok(());
        }
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "cmd", "/K"]).current_dir(dir);
        spawn(c)
    }
    #[cfg(target_os = "macos")]
    {
        spawn(cmd_with("open", &["-a", "Terminal"], dir))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(t) = std::env::var("TERMINAL") {
            candidates.push(t);
        }
        for t in ["x-terminal-emulator", "gnome-terminal", "konsole", "alacritty", "kitty", "xterm"] {
            candidates.push(t.into());
        }
        for t in candidates {
            let mut c = Command::new(&t);
            c.current_dir(dir);
            if spawn(c).is_ok() {
                return Ok(());
            }
        }
        Err("no terminal emulator found".into())
    }
}

/// Open `path` (file or folder) in VS Code. Uses `code` on PATH; macOS falls
/// back to the app bundle if `code` is not installed in the shell.
pub fn open_in_code(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // `code` is a .cmd shim on Windows; run it through cmd so PATH lookup works.
        let mut c = Command::new("cmd");
        c.args(["/c", "code"]).arg(path);
        spawn(c)
    }
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("code");
        c.arg(path);
        if spawn(c).is_ok() {
            return Ok(());
        }
        spawn(cmd_with("open", &["-a", "Visual Studio Code"], path))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut c = Command::new("code");
        c.arg(path);
        spawn(c)
    }
}

/// Show `path` in the OS file manager with the item selected.
/// Windows: Explorer `/select,`. macOS: Finder `open -R`. Linux: xdg-open on the parent.
pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut c = Command::new("explorer.exe");
        c.arg(format!("/select,{}", path.display()));
        spawn(c)
    }
    #[cfg(target_os = "macos")]
    {
        spawn(cmd_with("open", &["-R"], path))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        spawn(cmd_with("xdg-open", &[], parent))
    }
}

/// Open the OS "Properties" / "Get Info" dialog for `path`. Only Windows has a
/// scriptable one (via PowerShell + Shell.Application). Others return Err so
/// the caller shows its own window.
pub fn native_properties(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let p = path.display().to_string().replace('\'', "''");
        let script = format!(
            "$s=New-Object -ComObject Shell.Application;$f=$s.NameSpace((Split-Path '{p}'));$i=$f.ParseName((Split-Path '{p}' -Leaf));$i.InvokeVerb('Properties');Start-Sleep 2"
        );
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
        spawn(c)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("no native properties dialog".into())
    }
}

/// Name of the OS file manager, for menu labels.
pub fn file_manager_name() -> &'static str {
    if cfg!(windows) {
        "Explorer"
    } else if cfg!(target_os = "macos") {
        "Finder"
    } else {
        "file manager"
    }
}

fn cmd_with(bin: &str, args: &[&str], tail: &Path) -> Command {
    let mut c = Command::new(bin);
    c.args(args).arg(tail);
    c
}

/// Show the OS "open with" chooser for `path`.
///
/// Windows has a shell dialog for this; macOS and Linux have no standard
/// picker, so they fall back to the default handler and the caller can say so.
pub fn open_with(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut c = Command::new("rundll32.exe");
        c.arg("shell32.dll,OpenAs_RunDLL").arg(path);
        spawn(c)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("no 'open with' chooser on this platform".into())
    }
}
