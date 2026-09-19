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
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rust-explorer")
            .join("config.json")
    }

    pub fn load() -> Self {
        fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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
