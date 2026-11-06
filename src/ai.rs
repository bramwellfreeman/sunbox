//! Calls the user's chosen AI assistant for structured JSON. Claude through the Claude Code CLI
//! (your existing plan, no key needed) or the Messages API; ChatGPT through the Codex CLI or the
//! OpenAI API; Gemini through the Gemini CLI or the Gemini API.

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

    fn api_key(&self) -> Option<String> {
        key(&self.cfg.anthropic_key, "ANTHROPIC_API_KEY")
    }

    /// The provider id in use ("auto" resolves to the Claude API with a key, else Claude Code).
    pub fn backend(&self) -> &'static str {
        match PROVIDERS.iter().find(|p| p.id == self.cfg.backend) {
            Some(p) => p.id,
            None if self.api_key().is_some() => "api",
            None => "cli",
        }
    }

    /// The model this provider uses ("" = the tool's own default).
    pub fn model(&self) -> &str {
        match self.backend() {
            "codex" | "openai" => &self.cfg.openai_model,
            "gemini" | "gemini-api" => &self.cfg.gemini_model,
            _ => &self.cfg.model,
        }
    }

    pub fn describe(&self) -> String {
        let p = provider(self.backend());
        let model = if self.model().is_empty() { "default model" } else { self.model() };
        match self.backend() {
            "cli" | "api" => format!("{} · {model} · effort {}", p.name, self.cfg.effort),
            _ => format!("{} · {model}", p.name),
        }
    }

    /// One request; the reply must match `schema` (objects need `additionalProperties: false`).
    pub fn json(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        match self.backend() {
            "api" => self.via_api(system, prompt, schema),
            "codex" => self.via_codex(system, prompt, schema),
            "openai" => self.via_openai(system, prompt, schema),
            "gemini" => self.via_gemini_cli(system, prompt, schema),
            "gemini-api" => self.via_gemini_api(system, prompt, schema),
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
        let key = self.api_key().ok_or_else(|| anyhow!("the Claude API needs a key — add one in Settings → AI assistant"))?;
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

    fn via_codex(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let dir = std::env::temp_dir().join(format!("sunbox-codex-{}-{}", std::process::id(), rand_tag()));
        std::fs::create_dir_all(&dir)?;
        let (schema_path, out_path) = (dir.join("schema.json"), dir.join("out.json"));
        std::fs::write(&schema_path, schema.to_string())?;
        let mut cmd = Command::new("codex");
        cmd.args(["exec", "--skip-git-repo-check", "--ephemeral", "--sandbox", "read-only", "--color", "never"])
            .arg("--output-schema").arg(&schema_path)
            .arg("-o").arg(&out_path);
        if !self.cfg.openai_model.is_empty() {
            cmd.args(["-m", &self.cfg.openai_model]);
        }
        let input = format!("<instructions>\n{system}\nAnswer only with the JSON object; don't run commands.\n</instructions>\n\n{prompt}");
        let out = run_with_stdin(cmd.arg("-").current_dir(&dir), &input, "codex")?;
        let text = std::fs::read_to_string(&out_path).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        if text.trim().is_empty() {
            bail!("ChatGPT (Codex) failed ({}): {}", out.status, tail(&out.stderr));
        }
        parse_json(&text, "ChatGPT")
    }

    fn via_openai(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let key = key(&self.cfg.openai_key, "OPENAI_API_KEY").ok_or_else(|| anyhow!("the OpenAI API needs a key — add one in Settings → AI assistant"))?;
        let model = if self.cfg.openai_model.is_empty() { DEFAULT_OPENAI_MODEL } else { &self.cfg.openai_model };
        let body = json!({
            "model": model,
            "instructions": system,
            "input": prompt,
            "text": {"format": {"type": "json_schema", "name": "reply", "schema": schema, "strict": true}},
        });
        let v = post_json(ureq::post("https://api.openai.com/v1/responses").set("Authorization", &format!("Bearer {key}")), body, "OpenAI API")?;
        let text: String = v["output"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|o| o["content"].as_array().cloned().unwrap_or_default())
            .filter(|c| c["type"] == "output_text")
            .filter_map(|c| c["text"].as_str().map(String::from))
            .collect();
        if text.is_empty() {
            bail!("OpenAI returned no text (status {})", v["status"]);
        }
        parse_json(&text, "OpenAI")
    }

    fn via_gemini_cli(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let mut cmd = Command::new("gemini");
        cmd.args(["-o", "json", "-p", ""]);
        if !self.cfg.gemini_model.is_empty() {
            cmd.args(["-m", &self.cfg.gemini_model]);
        }
        let input = format!(
            "<instructions>\n{system}\nReply with only a JSON object matching this JSON Schema, no prose, no code fences:\n{schema}\n</instructions>\n\n{prompt}"
        );
        let out = run_with_stdin(cmd.current_dir(std::env::temp_dir()), &input, "gemini")?;
        let v: Value = serde_json::from_slice(&out.stdout)
            .map_err(|_| anyhow!("Gemini CLI failed ({}): {}", out.status, tail(&out.stderr)))?;
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            bail!("Gemini: {}", e["message"].as_str().unwrap_or(&e.to_string()));
        }
        parse_json(v["response"].as_str().unwrap_or_default(), "Gemini")
    }

    fn via_gemini_api(&self, system: &str, prompt: &str, schema: &Value) -> Result<Value> {
        let key = key(&self.cfg.gemini_key, "GEMINI_API_KEY").ok_or_else(|| anyhow!("the Gemini API needs a key — add one in Settings → AI assistant"))?;
        let model = if self.cfg.gemini_model.is_empty() { DEFAULT_GEMINI_MODEL } else { &self.cfg.gemini_model };
        let body = json!({
            "systemInstruction": {"parts": [{"text": system}]},
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
            "generationConfig": {"responseMimeType": "application/json", "responseJsonSchema": schema},
        });
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
        let v = post_json(ureq::post(&url).set("x-goog-api-key", &key), body, "Gemini API")?;
        let text: String = v["candidates"][0]["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["text"].as_str())
            .collect();
        if text.is_empty() {
            bail!("Gemini returned no text ({})", v["candidates"][0]["finishReason"]);
        }
        parse_json(&text, "Gemini")
    }
}

const DEFAULT_OPENAI_MODEL: &str = "gpt-5";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-pro";

/// An assistant the user can pick in Settings.
pub struct Provider {
    /// Stored as `[claude] backend`.
    pub id: &'static str,
    pub name: &'static str,
    /// One line on how it signs in / what it costs.
    pub how: &'static str,
}

pub const PROVIDERS: &[Provider] = &[
    Provider { id: "cli", name: "Claude · Claude Code", how: "your Claude plan, signs in with your Claude account" },
    Provider { id: "api", name: "Claude · API key", how: "pay as you go with an Anthropic API key" },
    Provider { id: "codex", name: "ChatGPT · Codex CLI", how: "your ChatGPT plan, signs in with your OpenAI account" },
    Provider { id: "openai", name: "OpenAI · API key", how: "pay as you go with an OpenAI API key" },
    Provider { id: "gemini", name: "Gemini · Gemini CLI", how: "signs in with your Google account" },
    Provider { id: "gemini-api", name: "Gemini · API key", how: "a Google AI Studio API key" },
];

pub fn provider(id: &str) -> &'static Provider {
    PROVIDERS.iter().find(|p| p.id == id).unwrap_or(&PROVIDERS[0])
}

/// Which API-key setting a provider uses, if it's a key-based one.
pub fn key_field(id: &str) -> Option<&'static str> {
    match id {
        "api" => Some("anthropic_key"),
        "openai" => Some("openai_key"),
        "gemini-api" => Some("gemini_key"),
        _ => None,
    }
}

/// The interactive sign-in for a CLI provider, run in the real terminal.
pub fn sign_in_command(id: &str) -> Option<(&'static str, &'static [&'static str])> {
    match id {
        "cli" => Some(("claude", &["auth", "login"])),
        "codex" => Some(("codex", &["login"])),
        // The Gemini CLI signs in on its first interactive run (pick "Sign in with Google", then /quit).
        "gemini" => Some(("gemini", &[])),
        _ => None,
    }
}

/// Is this provider ready to use? Runs the CLI's status check, so call it off the UI thread.
pub fn ready(id: &str, cfg: &config::Claude) -> Result<bool, String> {
    let quiet = |prog: &str, args: &[&str]| {
        Command::new(prog).args(args).stdin(Stdio::null()).stderr(Stdio::piped()).stdout(Stdio::piped()).output()
    };
    match id {
        "cli" => match quiet(&cfg.command, &["auth", "status"]) {
            Ok(o) => Ok(serde_json::from_slice::<Value>(&o.stdout).ok().and_then(|v| v["loggedIn"].as_bool()).unwrap_or(o.status.success())),
            Err(_) => Err("Claude Code isn't installed".into()),
        },
        "codex" => match quiet("codex", &["login", "status"]) {
            Ok(o) => Ok(o.status.success() && !String::from_utf8_lossy(&o.stdout).to_lowercase().contains("not logged in")),
            Err(_) => Err("the Codex CLI isn't installed (npm i -g @openai/codex)".into()),
        },
        // Starting the Gemini CLI takes seconds, so just look for it and its saved sign-in.
        "gemini" if !on_path("gemini") => Err("the Gemini CLI isn't installed (npm i -g @google/gemini-cli)".into()),
        "gemini" => Ok(dirs::home_dir().is_some_and(|h| h.join(".gemini/oauth_creds.json").exists())
            || key(&cfg.gemini_key, "GEMINI_API_KEY").is_some()),
        "api" => Ok(key(&cfg.anthropic_key, "ANTHROPIC_API_KEY").is_some()),
        "openai" => Ok(key(&cfg.openai_key, "OPENAI_API_KEY").is_some()),
        "gemini-api" => Ok(key(&cfg.gemini_key, "GEMINI_API_KEY").is_some()),
        _ => Ok(false),
    }
}

/// Every provider's `ready`, checked side by side.
pub fn ready_all(cfg: &config::Claude) -> Vec<Result<bool, String>> {
    std::thread::scope(|s| {
        let hs: Vec<_> = PROVIDERS.iter().map(|p| s.spawn(move || ready(p.id, cfg))).collect();
        hs.into_iter().map(|h| h.join().unwrap_or(Ok(false))).collect()
    })
}

fn on_path(prog: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

fn key(saved: &str, env: &str) -> Option<String> {
    Some(saved.trim().to_string()).filter(|k| !k.is_empty()).or_else(|| std::env::var(env).ok().filter(|k| !k.trim().is_empty()))
}

fn rand_tag() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
}

fn run_with_stdin(cmd: &mut Command, input: &str, what: &str) -> Result<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running `{what}` — is it installed?"))?;
    child.stdin.take().context("stdin")?.write_all(input.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn post_json(req: ureq::Request, body: Value, what: &str) -> Result<Value> {
    match req.set("content-type", "application/json").timeout(std::time::Duration::from_secs(600)).send_json(body) {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(code, r)) => bail!("{what} HTTP {code}: {}", r.into_string().unwrap_or_default().chars().take(400).collect::<String>()),
        Err(e) => Err(e).with_context(|| format!("calling the {what}")),
    }
}

fn tail(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    s.chars().skip(s.chars().count().saturating_sub(400)).collect()
}

/// The JSON object in a reply, tolerating ```json fences or a little prose around it.
fn parse_json(text: &str, who: &str) -> Result<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Ok(v);
    }
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if a < b => serde_json::from_str(&t[a..=b]).with_context(|| format!("{who}'s reply wasn't valid JSON")),
        _ => bail!("{who}'s reply wasn't JSON"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pulls_json_out_of_fences() {
        let v = super::parse_json("Here you go:\n```json\n{\"a\": 1}\n```", "x").unwrap();
        assert_eq!(v["a"], 1);
    }
}
