//! Calls Claude for structured JSON: the Claude Code CLI (your existing plan, no key needed)
//! or the Messages API over HTTP when ANTHROPIC_API_KEY is set.

use crate::config;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

pub struct Claude {
    cfg: config::Claude,
}

impl Claude {
    pub fn new(cfg: &config::Claude) -> Self {
        Self { cfg: cfg.clone() }
    }

    fn api_key() -> Option<String> {
        std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.trim().is_empty())
    }

    /// "cli" or "api", after resolving "auto".
    pub fn backend(&self) -> &'static str {
        match self.cfg.backend.as_str() {
            "api" => "api",
            "cli" => "cli",
            _ if Self::api_key().is_some() => "api",
            _ => "cli",
        }
    }

    pub fn describe(&self) -> String {
        match self.backend() {
            "api" => format!("Claude API · {} · effort {}", self.cfg.model, self.cfg.effort),
            _ => format!("Claude Code CLI ({}) · {} · effort {}", self.cfg.command, self.cfg.model, self.cfg.effort),
        }
    }

    /// One request; the reply must match `schema` (objects need `additionalProperties: false`).
    pub fn json(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        match self.backend() {
            "api" => self.via_api(system, prompt, schema),
            _ => self.via_cli(system, prompt, schema),
        }
    }

    fn via_cli(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let mut child = Command::new(&self.cfg.command)
            .args(["-p", "--output-format", "json", "--no-session-persistence"])
            // A pure text-in, JSON-out call: no tools, MCP servers, skills or project settings.
            .args(["--tools", "", "--strict-mcp-config", "--disable-slash-commands", "--setting-sources", ""])
            .args(["--model", &self.cfg.model, "--effort", &self.cfg.effort])
            .args(["--system-prompt", system])
            .args(["--json-schema", &schema.to_string()])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running `{}` — is Claude Code installed? (or set ANTHROPIC_API_KEY)", self.cfg.command))?;
        child.stdin.take().context("claude stdin")?.write_all(prompt.as_bytes())?;
        let out = child.wait_with_output()?;
        let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            anyhow!("Claude Code failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim().chars().take(400).collect::<String>())
        })?;
        if v["is_error"].as_bool() == Some(true) {
            bail!("Claude: {}", v["result"].as_str().unwrap_or("unknown error"));
        }
        match &v["structured_output"] {
            Value::Null => serde_json::from_str(v["result"].as_str().unwrap_or_default()).context("Claude's reply wasn't JSON"),
            s => Ok(s.clone()),
        }
    }

    fn via_api(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let key = Self::api_key().ok_or_else(|| anyhow!("backend = \"api\" needs ANTHROPIC_API_KEY"))?;
        let body = json!({
            "model": self.cfg.model,
            "max_tokens": 16000,
            "system": system,
            "messages": [{"role": "user", "content": prompt}],
            "output_config": {"effort": self.cfg.effort, "format": {"type": "json_schema", "schema": schema}},
            // If a safety classifier declines, let the API retry on a suitable model.
            "fallbacks": "default",
        });
        let resp = ureq::post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", &key)
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "server-side-fallback-2026-07-01")
            .set("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(600))
            .send_json(body);
        let v: Value = match resp {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => bail!("Claude API HTTP {code}: {}", r.into_string().unwrap_or_default()),
            Err(e) => return Err(e).context("calling the Claude API"),
        };
        match v["stop_reason"].as_str() {
            Some("refusal") => bail!("Claude declined this request"),
            Some("max_tokens") => bail!("Claude's reply was cut off (max_tokens)"),
            _ => {}
        }
        let text: String = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        serde_json::from_str(&text).context("Claude's reply wasn't JSON")
    }
}
