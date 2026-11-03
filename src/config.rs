//! Paths + config file (`~/.config/crayon-box/config.toml`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Gmail scopes: read + send + label/archive. Matches the Mac app's grant.
pub const SCOPES: &str = "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send";

/// The "Crayon Box" Desktop OAuth client (GCP project client-portal-494601).
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
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("crayon-box")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("crayon-box")
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
            Config { google: Google::default(), agent: Agent::default(), mcp: Mcp::default() }
        };
        if let Ok(id) = std::env::var("CRAYON_GOOGLE_CLIENT_ID") {
            cfg.google.client_id = id;
        }
        if let Ok(secret) = std::env::var("CRAYON_GOOGLE_CLIENT_SECRET") {
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
                "permission denied: agents lack '{perm}'. The user can grant it with `crayon agent allow {perm}`"
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
