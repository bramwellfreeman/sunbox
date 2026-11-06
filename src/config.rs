//! Paths + config file (`~/.config/sunbox/config.toml`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Gmail scopes: read + send + label/archive. Matches the Mac app's grant.
pub const SCOPES: &str = "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send";

/// The "Crayon Box" Desktop OAuth client (GCP project client-portal-494601) — the app's old name.
pub const DEFAULT_CLIENT_ID: &str =
    "630462177368-ms7b3m9ppd13hbkfkub4r2j7r1jism69.apps.googleusercontent.com";

/// What AI agents (MCP + routines) are allowed to do. Humans at the TUI/CLI
/// are never restricted.
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("read", "list, search and read mail"),
    ("draft", "create Gmail drafts"),
    ("send", "send mail without a human in the loop"),
    ("modify", "archive, star, label, mark read"),
    ("manage", "create/edit contacts, templates and routines"),
    ("exec", "routines may run shell commands"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub google: Google,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub mcp: Mcp,
    #[serde(default)]
    pub claude: Claude,
    #[serde(default)]
    pub tasks: Tasks,
}

/// Where "create a task" buttons send to-dos.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tasks {
    /// TickTick's add-tasks-by-email address (shown in TickTick's settings).
    #[serde(default)]
    pub email: String,
}

/// The built-in assistant (briefings, drafts, client profiles, templates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claude {
    /// "auto" (API when ANTHROPIC_API_KEY is set, else the Claude Code CLI), "cli" or "api".
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// low · medium · high · xhigh · max
    #[serde(default = "default_effort")]
    pub effort: String,
    /// The Claude Code binary for the "cli" backend.
    #[serde(default = "default_command")]
    pub command: String,
    /// A few lines about you and your work, so Claude knows what matters.
    #[serde(default)]
    pub about: String,
    /// Fix spelling as you type in the composer.
    #[serde(default = "default_true")]
    pub autocorrect: bool,
    /// Model for autocorrect ("" = `model`); a smaller one is quicker.
    #[serde(default)]
    pub autocorrect_model: String,
    #[serde(default = "default_autocorrect_effort")]
    pub autocorrect_effort: String,
    /// API keys pasted in Settings (this file is chmod 600). Env vars work too.
    #[serde(default)]
    pub anthropic_key: String,
    #[serde(default)]
    pub openai_key: String,
    #[serde(default)]
    pub gemini_key: String,
    /// Models for the other assistants ("" = that tool's default).
    #[serde(default)]
    pub openai_model: String,
    #[serde(default)]
    pub gemini_model: String,
}

fn default_true() -> bool {
    true
}
fn default_autocorrect_effort() -> String {
    "low".into()
}

fn default_backend() -> String {
    "auto".into()
}
fn default_model() -> String {
    "claude-opus-5-5".into()
}
fn default_effort() -> String {
    "medium".into()
}
fn default_command() -> String {
    "claude".into()
}

impl Default for Claude {
    fn default() -> Self {
        Self { backend: default_backend(), model: default_model(), effort: default_effort(), command: default_command(), about: String::new(),
            autocorrect: true, autocorrect_model: String::new(), autocorrect_effort: default_autocorrect_effort(),
            anthropic_key: String::new(), openai_key: String::new(), gemini_key: String::new(), openai_model: String::new(), gemini_model: String::new() }
    }
}

/// Remote (HTTP) MCP access for hosted agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mcp {
    #[serde(default)]
    pub http_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Google {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
}

impl Default for Google {
    fn default() -> Self {
        Self { client_id: DEFAULT_CLIENT_ID.into(), client_secret: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub allow: Vec<String>,
}

impl Default for Agent {
    fn default() -> Self {
        // Safe default: agents can read, draft, organise and manage profiles,
        // but sending and shell execution must be opted into.
        Self { allow: ["read", "draft", "modify", "manage"].map(String::from).to_vec() }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sunbox")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("sunbox")
}

/// One-time move from the pre-rename "Crayon Box" locations, so sign-ins and history carry over.
pub fn migrate_from_crayon() {
    let moves = [
        (dirs::config_dir().map(|d| d.join("crayon-box")), config_dir()),
        (dirs::data_dir().map(|d| d.join("crayon-box")), data_dir()),
    ];
    for (old, new) in moves {
        if let Some(old) = old.filter(|o| o.exists() && !new.exists()) {
            let _ = std::fs::rename(old, &new);
        }
    }
    let dir = data_dir();
    if !dir.join("sunbox.db").exists() {
        for ext in ["", "-wal", "-shm"] {
            let old = dir.join(format!("crayon.db{ext}"));
            if old.exists() {
                let _ = std::fs::rename(old, dir.join(format!("sunbox.db{ext}")));
            }
        }
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let mut cfg: Config = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config { google: Google::default(), agent: Agent::default(), mcp: Mcp::default(), claude: Claude::default(), tasks: Tasks::default() }
        };
        if let Ok(id) = std::env::var("SUNBOX_GOOGLE_CLIENT_ID") {
            cfg.google.client_id = id;
        }
        if let Ok(secret) = std::env::var("SUNBOX_GOOGLE_CLIENT_SECRET") {
            cfg.google.client_secret = secret;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let path = config_path();
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        restrict(&path);
        Ok(())
    }

    pub fn allows(&self, perm: &str) -> bool {
        self.agent.allow.iter().any(|p| p == perm)
    }

    /// Error out unless agents hold `perm`.
    pub fn require(&self, perm: &str) -> Result<()> {
        if !self.allows(perm) {
            bail!(
                "permission denied: agents lack '{perm}'. The user can grant it with `sunbox agent allow {perm}`"
            );
        }
        Ok(())
    }
}

/// chmod 600 — the config holds the OAuth secret, the DB holds tokens.
pub fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
