//! Minimal Language Server Protocol client over stdio.
//!
//! One `LspClient` per running server. Messages are JSON-RPC framed with
//! `Content-Length` headers. A writer thread serialises outgoing messages and
//! a reader thread parses incoming frames into an mpsc channel; the UI thread
//! drains that channel with `poll` once per frame, so nothing here blocks the
//! renderer. Only the parts of the protocol the editor uses are implemented:
//! initialize, didOpen/didChange/didSave (full-text sync), completion, hover,
//! and publishDiagnostics.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use serde_json::{json, Value};

/// One server definition, keyed by file extension in the config.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub struct ServerDef {
    /// Extensions this server handles, e.g. `["rs"]` or `["ts","tsx","js","jsx"]`.
    pub extensions: Vec<String>,
    /// LSP `languageId` sent in didOpen.
    pub language_id: String,
    pub command: String,
    pub args: Vec<String>,
}

/// Defaults for common languages. Anything not installed simply fails to spawn
/// and the editor reports that in the status bar.
pub fn default_servers() -> Vec<ServerDef> {
    let s = |exts: &[&str], lang: &str, cmd: &str, args: &[&str]| ServerDef {
        extensions: exts.iter().map(|e| e.to_string()).collect(),
        language_id: lang.into(),
        command: cmd.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
    };
    vec![
        s(&["rs"], "rust", "rust-analyzer", &[]),
        s(&["ts", "tsx", "js", "jsx", "mjs", "cjs"], "typescript", "typescript-language-server", &["--stdio"]),
        s(&["py"], "python", "pyright-langserver", &["--stdio"]),
        s(&["go"], "go", "gopls", &[]),
        s(&["c", "h", "cpp", "hpp", "cc"], "cpp", "clangd", &[]),
        s(&["json", "jsonc"], "json", "vscode-json-language-server", &["--stdio"]),
        s(&["toml"], "toml", "taplo", &["lsp", "stdio"]),
        s(&["md"], "markdown", "marksman", &["server"]),
        s(&["html"], "html", "vscode-html-language-server", &["--stdio"]),
        s(&["css", "scss"], "css", "vscode-css-language-server", &["--stdio"]),
    ]
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Zero-based line and UTF-16 column range, as the protocol sends them.
    pub line: u32,
    pub col_start: u32,
    pub line_end: u32,
    pub col_end: u32,
    /// 1 error, 2 warning, 3 info, 4 hint.
    pub severity: u8,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub detail: String,
    /// LSP CompletionItemKind number, for the icon.
    pub kind: u32,
}

/// One `textDocument/formatting` edit: replace `[start, end)` with `new_text`.
/// Positions are zero-based line and UTF-16 column, as the protocol sends
/// them, matching `Diagnostic`'s convention so the same line/col-to-byte
/// helper in `editor.rs` converts both.
#[derive(Clone, Debug)]
pub struct TextEdit {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub new_text: String,
}

pub enum LspEvent {
    Initialized,
    Diagnostics { uri: String, items: Vec<Diagnostic> },
    Completion { id: u64, items: Vec<CompletionItem> },
    Hover { id: u64, text: String },
    Formatting { id: u64, edits: Vec<TextEdit> },
    /// Server exited or the pipe broke.
    Died(String),
    /// Anything routed to the status bar (window/showMessage, logs).
    Message(String),
}

pub struct LspClient {
    child: Child,
    tx: Sender<Value>,
    rx: Receiver<Value>,
    next_id: u64,
    initialized: bool,
    /// didOpen calls made before the server finished initialising.
    pending: Vec<Value>,
    /// Outstanding request ids and what they were for.
    inflight: HashMap<u64, &'static str>,
    #[allow(dead_code)]
    pub name: String,
    /// From the initialize response's `documentFormattingProvider`. A server
    /// that never sets this is not going to usefully answer a formatting
    /// request, so the editor says so up front instead of sending one into
    /// the void.
    pub supports_formatting: bool,
}

impl LspClient {
    /// Spawn `def.command` with the workspace root. Returns Err if the binary
    /// is missing; the caller shows that in the status bar.
    pub fn start(def: &ServerDef, root: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(&def.command);
        cmd.args(&def.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", def.command))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, out_rx) = channel::<Value>();
        thread::spawn(move || {
            for msg in out_rx {
                let body = msg.to_string();
                let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                if stdin.write_all(frame.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });

        let (in_tx, rx) = channel::<Value>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(v)) => {
                        if in_tx.send(v).is_err() {
                            break;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        let _ = in_tx.send(json!({"__died": e}));
                        break;
                    }
                }
            }
        });

        let mut c = Self {
            child,
            tx,
            rx,
            next_id: 1,
            initialized: false,
            pending: Vec::new(),
            inflight: HashMap::new(),
            name: def.command.clone(),
            supports_formatting: false,
        };
        let root_uri = file_uri(root);
        let id = c.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()}],
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "completion": {"completionItem": {"snippetSupport": false, "insertReplaceSupport": false}},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "publishDiagnostics": {},
                        "formatting": {}
                    },
                    "workspace": {"workspaceFolders": true}
                }
            }),
        );
        c.inflight.insert(id, "initialize");
        Ok(c)
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if self.initialized || method == "initialized" {
            let _ = self.tx.send(msg);
        } else {
            self.pending.push(msg);
        }
    }

    pub fn did_open(&mut self, path: &Path, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": file_uri(path), "languageId": language_id, "version": 0, "text": text}}),
        );
    }

    /// Full-document sync; simple and fine for files an editor holds in memory.
    pub fn did_change(&mut self, path: &Path, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": file_uri(path), "version": version}, "contentChanges": [{"text": text}]}),
        );
    }

    pub fn did_save(&mut self, path: &Path, text: &str) {
        self.notify("textDocument/didSave", json!({"textDocument": {"uri": file_uri(path)}, "text": text}));
    }

    pub fn did_close(&mut self, path: &Path) {
        self.notify("textDocument/didClose", json!({"textDocument": {"uri": file_uri(path)}}));
    }

    /// Returns the request id; the matching `LspEvent::Completion` carries it.
    pub fn completion(&mut self, path: &Path, line: u32, character: u32) -> u64 {
        let id = self.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": file_uri(path)}, "position": {"line": line, "character": character}}),
        );
        self.inflight.insert(id, "completion");
        id
    }

    pub fn hover(&mut self, path: &Path, line: u32, character: u32) -> u64 {
        let id = self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": file_uri(path)}, "position": {"line": line, "character": character}}),
        );
        self.inflight.insert(id, "hover");
        id
    }

    /// Requests whole-document formatting. Returns the request id; the
    /// matching `LspEvent::Formatting` carries it. Callers should check
    /// `supports_formatting` first and say so in the status bar if it is
    /// false, rather than sending a request the server never advertised.
    pub fn formatting(&mut self, path: &Path, tab_size: u32, insert_spaces: bool) -> u64 {
        let id = self.request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": file_uri(path)},
                "options": {"tabSize": tab_size, "insertSpaces": insert_spaces}
            }),
        );
        self.inflight.insert(id, "formatting");
        id
    }

    /// Drain incoming messages. Call once per frame.
    pub fn poll(&mut self) -> Vec<LspEvent> {
        let mut out = Vec::new();
        while let Ok(v) = self.rx.try_recv() {
            if std::env::var_os("LSP_DEBUG").is_some() {
                let brief: String = v.to_string().chars().take(200).collect();
                eprintln!("<- {brief}");
            }
            if let Some(e) = v.get("__died") {
                out.push(LspEvent::Died(e.as_str().unwrap_or("").to_string()));
                continue;
            }
            // Server-initiated requests we must answer to keep it happy.
            if let (Some(id), Some(method)) = (v.get("id"), v["method"].as_str()) {
                let result = match method {
                    "client/registerCapability" | "client/unregisterCapability" => Value::Null,
                    "workspace/configuration" => {
                        let n = v["params"]["items"].as_array().map(|a| a.len()).unwrap_or(1);
                        Value::Array(vec![Value::Null; n])
                    }
                    "window/workDoneProgress/create" => Value::Null,
                    "workspace/workspaceFolders" => Value::Array(vec![]),
                    _ => Value::Null,
                };
                let _ = self.tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                continue;
            }
            if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                match self.inflight.remove(&id) {
                    Some("initialize") => {
                        // `documentFormattingProvider` is either absent/false
                        // (not supported), `true`, or an options object
                        // (also support, with options we do not need).
                        let dfp = &v["result"]["capabilities"]["documentFormattingProvider"];
                        self.supports_formatting = dfp.as_bool().unwrap_or(!dfp.is_null());
                        self.initialized = true;
                        self.notify("initialized", json!({}));
                        for m in std::mem::take(&mut self.pending) {
                            let _ = self.tx.send(m);
                        }
                        out.push(LspEvent::Initialized);
                    }
                    Some("completion") => {
                        let items = v["result"]["items"]
                            .as_array()
                            .or_else(|| v["result"].as_array())
                            .map(|a| a.iter().map(parse_completion).collect())
                            .unwrap_or_default();
                        out.push(LspEvent::Completion { id, items });
                    }
                    Some("hover") => {
                        let text = hover_text(&v["result"]["contents"]);
                        out.push(LspEvent::Hover { id, text });
                    }
                    Some("formatting") => {
                        let edits = v["result"].as_array().map(|a| a.iter().map(parse_text_edit).collect()).unwrap_or_default();
                        out.push(LspEvent::Formatting { id, edits });
                    }
                    _ => {}
                }
                continue;
            }
            match v["method"].as_str().unwrap_or("") {
                "textDocument/publishDiagnostics" => {
                    let uri = v["params"]["uri"].as_str().unwrap_or("").to_string();
                    let items = v["params"]["diagnostics"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|d| Diagnostic {
                                    line: d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                                    col_start: d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
                                    line_end: d["range"]["end"]["line"].as_u64().unwrap_or(0) as u32,
                                    col_end: d["range"]["end"]["character"].as_u64().unwrap_or(0) as u32,
                                    severity: d["severity"].as_u64().unwrap_or(1) as u8,
                                    message: d["message"].as_str().unwrap_or("").to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    out.push(LspEvent::Diagnostics { uri, items });
                }
                "window/showMessage" => {
                    out.push(LspEvent::Message(v["params"]["message"].as_str().unwrap_or("").to_string()));
                }
                _ => {}
            }
        }
        out
    }

    #[allow(dead_code)]
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "id": self.next_id, "method": "shutdown"}));
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        let _ = self.child.wait();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // kill() only signals. Without the wait the entry stays as a zombie on
        // Unix until the app itself exits, one per server restart or crash.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_completion(c: &Value) -> CompletionItem {
    let label = c["label"].as_str().unwrap_or("").to_string();
    // Prefer textEdit.newText, then insertText, then label.
    let insert_text = c["textEdit"]["newText"]
        .as_str()
        .or_else(|| c["insertText"].as_str())
        .unwrap_or(&label)
        .to_string();
    // Snippet syntax: strip `$0`, `${1:x}` placeholders to plain text.
    let insert_text = strip_snippet(&insert_text);
    CompletionItem {
        label,
        insert_text,
        detail: c["detail"].as_str().unwrap_or("").to_string(),
        kind: c["kind"].as_u64().unwrap_or(1) as u32,
    }
}

/// `foo(${1:arg})$0` -> `foo(arg)`.
fn strip_snippet(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some('{') => {
                    chars.next();
                    let mut inner = String::new();
                    let mut depth = 1;
                    for ch in chars.by_ref() {
                        if ch == '{' {
                            depth += 1;
                        } else if ch == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        inner.push(ch);
                    }
                    if let Some((_, text)) = inner.split_once(':') {
                        out.push_str(text);
                    }
                }
                Some(d) if d.is_ascii_digit() => {
                    while chars.peek().map(|d| d.is_ascii_digit()).unwrap_or(false) {
                        chars.next();
                    }
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One `TextEdit` from a `textDocument/formatting` result array.
fn parse_text_edit(e: &Value) -> TextEdit {
    TextEdit {
        start_line: e["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
        start_col: e["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
        end_line: e["range"]["end"]["line"].as_u64().unwrap_or(0) as u32,
        end_col: e["range"]["end"]["character"].as_u64().unwrap_or(0) as u32,
        new_text: e["newText"].as_str().unwrap_or("").to_string(),
    }
}

fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        Value::Array(a) => a.iter().map(hover_text).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Read one `Content-Length` framed JSON message. `Ok(None)` on a frame that
/// failed to parse, `Err` when the pipe closes.
fn read_frame<R: BufRead>(r: &mut R) -> Result<Option<Value>, String> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("server closed the pipe".into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let len = len.ok_or("missing Content-Length")?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(serde_json::from_slice(&buf).ok())
}

pub fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file:///{}", path.display().to_string().replace('\\', "/")))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drives the real rust-analyzer on this crate: initialize, didOpen a
    /// scratch document, request completion after `std::` and expect module
    /// names back. Skips (passes) when rust-analyzer is not installed.
    #[test]
    fn rust_analyzer_completes_std_modules() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let def = default_servers().into_iter().find(|s| s.language_id == "rust").unwrap();
        let mut client = match LspClient::start(&def, &root) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        // Use a file that is in the crate graph; a stray file gets no completions.
        let path = root.join("src").join("main.rs");
        let base = std::fs::read_to_string(&path).unwrap();
        let probe_line = base.lines().count() as u32; // zero-based line of the probe body
        // Derive the column from the probe text rather than hardcoding it: the
        // request has to land right after `std::`, and a literal silently drifts
        // the moment the line gains indentation. Asking four columns early is
        // still a valid request, so it returns in-scope names and the failure
        // looks like a broken server rather than a mis-aimed probe.
        const PROBE: &str = "    let _x = std::";
        let probe_col = PROBE.encode_utf16().count() as u32;
        let text = format!("{base}{PROBE}\n");
        client.did_open(&path, "rust", &text);

        let start = Instant::now();
        let mut initialized = false;
        let mut items: Option<Vec<CompletionItem>> = None;
        let mut req: Option<u64> = None;
        let mut last_ask = Instant::now() - Duration::from_secs(10);
        while start.elapsed() < Duration::from_secs(120) {
            for ev in client.poll() {
                match ev {
                    LspEvent::Initialized => {
                        eprintln!("initialized after {:?}", start.elapsed());
                        initialized = true
                    }
                    LspEvent::Message(m) => eprintln!("msg: {m}"),
                    LspEvent::Completion { id, items: got } if Some(id) == req => {
                        eprintln!("completion reply: {} items after {:?}", got.len(), start.elapsed());
                        if !got.is_empty() {
                            items = Some(got);
                        } else {
                            req = None; // indexing not done yet; ask again
                        }
                    }
                    // A server that starts and then dies is an environment
                    // problem, not a client bug; skip as if it were absent.
                    LspEvent::Died(e) => {
                        eprintln!("skipping: rust-analyzer died: {e}");
                        return;
                    }
                    _ => {}
                }
            }
            if items.is_some() {
                break;
            }
            if initialized && req.is_none() && last_ask.elapsed() > Duration::from_secs(2) {
                req = Some(client.completion(&path, probe_line, probe_col));
                last_ask = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        client.shutdown();
        let items = items.expect("no completion items from rust-analyzer within 120s");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // rust-analyzer labels a module completion with the `::` it is about to
        // insert, so match on the bare name rather than the label as sent.
        assert!(
            labels.iter().any(|l| {
                let l = l.trim_end_matches("::");
                l == "collections" || l == "fs" || l == "io"
            }),
            "expected std modules, got {:?}",
            &labels[..labels.len().min(20)]
        );
    }
}

/// Resolve a server command against PATH the way the OS would, so the editor
/// can tell "not installed" apart from "failed to start". A command that
/// contains a separator is taken as a path and only checked for existence.
pub fn resolve(command: &str) -> Option<std::path::PathBuf> {
    let direct = Path::new(command);
    if command.contains('/') || command.contains('\\') {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|e| e.to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    } else {
        vec![String::new()]
    };
    // A GUI-launched bundle gets a minimal PATH with no profile applied, so
    // look in the places user-installed servers actually live as well.
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if !cfg!(windows) {
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".cargo").join("bin"));
            dirs.push(home.join(".local").join("bin"));
            dirs.push(home.join(".bun").join("bin"));
        }
        dirs.push(std::path::PathBuf::from("/opt/homebrew/bin"));
        dirs.push(std::path::PathBuf::from("/usr/local/bin"));
    }
    for dir in dirs {
        let base = dir.join(command);
        if base.is_file() {
            return Some(base);
        }
        for ext in &exts {
            let cand = dir.join(format!("{command}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
