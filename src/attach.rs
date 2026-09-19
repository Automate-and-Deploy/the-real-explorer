//! File and folder attachments for the assistant panel.
//!
//! A drag from the tree (or table) carries `DragPaths`; the chat panel turns
//! each path into an `Attachment` at drop time, reading only metadata plus an
//! 8 KiB sniff so a large folder or file cannot stall a frame. Contents are
//! read on the worker thread at send time by `claude_block` (paths only, the
//! CLI has its own Read tool) or `openai_block` (inlined, capped, every cut
//! announced with both numbers and the constant that caused it).

use std::fs;
use std::path::{Path, PathBuf};

/// Drag payload shared by every source so the drop zone tests one type.
pub struct DragPaths(pub Vec<PathBuf>);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Text,
    Binary,
    Folder,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: Kind,
    /// Bytes for a file, 0 for a folder.
    pub size: u64,
}

/// Per-file read cap for the OpenAI-compatible backend (bytes).
pub const ATTACH_MAX_FILE_BYTES: usize = 128 * 1024;
/// Per-turn cap across all inlined attachments (bytes).
pub const ATTACH_MAX_TOTAL_BYTES: usize = 256 * 1024;
/// Per-file line cap; whichever of this and the byte cap bites first is named.
pub const ATTACH_MAX_LINES: usize = 2000;
/// Folder listing cap, matching the cwd listing already sent as system context.
pub const ATTACH_MAX_DIR_ENTRIES: usize = 200;

impl Attachment {
    /// Build from a dropped path. Err when the path is not valid UTF-8, since
    /// it would reach the model as U+FFFD and name a file that does not exist.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        if path.to_str().is_none() {
            return Err(format!("path is not valid UTF-8: {}", path.display()));
        }
        let md = fs::metadata(path).ok();
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = md.map(|m| m.len()).unwrap_or(0);
        let kind = if is_dir {
            Kind::Folder
        } else if is_binary(path) {
            Kind::Binary
        } else {
            Kind::Text
        };
        Ok(Self { path: path.to_path_buf(), kind, size })
    }

    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.path.display().to_string())
    }

    /// True when the OpenAI path will cut or skip this attachment.
    pub fn will_be_cut(&self) -> bool {
        self.kind == Kind::Binary || (self.kind == Kind::Text && self.size as usize > ATTACH_MAX_FILE_BYTES)
    }
}

/// Binary if the first 8 KiB contain a NUL byte or are not valid UTF-8.
fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else { return false };
    let mut buf = vec![0u8; 8192];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = &buf[..n];
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // A cut multi-byte sequence at the end of the window is not binary.
        Err(e) => e.error_len().is_some(),
    }
}

/// Framing for the Claude Code backend: absolute paths plus one directive.
/// No alternative to Read is mentioned, on purpose; a model reads an aside as
/// an instruction. Returns the user text unchanged when there is nothing attached.
pub fn claude_block(atts: &[Attachment], user_text: &str) -> String {
    if atts.is_empty() {
        return user_text.to_string();
    }
    let mut s = String::from("The user attached these paths from the file explorer:\n");
    for (i, a) in atts.iter().enumerate() {
        match a.kind {
            Kind::Folder => s.push_str(&format!("{}. {}  (folder)\n", i + 1, a.path.display())),
            _ => s.push_str(&format!("{}. {}  (file, {})\n", i + 1, a.path.display(), human(a.size))),
        }
    }
    s.push_str(
        "\nRead every attached file with the Read tool and list every attached folder before you answer.\nThe paths are absolute and are written exactly as they are on disk.\n\n",
    );
    s.push_str(user_text);
    s
}

/// Inline block for OpenAI-compatible backends. Returns the user message and
/// a list of notices (truncations, skips, drops) for the transcript.
pub fn openai_block(atts: &[Attachment], user_text: &str) -> (String, Vec<String>) {
    if atts.is_empty() {
        return (user_text.to_string(), Vec::new());
    }
    let mut out = String::new();
    let mut notices = Vec::new();
    let mut budget = ATTACH_MAX_TOTAL_BYTES;
    for a in atts {
        let p = a.path.display();
        match a.kind {
            Kind::Binary => {
                let line = format!(
                    "--- attached file: {p} ({} bytes) - binary (NUL byte or invalid UTF-8 within the first 8192 bytes); contents not sent, path only ---\n",
                    a.size
                );
                notices.push(format!("{}: binary, path only", a.name()));
                out.push_str(&line);
            }
            Kind::Folder => {
                let (listing, total) = list_dir(&a.path);
                out.push_str(&format!("--- attached folder: {p} ({total} entries) ---\n"));
                out.push_str(&listing);
                if total > ATTACH_MAX_DIR_ENTRIES {
                    out.push_str(&format!(
                        "--- end of {p} (listed {ATTACH_MAX_DIR_ENTRIES} of {total} entries; cut by ATTACH_MAX_DIR_ENTRIES in attach.rs) ---\n"
                    ));
                    notices.push(format!("{}: listed {ATTACH_MAX_DIR_ENTRIES} of {total} entries", a.name()));
                } else {
                    out.push_str(&format!("--- end of {p} ---\n"));
                }
            }
            Kind::Text => {
                if a.size as usize > budget {
                    out.push_str(&format!(
                        "--- attached file: {p} ({} bytes) - NOT SENT: the {ATTACH_MAX_TOTAL_BYTES}-byte per-turn attachment budget (ATTACH_MAX_TOTAL_BYTES in attach.rs) was already spent by the attachments above ---\n",
                        a.size
                    ));
                    notices.push(format!("{}: not sent, per-turn budget spent", a.name()));
                    continue;
                }
                match fs::read(&a.path) {
                    Err(e) => {
                        out.push_str(&format!("--- attached file: {p} - could not be read: {e} ---\n"));
                        notices.push(format!("{}: read failed: {e}", a.name()));
                    }
                    Ok(bytes) => {
                        let total_bytes = bytes.len();
                        let text = String::from_utf8_lossy(&bytes);
                        let total_lines = text.lines().count();
                        let mut sent_bytes = 0usize;
                        let mut sent_lines = 0usize;
                        let mut body = String::new();
                        let mut cut_by: Option<&str> = None;
                        for (i, line) in text.lines().enumerate() {
                            if sent_lines >= ATTACH_MAX_LINES {
                                cut_by = Some("ATTACH_MAX_LINES");
                                break;
                            }
                            if sent_bytes + line.len() + 1 > ATTACH_MAX_FILE_BYTES {
                                cut_by = Some("ATTACH_MAX_FILE_BYTES");
                                break;
                            }
                            body.push_str(&format!("{:>4}  {}\n", i + 1, line));
                            sent_bytes += line.len() + 1;
                            sent_lines += 1;
                        }
                        budget = budget.saturating_sub(sent_bytes);
                        out.push_str(&format!("--- attached file: {p} ({total_bytes} bytes, text) ---\n"));
                        out.push_str(&body);
                        if let Some(c) = cut_by {
                            out.push_str(&format!(
                                "--- end of {p} (TRUNCATED: sent {sent_lines} of {total_lines} lines, {sent_bytes} of {total_bytes} bytes; cut by {c} in attach.rs) ---\n"
                            ));
                            notices.push(format!("{}: sent {sent_lines} of {total_lines} lines ({c})", a.name()));
                        } else {
                            out.push_str(&format!("--- end of {p} ---\n"));
                        }
                    }
                }
            }
        }
    }
    out.push_str("\nThe blocks above are the bytes on disk as of this message. Answer using them.\n\n");
    out.push_str(user_text);
    (out, notices)
}

/// Non-recursive listing, directories first then names, both case-insensitive.
/// Returns the text and the total entry count before the cap.
fn list_dir(dir: &Path) -> (String, usize) {
    let mut entries: Vec<(bool, String, u64)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|de| {
                    let md = de.metadata().ok();
                    (
                        md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                        de.file_name().to_string_lossy().into_owned(),
                        md.map(|m| m.len()).unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    let total = entries.len();
    let mut s = String::new();
    for (is_dir, name, size) in entries.into_iter().take(ATTACH_MAX_DIR_ENTRIES) {
        if is_dir {
            s.push_str(&format!("[dir]  {name}\n"));
        } else {
            s.push_str(&format!("       {name:<24} {size}\n"));
        }
    }
    (s, total)
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tre-attach-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn text_file_is_inlined_with_line_numbers() {
        let p = tmp("a.txt");
        fs::write(&p, "one\ntwo\n").unwrap();
        let a = Attachment::from_path(&p).unwrap();
        assert_eq!(a.kind, Kind::Text);
        let (msg, notices) = openai_block(&[a], "q?");
        assert!(msg.contains("   1  one\n   2  two\n"), "{msg}");
        assert!(msg.ends_with("q?"));
        assert!(notices.is_empty());
    }

    #[test]
    fn binary_is_detected_and_not_inlined() {
        let p = tmp("b.bin");
        fs::write(&p, [0x89u8, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        let a = Attachment::from_path(&p).unwrap();
        assert_eq!(a.kind, Kind::Binary);
        let (msg, notices) = openai_block(&[a], "q");
        assert!(msg.contains("contents not sent"));
        assert!(!msg.contains("PNG"));
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn line_cap_names_itself_with_both_numbers() {
        let p = tmp("big.txt");
        let text: String = (0..(ATTACH_MAX_LINES + 50)).map(|i| format!("l{i}\n")).collect();
        fs::write(&p, &text).unwrap();
        let a = Attachment::from_path(&p).unwrap();
        let (msg, notices) = openai_block(&[a], "q");
        let expect = format!("sent {} of {} lines", ATTACH_MAX_LINES, ATTACH_MAX_LINES + 50);
        assert!(msg.contains(&expect), "{}", &msg[msg.len() - 300..]);
        assert!(msg.contains("cut by ATTACH_MAX_LINES"));
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn claude_block_lists_paths_and_keeps_user_text_last() {
        let p = tmp("c.rs");
        fs::write(&p, "fn main(){}").unwrap();
        let a = Attachment::from_path(&p).unwrap();
        let s = claude_block(&[a], "explain");
        assert!(s.starts_with("The user attached these paths"));
        assert!(s.contains(&p.display().to_string()));
        assert!(s.ends_with("explain"));
        assert_eq!(claude_block(&[], "plain"), "plain");
    }
}
