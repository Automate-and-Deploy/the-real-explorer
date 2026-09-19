//! Persistent settings. Stored as JSON under the platform config dir
//! (`%APPDATA%\rust-explorer` on Windows, `~/Library/Application Support`
//! on macOS, `~/.config` on Linux). The API key is stored in plaintext.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug)]
pub enum Backend {
    /// Local `claude` CLI in print mode. Has tools, runs in the cwd.
    ClaudeCode,
    /// Any OpenAI-compatible `/chat/completions` endpoint. Chat only, no tools.
    OpenAiCompatible,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug)]
pub enum Theme {
    System,
    Light,
    Dark,
    Omarchy,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Config {
    pub backend: Backend,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Passed to `claude --permission-mode`.
    pub claude_permission_mode: String,
    pub theme: Theme,
    pub show_hidden: bool,
    pub chat_open: bool,
    #[serde(default = "crate::lsp::default_servers")]
    pub lsp_servers: Vec<crate::lsp::ServerDef>,
    #[serde(default = "default_true")]
    pub format_json_on_save: bool,
    #[serde(default = "default_font_size")]
    pub editor_font_size: f32,
}

fn default_font_size() -> f32 {
    13.0
}

/// Per-project overrides read from `<project>/.code/settings.json`.
///
/// Every field is optional: a key that is absent leaves the global setting
/// alone, so a project file can carry one line. Applied when the explorer
/// changes folder, and written by File > Save project settings.
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct ProjectSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_font_size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_json_on_save: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_hidden: Option<bool>,
}

impl ProjectSettings {
    pub fn path(project: &std::path::Path) -> PathBuf {
        project.join(".code").join("settings.json")
    }

    /// `None` when the project has no `.code/settings.json`.
    pub fn load(project: &std::path::Path) -> Option<Self> {
        let text = fs::read_to_string(Self::path(project)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(project: &std::path::Path, cfg: &Config) -> Result<PathBuf, String> {
        let me = Self {
            theme: Some(cfg.theme),
            editor_font_size: Some(cfg.editor_font_size),
            format_json_on_save: Some(cfg.format_json_on_save),
            show_hidden: Some(cfg.show_hidden),
        };
        let p = Self::path(project);
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut s = serde_json::to_string_pretty(&me).map_err(|e| e.to_string())?;
        s.push('\n');
        fs::write(&p, s).map_err(|e| e.to_string())?;
        Ok(p)
    }

    /// Copy the present fields onto `cfg`, leaving the rest untouched.
    pub fn apply_to(&self, cfg: &mut Config) {
        if let Some(t) = self.theme {
            cfg.theme = t;
        }
        if let Some(f) = self.editor_font_size {
            cfg.editor_font_size = f.clamp(8.0, 32.0);
        }
        if let Some(b) = self.format_json_on_save {
            cfg.format_json_on_save = b;
        }
        if let Some(b) = self.show_hidden {
            cfg.show_hidden = b;
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::ClaudeCode,
            base_url: "http://localhost:11434/v1".into(),
            api_key: String::new(),
            model: "qwen2.5-coder:32b".into(),
            claude_permission_mode: "acceptEdits".into(),
            theme: Theme::System,
            show_hidden: false,
            chat_open: true,
            lsp_servers: crate::lsp::default_servers(),
            format_json_on_save: true,
            editor_font_size: 13.0,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("the-real-explorer")
            .join("config.json")
    }

    /// Load the saved settings, with a note when the file exists but could not
    /// be used. Falling back to defaults in silence looks identical to the app
    /// forgetting the backend, the key and the server list on its own.
    pub fn load() -> (Self, Option<String>) {
        let p = Self::path();
        match fs::read_to_string(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(format!("Settings not read ({e}); using defaults: {}", p.display()))),
            Ok(text) => match serde_json::from_str(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (
                    Self::default(),
                    Some(format!("Settings not understood ({e}); using defaults without overwriting {}", p.display())),
                ),
            },
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&p, s).map_err(|e| e.to_string())
    }
}
