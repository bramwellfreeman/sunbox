//! ⚙ Settings: a small menu (top right) leading to Accounts and the AI assistant picker.

use super::{App, EditorJob, InputKind, Msg, Overlay};
use crate::ai::{self, Claude, PROVIDERS};
use crate::config::{self, Config};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use serde_json::json;

pub const MENU: [(&str, &str); 2] = [
    ("Accounts", "switch, add, pick the default, sign out, show inboxes together"),
    ("AI assistant", "pick Claude, ChatGPT, Gemini or Grok and sign in"),
];

/// The AI assistant page.
pub struct Assistant {
    pub sel: usize,
    /// Per provider: None = still checking, Some(Ok(signed in?)), Some(Err(why not usable)).
    pub ready: Vec<Option<Result<bool, String>>>,
}

/// Change the saved assistant settings.
fn save(f: impl FnOnce(&mut config::Claude)) -> anyhow::Result<()> {
    let mut c = Config::load()?;
    f(&mut c.claude);
    c.save()
}

fn model_of(c: &config::Claude, id: &str) -> String {
    let m = match id {
        "codex" | "openai" => &c.openai_model,
        "gemini" | "gemini-api" => &c.gemini_model,
        "grok" | "xai" => &c.grok_model,
        _ => &c.model,
    };
    if m.is_empty() { "default model".into() } else { m.clone() }
}

impl App {
    pub(super) fn open_settings(&mut self) {
        self.overlay = Overlay::Settings { sel: 0 };
    }

    pub(super) fn settings_key(&mut self, mut sel: usize, k: KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(MENU.len() - 1),
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            KeyCode::Char('1') => return self.open_accounts(),
            KeyCode::Char('2') => return self.open_assistant(),
            KeyCode::Enter => return if sel == 0 { self.open_accounts() } else { self.open_assistant() },
            _ => {}
        }
        self.overlay = Overlay::Settings { sel };
    }

    pub(super) fn open_assistant(&mut self) {
        let current = Claude::new(&self.e.assistant_cfg()).backend();
        let sel = PROVIDERS.iter().position(|p| p.id == current).unwrap_or(0);
        self.overlay = Overlay::Assistant(Assistant { sel, ready: vec![None; PROVIDERS.len()] });
        self.check_assistants();
    }

    /// Ask every provider whether it's signed in (CLIs take a moment, so off the UI thread).
    pub(super) fn check_assistants(&self) {
        let cfg = self.e.assistant_cfg();
        self.spawn(move |_| Msg::AiReady(ai::ready_all(&cfg)));
    }

    pub(super) fn on_ai_ready(&mut self, r: Vec<Result<bool, String>>) {
        if let Overlay::Assistant(a) = &mut self.overlay {
            a.ready = r.into_iter().map(Some).collect();
        }
    }

    pub(super) fn assistant_key(&mut self, mut a: Assistant, k: KeyEvent) {
        let p = &PROVIDERS[a.sel];
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Down | KeyCode::Char('j') => a.sel = (a.sel + 1).min(PROVIDERS.len() - 1),
            KeyCode::Up | KeyCode::Char('k') => a.sel = a.sel.saturating_sub(1),
            KeyCode::Enter => match a.ready.get(a.sel).cloned().flatten() {
                Some(Ok(true)) => {
                    match save(|c| c.backend = p.id.to_string()) {
                        Ok(()) => self.status = format!("✓ Sunbox now uses {}", p.name),
                        Err(e) => self.status = format!("Error: {e:#}"),
                    }
                    return;
                }
                Some(Err(why)) => self.status = why,
                // Not signed in yet (or still checking): sign in, then it's chosen.
                _ => return self.sign_in(p.id),
            },
            KeyCode::Char('s') => return self.sign_in(p.id),
            KeyCode::Char('m') => {
                self.overlay = Overlay::Input { kind: InputKind::AiModel { provider: p.id.to_string() }, buf: String::new() };
                return;
            }
            KeyCode::Char('u') => {
                let about = self.e.assistant_cfg().about;
                self.overlay = Overlay::Input { kind: InputKind::AiAbout, buf: about };
                return;
            }
            KeyCode::Char('t') => {
                let mut cfg = self.e.assistant_cfg();
                cfg.backend = p.id.to_string();
                let name = p.name;
                self.status = format!("Asking {name} a test question…");
                self.spawn(move |_| {
                    let t = std::time::Instant::now();
                    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"], "additionalProperties": false});
                    let r = Claude::new(&cfg).json("You are a connectivity check.", "Reply with {\"ok\": true}.", &schema);
                    let status = r.map(|_| format!("{name} answered in {:.1} s", t.elapsed().as_secs_f32()));
                    Msg::Done { status, reload: false }
                });
            }
            _ => {}
        }
        self.overlay = Overlay::Assistant(a);
    }

    /// Paste a key, or hand the terminal to the provider's own sign-in.
    fn sign_in(&mut self, id: &str) {
        if ai::key_field(id).is_some() {
            self.overlay = Overlay::Input { kind: InputKind::AiKey { provider: id.to_string() }, buf: String::new() };
        } else {
            self.job = Some(EditorJob::SignIn(id.to_string()));
        }
    }

    /// Runs in the real terminal (the TUI is suspended).
    pub(super) fn run_sign_in(&mut self, id: &str) {
        let p = ai::provider(id);
        if let Some((prog, args)) = ai::sign_in_command(id) {
            println!("\n  Signing in to {} …\n", p.name);
            if id == "gemini" {
                println!("  Pick “Sign in with Google”, finish in the browser, then type /quit to come back.\n");
            }
            let ok = std::process::Command::new(prog).args(args).status().map(|s| s.success()).unwrap_or(false);
            if !ok && id != "gemini" {
                println!("\n  That didn't finish. Press Enter to go back to Sunbox.");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
        }
        match save(|c| c.backend = id.to_string()) {
            Ok(()) => self.status = format!("✓ Sunbox now uses {} — t in Settings tests it", p.name),
            Err(e) => self.status = format!("Error: {e:#}"),
        }
        self.open_assistant();
    }

    /// Text typed into one of the assistant prompts.
    pub(super) fn submit_assistant_input(&mut self, kind: InputKind, buf: String) {
        let r = match &kind {
            InputKind::AiKey { provider } => {
                let id = provider.clone();
                save(|c| {
                    match ai::key_field(&id) {
                        Some("anthropic_key") => c.anthropic_key = buf.clone(),
                        Some("openai_key") => c.openai_key = buf.clone(),
                        Some("xai_key") => c.xai_key = buf.clone(),
                        _ => c.gemini_key = buf.clone(),
                    }
                    c.backend = id.clone();
                })
                .map(|_| format!("✓ Key saved — Sunbox now uses {}", ai::provider(&id).name))
            }
            InputKind::AiModel { provider } => {
                let id = provider.clone();
                save(|c| match id.as_str() {
                    "codex" | "openai" => c.openai_model = buf.clone(),
                    "gemini" | "gemini-api" => c.gemini_model = buf.clone(),
                    "grok" | "xai" => c.grok_model = buf.clone(),
                    _ => c.model = buf.clone(),
                })
                .map(|_| format!("✓ Model set to {}", if buf.is_empty() { "the default" } else { &buf }))
            }
            InputKind::AiAbout => save(|c| c.about = buf.clone()).map(|_| "✓ Saved what the assistant knows about you".to_string()),
            _ => return,
        };
        self.status = match r {
            Ok(s) => s,
            Err(e) => format!("Error: {e:#}"),
        };
        self.open_assistant();
    }

    pub(super) fn draw_settings(&self, f: &mut Frame, sel: usize) {
        let lines: Vec<Line> = MENU
            .iter()
            .enumerate()
            .map(|(i, (name, what))| {
                let style = if i == sel { Style::new().fg(self.th.on_chip).bg(self.th.sun).bold() } else { Style::new().fg(self.th.bright).bold() };
                Line::from(vec![
                    Span::styled(format!(" {} ", i + 1), Style::new().fg(self.th.sun)),
                    Span::styled(format!(" {name:<13} "), style),
                    Span::styled(format!("  {what}"), Style::new().fg(self.th.muted)),
                ])
            })
            .collect();
        let r = top_right(f.area(), 78, lines.len() as u16 + 2);
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(lines).block(self.th.popup(" ⚙ Settings — ↵ open · esc close ")), r);
    }

    pub(super) fn draw_assistant(&self, f: &mut Frame, a: &Assistant) {
        let cfg = self.e.assistant_cfg();
        let current = Claude::new(&cfg).backend();
        let th = &self.th;
        let mut lines = vec![
            Line::from(Span::styled(" Used for briefings, drafted replies, action buttons and autocorrect.", Style::new().fg(th.muted))),
            Line::raw(""),
        ];
        for (i, p) in PROVIDERS.iter().enumerate() {
            let picked = i == a.sel;
            let dot = if p.id == current { Span::styled(" ● ", Style::new().fg(th.sun).bold()) } else { Span::styled(" ○ ", Style::new().fg(th.muted)) };
            let name = Span::styled(
                format!(" {:<22} ", p.name),
                if picked { Style::new().fg(th.on_chip).bg(th.sun).bold() } else { Style::new().fg(th.bright) },
            );
            let state = match a.ready.get(i).cloned().flatten() {
                None => Span::styled(format!("{:<16}", "checking…"), Style::new().fg(th.muted)),
                Some(Ok(true)) if ai::key_field(p.id).is_some() => Span::styled(format!("{:<16}", "✓ key saved"), Style::new().fg(th.accent)),
                Some(Ok(true)) => Span::styled(format!("{:<16}", "✓ signed in"), Style::new().fg(th.accent)),
                Some(Ok(false)) if ai::key_field(p.id).is_some() => Span::styled(format!("{:<16}", "no key yet"), Style::new().fg(th.muted)),
                Some(Ok(false)) => Span::styled(format!("{:<16}", "not signed in"), Style::new().fg(th.muted)),
                Some(Err(_)) => Span::styled(format!("{:<16}", "not installed"), Style::new().fg(th.muted)),
            };
            lines.push(Line::from(vec![dot, name, Span::raw("  "), state, Span::styled(model_of(&cfg, p.id), Style::new().fg(th.muted))]));
        }
        let p = &PROVIDERS[a.sel];
        lines.push(Line::raw(""));
        let note = match a.ready.get(a.sel).cloned().flatten() {
            Some(Err(why)) => why,
            _ => p.how.to_string(),
        };
        lines.push(Line::from(Span::styled(format!(" {}: {note}", p.name), Style::new().fg(th.fg))));
        if !cfg.about.is_empty() {
            let about: String = cfg.about.chars().take(70).collect();
            lines.push(Line::from(Span::styled(format!(" About you: {about}"), Style::new().fg(th.muted))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(" ↵ use (signs in first if needed) · s sign in / new key · m model · t test · u about you · esc", Style::new().fg(th.muted))));
        let r = top_right(f.area(), 96, lines.len() as u16 + 2);
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(lines).block(th.popup(" ⚙ AI assistant ")), r);
    }
}

/// Popups that open from the gear sit under it, at the top right.
fn top_right(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.right().saturating_sub(w + 1), y: area.y + 1, width: w, height: h }
}
