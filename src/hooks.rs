//! Model and IO layer for Claude Code "hooks" configuration living inside
//! `settings.json` / `settings.local.json` files (project, project-local, and
//! user scope). Parses only the `"hooks"` top-level key, leaves every other
//! key in the document untouched (byte-for-byte, including key order), and
//! provides a small process runner for exercising a hook command the same
//! way the real CLI would invoke it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

/// Which settings file a set of hooks lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `<cwd>/.claude/settings.json` — shared, checked into the repo.
    Project,
    /// `<cwd>/.claude/settings.local.json` — per-checkout, not shared.
    ProjectLocal,
    /// `~/.claude/settings.json` — applies across all projects for this user.
    User,
}

/// Resolve the on-disk path for a given [`Scope`], relative to `project`.
///
/// Returns `None` only for [`Scope::User`] when the home directory cannot be
/// determined (`dirs::home_dir()` returned `None`).
pub fn path_for(scope: Scope, project: &Path) -> Option<PathBuf> {
    match scope {
        Scope::Project => Some(project.join(".claude").join("settings.json")),
        Scope::ProjectLocal => Some(project.join(".claude").join("settings.local.json")),
        Scope::User => dirs::home_dir().map(|home| home.join(".claude").join("settings.json")),
    }
}

/// A single `{"type":"command", "command": "...", "timeout"?: N}` entry.
///
/// Only `type: "command"` hooks exist in the real CLI today, so `type` is
/// not modeled as a dedicated field: it round-trips through `extra` along
/// with any other keys this layer does not know about, so a newer CLI
/// version's fields survive a load/save cycle untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The shell command string to run.
    pub command: String,
    /// Optional per-hook timeout in seconds, as the CLI stores it.
    pub timeout: Option<u64>,
    /// Any JSON keys on this object other than `command`/`timeout`
    /// (typically `"type": "command"`), preserved in their original order.
    pub extra: Map<String, Value>,
}

impl Command {
    fn from_value(value: &Value) -> Self {
        let mut extra = match value.as_object() {
            Some(map) => map.clone(),
            None => Map::new(),
        };
        let command = extra
            .remove("command")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        let timeout = extra.remove("timeout").and_then(|v| v.as_u64());
        Command {
            command,
            timeout,
            extra,
        }
    }

    fn to_value(&self) -> Value {
        // Unknown keys (e.g. "type") come first, matching the shape every
        // hook command object has in the real CLI: {"type":..., "command":...}.
        let mut map = self.extra.clone();
        map.insert("command".to_string(), Value::String(self.command.clone()));
        if let Some(t) = self.timeout {
            map.insert("timeout".to_string(), Value::Number(t.into()));
        }
        Value::Object(map)
    }
}

/// One matcher group: `{"matcher"?: "...", "hooks": [Command, ...]}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The tool-name matcher, when present. Absence is meaningful (matches
    /// everything) and is distinct from an empty string, so this is
    /// `Option<String>`, not a defaulted `String`.
    pub matcher: Option<String>,
    /// The commands run for this matcher.
    pub hooks: Vec<Command>,
    /// Any JSON keys on this object other than `matcher`/`hooks`.
    pub extra: Map<String, Value>,
}

impl Group {
    fn from_value(value: &Value) -> Self {
        let obj = value.as_object();
        let mut extra = match obj {
            Some(map) => map.clone(),
            None => Map::new(),
        };
        let matcher = extra
            .remove("matcher")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let hooks = extra
            .remove("hooks")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(Command::from_value)
            .collect();
        Group {
            matcher,
            hooks,
            extra,
        }
    }

    fn to_value(&self) -> Value {
        let mut map = self.extra.clone();
        if let Some(m) = &self.matcher {
            map.insert("matcher".to_string(), Value::String(m.clone()));
        }
        let hooks: Vec<Value> = self.hooks.iter().map(Command::to_value).collect();
        map.insert("hooks".to_string(), Value::Array(hooks));
        Value::Object(map)
    }
}

/// The full parsed contents of a document's `"hooks"` key.
///
/// Event order (the order event names appeared in the JSON object) and
/// unknown event names (anything not in [`KNOWN_EVENTS`]) are both
/// preserved, since a newer CLI may define events this layer doesn't know
/// about yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hooks {
    /// `(event name, groups)` pairs, in original document order.
    pub events: Vec<(String, Vec<Group>)>,
}

impl Hooks {
    fn from_value(hooks_value: Option<&Value>) -> Self {
        let mut events = Vec::new();
        if let Some(Value::Object(map)) = hooks_value {
            for (event, groups_value) in map.iter() {
                let groups = groups_value
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(Group::from_value)
                    .collect();
                events.push((event.clone(), groups));
            }
        }
        Hooks { events }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        for (event, groups) in &self.events {
            let groups_value: Vec<Value> = groups.iter().map(Group::to_value).collect();
            map.insert(event.clone(), Value::Array(groups_value));
        }
        Value::Object(map)
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Event names the real CLI defines today. [`Hooks`] is not limited to
/// these — unknown event names are preserved on load/save — this list is
/// informational (e.g. for building a picker UI).
pub const KNOWN_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "SubagentStop",
    "SessionStart",
    "SessionEnd",
    "Notification",
    "PreCompact",
];

/// A settings document loaded from disk, along with enough metadata
/// (`mtime`/`len`) to detect a concurrent edit before writing it back.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The path this was loaded from (and that [`save`] writes back to).
    pub path: PathBuf,
    /// The full parsed JSON document, including every non-`hooks` key.
    pub root: Value,
    /// The parsed `"hooks"` key, if any.
    pub hooks: Hooks,
    /// The file's mtime at load time, or `None` if the file did not exist.
    pub mtime: Option<SystemTime>,
    /// The file's length in bytes at load time, or `0` if it did not exist.
    pub len: u64,
}

/// Failure loading a settings document.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum LoadError {
    /// The file does not exist. `load` does not actually return this
    /// variant (a missing file loads as an empty, editable document), but
    /// the variant exists so callers that want to distinguish "never
    /// existed" from a real IO failure have somewhere to put that check.
    Missing,
    /// The file exists but could not be read (permissions, etc).
    Unreadable(String),
    /// The file's contents are not valid JSON.
    Invalid(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Missing => write!(f, "settings file does not exist"),
            LoadError::Unreadable(msg) => write!(f, "could not read settings file: {msg}"),
            LoadError::Invalid(msg) => write!(f, "settings file is not valid JSON: {msg}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Load a settings document from `path`.
///
/// A missing file is not an error: it loads as an empty document (`root` is
/// an empty JSON object, `hooks` is empty), ready to be filled in and saved.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    if !path.exists() {
        return Ok(Loaded {
            path: path.to_path_buf(),
            root: Value::Object(Map::new()),
            hooks: Hooks::default(),
            mtime: None,
            len: 0,
        });
    }

    let metadata = fs::metadata(path).map_err(|e| LoadError::Unreadable(e.to_string()))?;
    let bytes = fs::read(path).map_err(|e| LoadError::Unreadable(e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let root: Value = serde_json::from_str(&text).map_err(|e| LoadError::Invalid(e.to_string()))?;
    let hooks = Hooks::from_value(root.get("hooks"));

    Ok(Loaded {
        path: path.to_path_buf(),
        root,
        hooks,
        mtime: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// Failure saving a settings document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// The file changed on disk since it was loaded (different mtime or
    /// length). Reload and retry rather than clobbering the other writer.
    ChangedOnDisk,
    /// An IO failure while backing up, writing, or renaming the file.
    Io(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::ChangedOnDisk => write!(
                f,
                "settings.json changed on disk since it was loaded; reload before saving"
            ),
            SaveError::Io(msg) => write!(f, "io error saving settings file: {msg}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Paths that have already received a one-time `.bak` copy in this process.
fn backed_up_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static BACKED_UP: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    BACKED_UP.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Write `hooks` back into the document `loaded` was read from, replacing
/// only the top-level `"hooks"` key.
///
/// Every other top-level key, and the order of all top-level keys, is left
/// exactly as it was in `loaded.root`. Before writing, the file on disk is
/// re-stat'd and compared against `loaded.mtime`/`loaded.len`; a mismatch
/// means someone else touched the file since it was loaded, and this
/// returns [`SaveError::ChangedOnDisk`] instead of overwriting their change.
pub fn save(loaded: &Loaded, hooks: &Hooks) -> Result<(), SaveError> {
    // Step 1: detect a concurrent edit before doing anything else.
    let path = &loaded.path;
    if path.exists() {
        let metadata = fs::metadata(path).map_err(|e| SaveError::Io(e.to_string()))?;
        let current_mtime = metadata.modified().ok();
        let current_len = metadata.len();
        if current_mtime != loaded.mtime || current_len != loaded.len {
            return Err(SaveError::ChangedOnDisk);
        }
    } else if loaded.mtime.is_some() {
        // Loaded believed the file existed, but it is gone now.
        return Err(SaveError::ChangedOnDisk);
    }

    // Step 2: build the new document, replacing/removing only "hooks".
    let mut root = match &loaded.root {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    if hooks.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".to_string(), hooks.to_value());
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|e| SaveError::Io(e.to_string()))?;
    text.push('\n');

    // Step 3: one-time .bak of the pre-existing file, before it is touched.
    if path.exists() {
        let mut guard = backed_up_paths()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.contains(path.as_path()) {
            let bak = bak_path(path);
            fs::copy(path, &bak).map_err(|e| SaveError::Io(e.to_string()))?;
            guard.insert(path.clone());
        }
    }

    // Step 4: write to a sibling .tmp file, then atomically rename over the
    // original, so a crash mid-write never leaves a truncated settings file.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SaveError::Io(e.to_string()))?;
    }
    let tmp_path = tmp_path(path);
    fs::write(&tmp_path, text.as_bytes()).map_err(|e| SaveError::Io(e.to_string()))?;
    fs::rename(&tmp_path, path).map_err(|e| SaveError::Io(e.to_string()))?;

    Ok(())
}

fn bak_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// The outcome of running a hook command via [`run_test`].
#[derive(Debug, Clone)]
pub struct TestResult {
    /// The child's exit code, or `None` if it was killed (timeout) or the
    /// exit code could not be determined.
    pub exit_code: Option<i32>,
    /// Whatever was captured from stdout before the read grace period ended.
    pub stdout: String,
    /// Whatever was captured from stderr before the read grace period ended.
    pub stderr: String,
    /// Wall-clock time from spawn to when this returned.
    pub duration: Duration,
    /// `true` if the command was killed for exceeding `timeout`.
    pub timed_out: bool,
}

fn next_hex_chunk(len: usize) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // fetch_add guarantees a distinct counter per call even when several
    // calls land in the same nanosecond (seen in practice under parallel
    // test threads); the golden-ratio multiply spreads that single
    // increment across the whole 64-bit word instead of leaving it stuck
    // in a handful of low bits.
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) as u64;
    let pid = std::process::id() as u64;
    let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.rotate_left(13);
    let hex = format!("{mixed:016x}");
    hex.chars().take(len.min(hex.len())).collect()
}

/// Build a plausible `session_id` in the same shape the real CLI uses
/// (lowercase hex, dash-grouped 8-4-4-4-12). It is not a real UUID (no
/// version/variant bits are set) — it only needs to look like one for a
/// hook script that pattern-matches the field.
fn fake_session_id() -> String {
    format!(
        "{}-{}-{}-{}-{}",
        next_hex_chunk(8),
        next_hex_chunk(4),
        next_hex_chunk(4),
        next_hex_chunk(4),
        next_hex_chunk(12)
    )
}

/// Build the JSON stdin payload the real CLI feeds a hook for `event`,
/// for manual testing of a hook command outside the CLI.
pub fn sample_stdin(event: &str, cwd: &Path) -> String {
    let mut root = Map::new();
    root.insert(
        "session_id".to_string(),
        Value::String(fake_session_id()),
    );
    root.insert("transcript_path".to_string(), Value::String(String::new()));
    root.insert(
        "cwd".to_string(),
        Value::String(cwd.to_string_lossy().into_owned()),
    );
    root.insert(
        "hook_event_name".to_string(),
        Value::String(event.to_string()),
    );
    if event == "PreToolUse" || event == "PostToolUse" {
        root.insert("tool_name".to_string(), Value::String("Bash".to_string()));
        let mut tool_input = Map::new();
        tool_input.insert(
            "command".to_string(),
            Value::String("echo hi".to_string()),
        );
        root.insert("tool_input".to_string(), Value::Object(tool_input));
    }
    serde_json::to_string(&Value::Object(root)).unwrap_or_default()
}

#[cfg(windows)]
fn spawn_shell(command: &str, cwd: &Path) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // `raw_arg`: the stored value is already a shell string (quotes included).
    // `.arg` would re-escape it and cmd would see the quotes as part of the path.
    ProcCommand::new("cmd")
        .arg("/c")
        .raw_arg(command)
        .current_dir(cwd)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// PATH with the directories a desktop launch never inherits appended.
///
/// Same set `lsp::resolve` searches, so a hook and a language server agree on
/// where user-installed tools live.
#[cfg(not(windows))]
fn augmented_path() -> std::ffi::OsString {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(home) = dirs::home_dir() {
        for sub in [".cargo/bin", ".local/bin", ".bun/bin"] {
            dirs.push(home.join(sub));
        }
    }
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let p = std::path::PathBuf::from(extra);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

#[cfg(not(windows))]
fn spawn_shell(command: &str, cwd: &Path) -> std::io::Result<std::process::Child> {
    // A plain `sh -c`, with PATH extended by hand.
    //
    // An app launched from the Dock, Finder or a .desktop file inherits a bare
    // PATH and no profile, so a hook in ~/.cargo/bin would be unreachable. A
    // login shell would fix that but costs more than it gives on Linux, where
    // /bin/sh is dash and dash treats a failed `.` as fatal: one stale line in
    // ~/.profile left by a removed toolchain makes every hook exit 2 with no
    // output and no explanation. Measured on a real box whose profile still
    // sourced a deleted cargo env. Extending PATH ourselves is deterministic,
    // needs no profile, and matches how `lsp::resolve` finds servers.
    ProcCommand::new("sh")
        .arg("-c")
        .arg(command)
        .env("PATH", augmented_path())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Run `command` as a hook would run, feeding it `stdin` and capturing
/// output, the same way the real CLI invokes a hook (platform shell: `cmd
/// /c` on Windows with a hidden console window, `sh -c` elsewhere).
///
/// # Why this polls `try_wait` instead of reading stdout to EOF
///
/// A hook is free to spawn a detached grandchild (e.g. `start /b ...`) that
/// inherits the pipe handles. If this waited for the stdout/stderr pipes to
/// hit EOF, a hook that itself exits in milliseconds but leaves a
/// long-lived detached grandchild holding the pipe open would be
/// misreported as hung/timed-out — the EOF never arrives until the
/// grandchild also exits, which may be much later (or never). Instead this
/// polls the *child's* exit status directly, reads whatever the pipe reader
/// threads have buffered so far, waits a short grace period for them to
/// flush, and then drops the pipes regardless of whether they are still
/// open. That correctly reports "the hook finished quickly" even when it
/// left something running in the background.
pub fn run_test(command: &str, stdin: &str, cwd: &Path, timeout: Duration) -> TestResult {
    let start = Instant::now();

    let mut child = match spawn_shell(command, cwd) {
        Ok(c) => c,
        Err(e) => {
            return TestResult {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to spawn: {e}"),
                duration: start.elapsed(),
                timed_out: false,
            };
        }
    };

    if let Some(mut child_stdin) = child.stdin.take() {
        let _ = child_stdin.write_all(stdin.as_bytes());
        // Drop closes the pipe so the child sees EOF on its stdin.
    }

    let stdout_buf = std::sync::Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = std::sync::Arc::new(Mutex::new(Vec::new()));

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_thread = stdout_pipe.map(|mut pipe| {
        let buf = std::sync::Arc::clone(&stdout_buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        })
    });
    let stderr_thread = stderr_pipe.map(|mut pipe| {
        let buf = std::sync::Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        })
    });

    // Poll the child's own exit status; do NOT block on the pipe readers.
    let mut timed_out = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break None,
        }
    };

    // Grace period for the reader threads to flush whatever is already in
    // the pipe buffer, then move on regardless of whether they finished
    // (a lingering detached grandchild can keep a pipe open indefinitely).
    let grace_deadline = Instant::now() + Duration::from_millis(200);
    if let Some(t) = stdout_thread {
        let remaining = grace_deadline.saturating_duration_since(Instant::now());
        wait_with_timeout(t, remaining);
    }
    if let Some(t) = stderr_thread {
        let remaining = grace_deadline.saturating_duration_since(Instant::now());
        wait_with_timeout(t, remaining);
    }

    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap_or_else(|p| p.into_inner())).into_owned();

    TestResult {
        exit_code: exit_status.and_then(|s| s.code()),
        stdout,
        stderr,
        duration: start.elapsed(),
        timed_out,
    }
}

/// Join a thread, but give up waiting after `timeout` and leave it detached
/// (it will finish on its own; we just stop blocking on it).
fn wait_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) {
    if handle.is_finished() {
        let _ = handle.join();
        return;
    }
    if timeout.is_zero() {
        return;
    }
    // Simplest portable "join with timeout": poll is_finished.
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Kept only to silence an unused-import lint if HashMap is ever needed by
// downstream consumers copy-pasting from this module during review.
#[allow(dead_code)]
type _Unused = HashMap<(), ()>;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn fixture_value() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    // 13 unrelated top-level keys (k01..k13) in a specific order, plus
    // "hooks", matching the real shape: a UserPromptSubmit/Stop-style group
    // with no matcher, and a PreToolUse-style group that does have one, so
    // both "matcher present" and "matcher absent" round-trip.
    const FIXTURE: &str = r#"{
  "k01": "a",
  "k02": 2,
  "k03": true,
  "k04": null,
  "k05": [1, 2, 3],
  "k06": {"nested": "obj"},
  "k07": "g",
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "node hook1.mjs"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "node hook2.mjs",
            "timeout": 30
          }
        ]
      }
    ]
  },
  "k08": "h",
  "k09": 9,
  "k10": false,
  "k11": "k",
  "k12": {"a": 1, "b": 2},
  "k13": [true, false]
}"#;

    #[test]
    fn round_trip_no_edits_is_byte_identical() {
        let dir = std::env::temp_dir().join(format!("hooks_test_{}", next_hex_chunk(8)));
        let path = dir.join("settings.json");
        write_file(&path, FIXTURE);

        let loaded = load(&path).expect("load should succeed");
        // No matcher key at all for the UserPromptSubmit group.
        assert_eq!(loaded.hooks.events[0].0, "UserPromptSubmit");
        assert!(loaded.hooks.events[0].1[0].matcher.is_none());

        let hooks = loaded.hooks.clone();
        save(&loaded, &hooks).expect("save should succeed");

        let expected = serde_json::to_string_pretty(&fixture_value()).unwrap() + "\n";
        let actual = fs::read_to_string(&path).unwrap();
        assert_eq!(actual, expected);
        assert!(!actual.contains("\"matcher\": null"));

        // Confirm no "matcher" key was invented for the group that had none.
        let saved_value: Value = serde_json::from_str(&actual).unwrap();
        let group0 = &saved_value["hooks"]["UserPromptSubmit"][0];
        assert!(group0.get("matcher").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adding_a_group_round_trips_and_keeps_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("hooks_test_{}", next_hex_chunk(8)));
        let path = dir.join("settings.json");
        write_file(&path, FIXTURE);

        let loaded = load(&path).expect("load should succeed");
        let mut hooks = loaded.hooks.clone();
        hooks.events.push((
            "Stop".to_string(),
            vec![Group {
                matcher: Some("Bash".to_string()),
                hooks: vec![Command {
                    command: "echo done".to_string(),
                    timeout: None,
                    extra: Map::new(),
                }],
                extra: Map::new(),
            }],
        ));
        save(&loaded, &hooks).expect("save should succeed");

        let reloaded = load(&path).expect("reload should succeed");
        let stop = reloaded
            .hooks
            .events
            .iter()
            .find(|(name, _)| name == "Stop")
            .expect("Stop event present");
        assert_eq!(stop.1[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(stop.1[0].hooks[0].command, "echo done");

        assert_eq!(reloaded.root["k01"], "a");
        assert_eq!(reloaded.root["k07"], "g");
        assert_eq!(reloaded.root["k13"], serde_json::json!([true, false]));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changed_on_disk_is_detected() {
        let dir = std::env::temp_dir().join(format!("hooks_test_{}", next_hex_chunk(8)));
        let path = dir.join("settings.json");
        write_file(&path, FIXTURE);

        let loaded = load(&path).expect("load should succeed");

        // Touch with different content and, if the filesystem's mtime
        // granularity is coarse, force a length change too so the check is
        // guaranteed to see *something* different.
        std::thread::sleep(Duration::from_millis(20));
        write_file(&path, &format!("{FIXTURE}\n"));

        let hooks = loaded.hooks.clone();
        let result = save(&loaded, &hooks);
        assert_eq!(result, Err(SaveError::ChangedOnDisk));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bak_created_once() {
        let dir = std::env::temp_dir().join(format!("hooks_test_{}", next_hex_chunk(8)));
        let path = dir.join("settings.json");
        write_file(&path, FIXTURE);
        let bak = dir.join("settings.json.bak");

        let loaded1 = load(&path).expect("load should succeed");
        let hooks1 = loaded1.hooks.clone();
        save(&loaded1, &hooks1).expect("first save should succeed");
        assert!(bak.exists());
        let bak_contents_first = fs::read_to_string(&bak).unwrap();

        // Second save from a fresh load: .bak must NOT be overwritten again
        // (it is a one-time-per-process copy of the *original* file).
        let loaded2 = load(&path).expect("reload should succeed");
        let mut hooks2 = loaded2.hooks.clone();
        hooks2.events.push((
            "SessionStart".to_string(),
            vec![Group {
                matcher: None,
                hooks: vec![Command {
                    command: "echo start".to_string(),
                    timeout: None,
                    extra: Map::new(),
                }],
                extra: Map::new(),
            }],
        ));
        save(&loaded2, &hooks2).expect("second save should succeed");
        let bak_contents_second = fs::read_to_string(&bak).unwrap();
        assert_eq!(bak_contents_first, bak_contents_second);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(windows)]
    fn run_test_echo_succeeds() {
        let cwd = std::env::temp_dir();
        let result = run_test("echo hi", "", &cwd, Duration::from_secs(5));
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hi"), "stdout was: {:?}", result.stdout);
        assert!(!result.timed_out);
    }

    /// Covers the `sh -lc` branch. A login shell is used so a hook can find
    /// tools installed under the user's profile, which a GUI-launched app
    /// would otherwise miss; this proves the spawn still works.
    #[test]
    #[cfg(not(windows))]
    fn run_test_echo_succeeds_on_unix() {
        let cwd = std::env::temp_dir();
        let result = run_test("echo hi", "", &cwd, Duration::from_secs(10));
        assert_eq!(result.exit_code, Some(0), "stderr: {}", result.stderr);
        assert!(result.stdout.contains("hi"), "stdout was: {:?}", result.stdout);
        assert!(!result.timed_out);
    }

    /// A login shell sources profile scripts, which are free to print. Anything
    /// they emit lands in the hook's captured stdout, so keep an eye on it.
    #[test]
    #[cfg(not(windows))]
    fn run_test_passes_quoted_paths_through_unchanged_on_unix() {
        let dir = std::env::temp_dir().join(format!("tre hooks {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x y.txt");
        std::fs::write(&file, "quoted-ok").unwrap();
        let cmd = format!("cat \"{}\"", file.display());
        let r = run_test(&cmd, "", &dir, Duration::from_secs(10));
        assert_eq!(r.exit_code, Some(0), "stderr: {}", r.stderr);
        assert!(r.stdout.contains("quoted-ok"), "stdout: {}", r.stdout);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn run_test_nonexistent_command_fails_fast_not_timeout() {
        let cwd = std::env::temp_dir();
        let result = run_test(
            "this_command_does_not_exist_xyz",
            "",
            &cwd,
            Duration::from_secs(5),
        );
        assert_ne!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert!(!result.stderr.is_empty(), "expected stderr, got none");
    }

    #[test]
    #[cfg(windows)]
    fn run_test_detached_grandchild_does_not_misreport_timeout() {
        let cwd = std::env::temp_dir();
        let start = Instant::now();
        let result = run_test(
            "start /b cmd /c ping -n 6 127.0.0.1 >nul",
            "",
            &cwd,
            Duration::from_secs(10),
        );
        let elapsed = start.elapsed();
        assert!(!result.timed_out, "should not report timeout: {:?}", result);
        assert!(
            elapsed < Duration::from_secs(2),
            "took too long: {elapsed:?}"
        );
    }

    #[test]
    fn sample_stdin_shapes_match_event() {
        let cwd = Path::new("/tmp/proj");
        let generic: Value = serde_json::from_str(&sample_stdin("Stop", cwd)).unwrap();
        assert!(generic.get("tool_name").is_none());
        assert_eq!(generic["hook_event_name"], "Stop");

        let pre: Value = serde_json::from_str(&sample_stdin("PreToolUse", cwd)).unwrap();
        assert_eq!(pre["tool_name"], "Bash");
        assert_eq!(pre["tool_input"]["command"], "echo hi");
    }

    #[test]
    fn path_for_scopes() {
        let project = Path::new("/some/project");
        assert_eq!(
            path_for(Scope::Project, project).unwrap(),
            project.join(".claude").join("settings.json")
        );
        assert_eq!(
            path_for(Scope::ProjectLocal, project).unwrap(),
            project.join(".claude").join("settings.local.json")
        );
        // User scope depends on dirs::home_dir(); just assert it resolves
        // to *something* ending in the right suffix when present.
        if let Some(p) = path_for(Scope::User, project) {
            assert!(p.ends_with(Path::new(".claude").join("settings.json")));
        }
    }

    /// The stored command is a shell string with its own quoting; it must reach
    /// cmd verbatim (`raw_arg`), or a quoted path is looked up quotes included.
    #[cfg(windows)]
    #[test]
    fn run_test_passes_quoted_paths_through_unchanged() {
        let dir = std::env::temp_dir().join(format!("tre hooks {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x y.txt");
        std::fs::write(&file, "quoted-ok").unwrap();
        let cmd = format!("type \"{}\"", file.display());
        let r = run_test(&cmd, "", &dir, Duration::from_secs(10));
        assert_eq!(r.exit_code, Some(0), "stderr: {}", r.stderr);
        assert!(r.stdout.contains("quoted-ok"), "stdout: {}", r.stdout);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
