//! File-type icons from the Phosphor icon font, with a colour per type.
//! Colours follow the Tokyo Night palette so they read well on both the
//! Omarchy theme and the stock dark theme; on light themes they are darkened.

use eframe::egui::{self, Color32};
use egui_phosphor::regular as ph;

/// Register the Phosphor font for both proportional and monospace families.
/// `egui_phosphor::add_to_fonts` only touches the proportional family, and the
/// Omarchy theme renders everything in monospace.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if let Some(keys) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        keys.push("phosphor".into());
    }
    ctx.set_fonts(fonts);
}

pub const BACK: &str = ph::ARROW_LEFT;
pub const FORWARD: &str = ph::ARROW_RIGHT;
pub const UP: &str = ph::ARROW_UP;
pub const REFRESH: &str = ph::ARROWS_CLOCKWISE;
pub const SETTINGS: &str = ph::GEAR;
pub const CHAT: &str = ph::CHAT_CIRCLE_TEXT;
pub const CLOSE: &str = ph::X;
pub const CARET_OPEN: &str = ph::CARET_DOWN;
pub const CARET_CLOSED: &str = ph::CARET_RIGHT;
pub const DRIVE: &str = ph::HARD_DRIVE;
pub const EXPLORER: &str = ph::FOLDERS;
pub const IDE: &str = ph::CODE;

// Tokyo Night accents.
const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9e, 0x64);
const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
const TEAL: Color32 = Color32::from_rgb(0x73, 0xda, 0xca);
const CYAN: Color32 = Color32::from_rgb(0x7d, 0xcf, 0xff);
const PURPLE: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);
const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
const GRAY: Color32 = Color32::from_rgb(0x9a, 0xa5, 0xce);

pub fn folder(open: bool) -> (&'static str, Color32) {
    (if open { ph::FOLDER_OPEN } else { ph::FOLDER }, YELLOW)
}

/// Icon and colour for a file name, by extension (and a few special names).
pub fn file(name: &str) -> (&'static str, Color32) {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return (ph::PACKAGE, ORANGE),
        "package.json" | "package-lock.json" => return (ph::PACKAGE, GREEN),
        "dockerfile" | "docker-compose.yml" | "docker-compose.yaml" => return (ph::CUBE, CYAN),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return (ph::GIT_BRANCH, ORANGE),
        "license" | "license.md" | "license.txt" => return (ph::CERTIFICATE, YELLOW),
        "makefile" | "justfile" => return (ph::WRENCH, GRAY),
        ".env" | ".env.local" => return (ph::KEY, RED),
        _ => {}
    }
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" => (ph::FILE_RS, ORANGE),
        "ts" => (ph::FILE_TS, BLUE),
        "tsx" => (ph::FILE_TSX, BLUE),
        "js" | "mjs" | "cjs" => (ph::FILE_JS, YELLOW),
        "jsx" => (ph::FILE_JSX, CYAN),
        "vue" => (ph::FILE_VUE, GREEN),
        "py" | "pyw" => (ph::FILE_PY, BLUE),
        "c" | "h" => (ph::FILE_C, BLUE),
        "cpp" | "cc" | "cxx" | "hpp" => (ph::FILE_CPP, BLUE),
        "cs" => (ph::FILE_C_SHARP, PURPLE),
        "go" | "java" | "kt" | "swift" | "rb" | "php" | "lua" | "zig" | "gd" | "sh" | "ps1" | "bat" | "cmd" => {
            (ph::FILE_CODE, GREEN)
        }
        "html" | "htm" => (ph::FILE_HTML, ORANGE),
        "css" | "scss" | "sass" | "less" => (ph::FILE_CSS, BLUE),
        "md" | "mdx" => (ph::FILE_MD, CYAN),
        "txt" | "log" => (ph::FILE_TXT, GRAY),
        "json" | "jsonc" => (ph::BRACKETS_CURLY, YELLOW),
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => (ph::FILE_INI, GRAY),
        "xml" | "svg" if ext == "svg" => (ph::FILE_SVG, PURPLE),
        "xml" | "plist" => (ph::CODE, ORANGE),
        "csv" | "tsv" => (ph::FILE_CSV, GREEN),
        "xls" | "xlsx" => (ph::FILE_XLS, GREEN),
        "doc" | "docx" | "rtf" | "odt" => (ph::FILE_DOC, BLUE),
        "ppt" | "pptx" => (ph::FILE_PPT, ORANGE),
        "pdf" => (ph::FILE_PDF, RED),
        "png" => (ph::FILE_PNG, PURPLE),
        "jpg" | "jpeg" => (ph::FILE_JPG, PURPLE),
        "gif" | "webp" | "bmp" | "ico" | "tiff" | "psd" => (ph::FILE_IMAGE, PURPLE),
        "mp4" | "mkv" | "mov" | "avi" | "webm" => (ph::FILE_VIDEO, RED),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => (ph::FILE_AUDIO, TEAL),
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "zst" | "bz2" => (ph::FILE_ZIP, YELLOW),
        "sql" | "db" | "sqlite" | "sqlite3" => (ph::FILE_SQL, TEAL),
        "exe" | "msi" | "app" | "dmg" | "deb" | "rpm" | "appimage" => (ph::APP_WINDOW, GREEN),
        "dll" | "so" | "dylib" | "lib" | "a" => (ph::CUBE, GRAY),
        "lock" => (ph::FILE_LOCK, GRAY),
        "blend" | "fbx" | "obj" | "gltf" | "glb" => (ph::CUBE, ORANGE),
        "ttf" | "otf" | "woff" | "woff2" => (ph::TEXT_AA, GRAY),
        "iso" | "img" => (ph::DISC, GRAY),
        "pem" | "crt" | "cer" | "key" | "pfx" => (ph::KEY, RED),
        _ => (ph::FILE, GRAY),
    }
}

/// Darken accent colours for light themes so they keep contrast.
pub fn tint(color: Color32, dark_mode: bool) -> Color32 {
    if dark_mode {
        color
    } else {
        let [r, g, b, _] = color.to_array();
        Color32::from_rgb((r as f32 * 0.65) as u8, (g as f32 * 0.65) as u8, (b as f32 * 0.65) as u8)
    }
}
