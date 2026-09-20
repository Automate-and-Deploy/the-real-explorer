//! Notes and tags for the entries of one folder.
//!
//! The store is a plain JSON sidecar, `.folder-meta.json`, written at the root
//! of the folder it describes and keyed by entry name, so one read serves a
//! whole details list. It is an ordinary file, which is the whole point: it
//! survives a copy, a zip, a share and a commit. Extended attributes, Finder
//! comments and `desktop.ini` are all carried separately from the bytes and
//! are dropped at the first boundary.
//!
//! A folder's own note lives in its *parent's* sidecar under its name. There
//! is exactly one home for it, so a rename in place keeps the note — the entry
//! is moved with the file — and a move to another parent leaves it behind.
//! A drive root has no parent and cannot be annotated.
//!
//! Unknown keys, at both levels, are kept and written back untouched, so a
//! sidecar written by a later build (or by another tool sharing the format)
//! does not lose fields on a round trip through this one. A `version` higher
//! than [`VERSION`] loads and displays but refuses to save.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sidecar file name, at the root of the folder it describes.
///
/// The leading dot hides it on Unix and satisfies this app's own `is_hidden`;
/// on Windows [`hide`] sets `FILE_ATTRIBUTE_HIDDEN` after every save.
pub const FILE_NAME: &str = ".folder-meta.json";

/// Schema version this build writes. A file claiming more than this is
/// displayed but never written back over.
pub const VERSION: u32 = 1;

/// Longest note kept, in bytes. A note goes into a table cell and a tooltip.
pub const MAX_NOTE: usize = 4096;

/// Most tags kept on one entry.
pub const MAX_TAGS: usize = 32;

/// Longest single tag, in characters.
pub const MAX_TAG_LEN: usize = 64;

/// Every annotated entry directly inside one folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderMeta {
    pub version: u32,
    /// Keyed by the entry's file name, not its path.
    #[serde(default)]
    pub items: BTreeMap<String, ItemMeta>,
    /// Top-level keys this build does not know, kept so a round trip through
    /// an older reader does not silently delete a newer writer's fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for FolderMeta {
    fn default() -> Self {
        Self { version: VERSION, items: BTreeMap::new(), extra: Map::new() }
    }
}

/// The note and tags on one entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemMeta {
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// RFC 3339, written on every edit.
    #[serde(default)]
    pub updated: String,
    /// Per-item keys this build does not know; see [`FolderMeta::extra`].
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ItemMeta {
    /// True when there is nothing left worth storing. An entry that reaches
    /// this state is dropped rather than written as an empty record — unless
    /// it carries keys from another writer, which are not ours to delete.
    pub fn is_empty(&self) -> bool {
        self.note.is_empty() && self.tags.is_empty() && self.extra.is_empty()
    }
}

impl FolderMeta {
    /// Move an entry to a new name, as a rename in place does. No-op when
    /// there is no entry under `from`.
    pub fn rename_entry(&mut self, from: &str, to: &str) -> bool {
        if from == to {
            return false;
        }
        match self.items.remove(from) {
            Some(item) => {
                self.items.insert(to.to_string(), item);
                true
            }
            None => false,
        }
    }

    /// Copy an entry to a second name, as a duplicate-into-the-same-folder
    /// paste does. No-op when there is no entry under `from`.
    pub fn copy_entry(&mut self, from: &str, to: &str) -> bool {
        if from == to {
            return false;
        }
        match self.items.get(from).cloned() {
            Some(item) => {
                self.items.insert(to.to_string(), item);
                true
            }
            None => false,
        }
    }

    /// Drop an entry, as a delete to the recycle bin does.
    pub fn remove_entry(&mut self, name: &str) -> bool {
        self.items.remove(name).is_some()
    }

    /// Drop entries whose name is no longer in the folder.
    ///
    /// `present` must come from a directory listing that actually succeeded:
    /// pruning against a transient read error would destroy every note in the
    /// folder. Callers therefore prune at save time, never at load time.
    pub fn prune(&mut self, present: &HashSet<String>) -> bool {
        let before = self.items.len();
        self.items.retain(|name, _| present.contains(name));
        self.items.len() != before
    }

    /// True when this file was written by a build that knows more than we do.
    pub fn is_read_only(&self) -> bool {
        self.version > VERSION
    }
}

/// Where the sidecar for `dir` lives.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Read the sidecar for `dir`.
///
/// A folder with no sidecar is `Ok(None)`, which is the common case and not an
/// error. Malformed JSON is `Err` naming the path, and the file is left
/// exactly as it was: a hand-edit with a stray comma must not cost the user
/// every note in the folder.
pub fn load(dir: &Path) -> Result<Option<FolderMeta>, String> {
    let p = path(dir);
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} could not be read: {e}", p.display())),
    };
    match serde_json::from_str::<FolderMeta>(&text) {
        Ok(m) => Ok(Some(m)),
        Err(e) => Err(format!("{} is not valid notes JSON ({e}); left untouched", p.display())),
    }
}

/// Write the sidecar for `dir`.
///
/// Writes to a sibling `.tmp` and renames over the target, as `hooks.rs` does,
/// so a crash mid-write cannot leave a truncated file of notes. The rename
/// lands a brand-new file that has none of the old one's attributes, so the
/// Windows hidden bit is re-applied on *every* save, not once at creation.
///
/// A sidecar with nothing left in it is removed rather than left behind empty.
pub fn save(dir: &Path, meta: &FolderMeta) -> Result<(), String> {
    let p = path(dir);
    if meta.is_read_only() {
        return Err(format!(
            "{} was written by a newer version (schema {}); not saving over it",
            p.display(),
            meta.version
        ));
    }
    if meta.items.is_empty() && meta.extra.is_empty() {
        return match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{} could not be removed: {e}", p.display())),
        };
    }
    let mut text = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    text.push('\n');
    let tmp = p.with_file_name(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, text.as_bytes()).map_err(|e| format!("{} could not be written: {e}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &p) {
        // Leaving the half-written temp file behind would show up in the
        // listing as junk the user did not make.
        let _ = fs::remove_file(&tmp);
        return Err(format!("{} could not be written: {e}", p.display()));
    }
    hide(&p)
}

/// Hide the sidecar from ordinary listings.
///
/// A no-op everywhere but Windows, where the leading dot means nothing.
#[cfg(windows)]
pub fn hide(p: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };

    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives both calls; neither
    // function retains the pointer.
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(format!("attributes of {} unreadable: {}", p.display(), std::io::Error::last_os_error()));
    }
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        return Ok(());
    }
    if unsafe { SetFileAttributesW(wide.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN) } == 0 {
        return Err(format!("{} could not be hidden: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The leading dot in [`FILE_NAME`] already hides the file here.
#[cfg(not(windows))]
pub fn hide(p: &Path) -> Result<(), String> {
    let _ = p;
    Ok(())
}

/// Now, as the `updated` field stores it.
pub fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Does this row survive the details-list filter?
///
/// `needle` must already be lowercased: the caller has one needle and
/// thousands of rows, and lowercasing it here would allocate once per row.
/// Everything it is matched against is lowercased here, including tags, which
/// are normally stored lowercase but need not be in a hand-edited file.
/// An empty needle passes everything, which is how an empty filter box behaves.
pub fn matches(needle: &str, name: &str, note: &str, tags: &[String]) -> bool {
    if needle.is_empty() {
        return true;
    }
    name.to_lowercase().contains(needle)
        || note.to_lowercase().contains(needle)
        || tags.iter().any(|t| t.to_lowercase().contains(needle))
}

/// Trim a typed note and cap it at [`MAX_NOTE`] bytes, on a character
/// boundary, so an accidental paste of a whole file does not become the
/// contents of a table cell.
pub fn normalize_note(raw: &str) -> String {
    let t = raw.trim();
    if t.len() <= MAX_NOTE {
        return t.to_string();
    }
    let mut end = MAX_NOTE;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].trim_end().to_string()
}

/// Parse the comma-separated tags field: trim, drop a typed `#`, lowercase,
/// drop blanks, dedupe keeping the first occurrence, and cap both the length
/// of a tag and the number of them.
pub fn normalize_tags(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let t = part.trim().trim_start_matches('#').trim().to_lowercase();
        if t.is_empty() {
            continue;
        }
        let t: String = t.chars().take(MAX_TAG_LEN).collect();
        if !out.contains(&t) {
            out.push(t);
        }
        if out.len() == MAX_TAGS {
            break;
        }
    }
    out
}

/// Tags as the Properties window shows them for editing.
pub fn tags_line(tags: &[String]) -> String {
    tags.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A fresh directory under the system temp dir, as the other modules'
    /// tests do it — this crate has no `tempfile` dev-dependency.
    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!(
            "rust-explorer-meta-{prefix}-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn item(note: &str, tags: &[&str]) -> ItemMeta {
        ItemMeta {
            note: note.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            updated: now(),
            extra: Map::new(),
        }
    }

    #[test]
    fn unknown_keys_at_both_levels_survive_a_round_trip() {
        let dir = temp_dir("unknown");
        fs::write(
            path(&dir),
            r##"{
              "version": 1,
              "generator": "some other tool",
              "items": {
                "notes.txt": { "note": "keep me", "tags": ["a"], "updated": "2026-01-01T00:00:00+00:00", "colour": "#ff0000" }
              }
            }"##,
        )
        .unwrap();

        let loaded = load(&dir).unwrap().expect("sidecar is there");
        assert_eq!(loaded.extra["generator"], Value::String("some other tool".into()));
        assert_eq!(loaded.items["notes.txt"].extra["colour"], Value::String("#ff0000".into()));

        save(&dir, &loaded).unwrap();
        let again = load(&dir).unwrap().expect("still there");
        assert_eq!(again, loaded, "a rewrite must not drop keys it did not understand");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newer_schema_loads_but_is_never_written_over() {
        let dir = temp_dir("newer");
        let original = r#"{"version":99,"items":{"a.txt":{"note":"from the future","tags":[],"updated":""}}}"#;
        fs::write(path(&dir), original).unwrap();

        let loaded = load(&dir).unwrap().expect("a newer file still loads");
        assert_eq!(loaded.items["a.txt"].note, "from the future");
        assert!(loaded.is_read_only());

        let err = save(&dir, &loaded).unwrap_err();
        assert!(err.contains("newer version"), "the message has to say why: {err}");
        assert_eq!(fs::read_to_string(path(&dir)).unwrap(), original);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_is_none_and_malformed_is_an_error_that_touches_nothing() {
        let dir = temp_dir("missing");
        assert_eq!(load(&dir).unwrap(), None);

        let broken = "{ \"version\": 1, \"items\": { oops }";
        fs::write(path(&dir), broken).unwrap();
        let err = load(&dir).unwrap_err();
        assert!(err.contains(FILE_NAME), "the message has to name the file: {err}");
        assert_eq!(fs::read_to_string(path(&dir)).unwrap(), broken, "a bad parse must not rewrite the file");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_filter_matches_name_note_and_tags() {
        let tags = vec!["tax".to_string(), "2026".to_string()];
        let cases: &[(&str, bool, &str)] = &[
            ("report", true, "name hit, and the stored name is mixed case"),
            ("quarterly", true, "note hit, and the stored note is mixed case"),
            ("tax", true, "tag hit"),
            ("2026", true, "the second tag hits too"),
            ("", true, "an empty needle passes everything"),
            ("invoice", false, "no hit anywhere"),
            ("REPORT", false, "the needle arrives lowercased; this documents that contract"),
        ];
        for (needle, want, why) in cases {
            let got = matches(needle, "Report.PDF", "Quarterly numbers", &tags);
            assert_eq!(got, *want, "{why}: {needle:?}");
        }
        // Case-insensitive on the stored side as well, for a hand-edited file.
        assert!(matches("tax", "x", "", &["TAX".to_string()]));
    }

    #[test]
    fn rename_copy_remove_and_prune_follow_the_entries() {
        let mut m = FolderMeta::default();
        m.items.insert("old.txt".into(), item("a note", &["one"]));
        m.items.insert("gone.txt".into(), item("orphan", &[]));

        assert!(m.rename_entry("old.txt", "new.txt"));
        assert!(!m.items.contains_key("old.txt"));
        assert_eq!(m.items["new.txt"].note, "a note");
        assert!(!m.rename_entry("nothing.txt", "else.txt"));

        assert!(m.copy_entry("new.txt", "new - Copy.txt"));
        assert_eq!(m.items["new - Copy.txt"].note, "a note");

        assert!(m.remove_entry("new - Copy.txt"));
        assert!(!m.remove_entry("new - Copy.txt"));

        let present: HashSet<String> = ["new.txt".to_string()].into_iter().collect();
        assert!(m.prune(&present));
        assert_eq!(m.items.keys().collect::<Vec<_>>(), vec!["new.txt"]);
        assert!(!m.prune(&present), "a second prune has nothing left to do");
    }

    #[test]
    fn tags_are_trimmed_lowercased_deduped_and_capped() {
        assert_eq!(normalize_tags("  Tax , #ARCHIVE ,, tax  "), vec!["tax", "archive"]);
        assert_eq!(normalize_tags(""), Vec::<String>::new());

        let long = "x".repeat(MAX_TAG_LEN + 40);
        assert_eq!(normalize_tags(&long)[0].chars().count(), MAX_TAG_LEN);

        let many: Vec<String> = (0..MAX_TAGS + 10).map(|i| format!("t{i}")).collect();
        assert_eq!(normalize_tags(&many.join(",")).len(), MAX_TAGS);

        let note = "n".repeat(MAX_NOTE + 100);
        assert_eq!(normalize_note(&format!("  {note}  ")).len(), MAX_NOTE);
        assert_eq!(normalize_note("  spaced  "), "spaced");
    }

    /// The bit is set after the first save and again after the second. The
    /// second save is the one that matters: it renames a brand-new temp file
    /// over a hidden target, which is exactly where the attribute is lost.
    #[cfg(windows)]
    #[test]
    fn the_hidden_attribute_survives_a_second_save() {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;

        let dir = temp_dir("hidden");
        let mut m = FolderMeta::default();
        m.items.insert("a.txt".into(), item("first", &[]));
        save(&dir, &m).unwrap();
        assert!(fs::metadata(path(&dir)).unwrap().file_attributes() & HIDDEN != 0, "hidden after one save");

        m.items.insert("b.txt".into(), item("second", &["tag"]));
        save(&dir, &m).unwrap();
        let attrs = fs::metadata(path(&dir)).unwrap().file_attributes();
        assert!(attrs & HIDDEN != 0, "hidden after two saves, attributes were {attrs:#x}");
        assert!(!path(&dir).with_file_name(format!("{FILE_NAME}.tmp")).exists(), "no temp file left behind");

        fs::remove_dir_all(&dir).ok();
    }
}
