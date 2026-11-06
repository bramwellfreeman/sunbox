//! A small editor for templates and contact profiles: labelled one-line fields plus
//! multi-line boxes, Ctrl+E to edit the focused field in $EDITOR.

use super::theme::Theme;
use super::{App, EditorJob, Overlay};
use crate::edit_text;
use crate::engine::Actor;
use crate::models::{Contact, Template};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub multiline: bool,
}

pub enum FormKind {
    /// `orig` is the stored name when editing (renaming deletes the old one).
    Template { orig: Option<String> },
    /// `base` keeps what the form doesn't show (custom fields, timestamps).
    Contact { orig: Option<String>, base: Contact },
}

pub struct Form {
    pub title: String,
    pub kind: FormKind,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub dirty: bool,
    armed_discard: bool,
}

fn f(label: &'static str, value: &str, multiline: bool) -> Field {
    Field { label, value: value.to_string(), multiline }
}

impl Form {
    /// `t` prefills (an existing template, or one Claude wrote); `orig` is its stored name.
    pub fn template(t: Option<Template>, orig: Option<String>) -> Form {
        let fresh = t.is_some() && orig.is_none();
        let t = t.unwrap_or_default();
        let title = match (&orig, fresh) {
            (Some(n), _) => format!(" Template “{n}” "),
            (None, true) => " New template from Claude — check it ".to_string(),
            (None, false) => " New template ".to_string(),
        };
        Form {
            title,
            kind: FormKind::Template { orig },
            fields: vec![
                f("Name", &t.name, false),
                f("When to use", &t.description, false),
                f("Subject", &t.subject, false),
                f("Body", &t.body, true),
            ],
            focus: 0,
            // Claude's version isn't saved yet, so leaving should ask first.
            dirty: fresh,
            armed_discard: false,
        }
    }

    pub fn contact(c: Option<Contact>) -> Form {
        let orig = c.as_ref().map(|c| c.email.clone());
        let c = c.unwrap_or_default();
        let custom = c.fields.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        Form {
            title: match &orig {
                Some(e) => format!(" {} ", if c.name.is_empty() { e.clone() } else { format!("{} · {e}", c.name) }),
                None => " New contact ".into(),
            },
            fields: vec![
                f("Email", &c.email, false),
                f("Name", &c.name, false),
                f("Company", &c.company, false),
                f("Role", &c.role, false),
                f("Relationship", &c.relationship, false),
                f("Tone", &c.tone, false),
                f("Tags", &c.tags.join(", "), false),
                f("Fields", &custom, false),
                f("Notes", &c.notes, true),
                f("Learned by Claude", &c.learned, true),
            ],
            kind: FormKind::Contact { orig, base: c },
            focus: 0,
            dirty: false,
            armed_discard: false,
        }
    }

    fn val(&self, label: &str) -> String {
        self.fields.iter().find(|f| f.label == label).map(|f| f.value.trim().to_string()).unwrap_or_default()
    }

    pub fn edit_focused_in_editor(&mut self) -> Result<()> {
        let field = &mut self.fields[self.focus];
        let edited = edit_text(&field.value)?;
        let edited = if field.multiline { edited.trim_end().to_string() } else { edited.lines().next().unwrap_or("").to_string() };
        if edited != field.value {
            field.value = edited;
            self.dirty = true;
        }
        Ok(())
    }

    pub fn draw(&self, frame: &mut Frame, th: &Theme) {
        let area = frame.area();
        let w = area.width.saturating_sub(4).min(110);
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + 1, width: w, height: area.height.saturating_sub(2) };
        frame.render_widget(Clear, r);
        let block = th.popup(&self.title).title_bottom(
            Line::from(" Ctrl+S save · Tab next field · Ctrl+E edit field in $EDITOR · Esc close ").fg(th.muted),
        );
        let inner = block.inner(r);
        frame.render_widget(block, r);

        let singles: Vec<usize> = (0..self.fields.len()).filter(|i| !self.fields[*i].multiline).collect();
        let multis: Vec<usize> = (0..self.fields.len()).filter(|i| self.fields[*i].multiline).collect();
        let mut constraints = vec![Constraint::Length(singles.len() as u16), Constraint::Length(1)];
        constraints.extend(multis.iter().map(|_| Constraint::Fill(1)));
        let areas = Layout::vertical(constraints).split(inner);
        let cursor = Span::styled("▏", Style::new().fg(th.sun));
        let label_style = |focused: bool| if focused { Style::new().fg(th.sun).bold() } else { Style::new().fg(th.muted) };

        let lines: Vec<Line> = singles
            .iter()
            .map(|&i| {
                let fld = &self.fields[i];
                let focused = self.focus == i;
                let mut l = vec![Span::styled(format!("{:>13}  ", fld.label), label_style(focused)), Span::raw(fld.value.clone())];
                if focused {
                    l.push(cursor.clone());
                }
                Line::from(l)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), areas[0]);

        for (n, &i) in multis.iter().enumerate() {
            let fld = &self.fields[i];
            let focused = self.focus == i;
            let b = Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(if focused { th.sun } else { th.rule }))
                .title(Span::styled(format!(" {} ", fld.label), label_style(focused)))
                .padding(Padding::horizontal(1));
            let mut body: Vec<Line> = fld.value.split('\n').map(|l| Line::raw(l.to_string())).collect();
            if focused {
                if let Some(last) = body.last_mut() {
                    last.spans.push(cursor.clone());
                }
            }
            let box_area = areas[2 + n];
            let para = Paragraph::new(body).wrap(Wrap { trim: false }).block(b);
            let total = para.line_count(box_area.width.saturating_sub(2)) as u16;
            let scroll = if focused { total.saturating_sub(box_area.height) } else { 0 };
            frame.render_widget(para.scroll((scroll, 0)), box_area);
        }
    }
}

impl App {
    pub(super) fn form_key(&mut self, mut form: Form, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code != KeyCode::Esc {
            form.armed_discard = false;
        }
        let n = form.fields.len();
        let multiline = form.fields[form.focus].multiline;
        match k.code {
            KeyCode::Char('s') if ctrl => {
                match self.save_form(&form) {
                    Ok(msg) => {
                        self.status = format!("✓ {msg}");
                        self.refresh_library();
                        return;
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            KeyCode::Char('e') if ctrl => {
                self.job = Some(EditorJob::Field(form));
                return;
            }
            KeyCode::Esc => {
                if form.dirty && !form.armed_discard {
                    form.armed_discard = true;
                    self.status = "Unsaved changes — Esc again to discard, Ctrl+S to save".into();
                } else {
                    self.status.clear();
                    return;
                }
            }
            KeyCode::Tab => form.focus = (form.focus + 1) % n,
            KeyCode::BackTab => form.focus = (form.focus + n - 1) % n,
            KeyCode::Down if !multiline => form.focus = (form.focus + 1) % n,
            KeyCode::Up if !multiline => form.focus = (form.focus + n - 1) % n,
            KeyCode::Enter => {
                if multiline {
                    form.fields[form.focus].value.push('\n');
                    form.dirty = true;
                } else {
                    form.focus = (form.focus + 1) % n;
                }
            }
            KeyCode::Backspace => {
                let v = &mut form.fields[form.focus].value;
                if ctrl {
                    while v.ends_with(char::is_whitespace) {
                        v.pop();
                    }
                    while v.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        v.pop();
                    }
                } else {
                    v.pop();
                }
                form.dirty = true;
            }
            KeyCode::Char(c) if !ctrl => {
                form.fields[form.focus].value.push(c);
                form.dirty = true;
            }
            _ => {}
        }
        self.overlay = Overlay::Form(form);
    }

    fn save_form(&mut self, form: &Form) -> Result<String> {
        let me = Actor::User;
        match &form.kind {
            FormKind::Template { orig } => {
                let t = Template {
                    name: form.val("Name"),
                    description: form.val("When to use"),
                    subject: form.val("Subject"),
                    body: form.fields.iter().find(|f| f.label == "Body").map(|f| f.value.trim_end().to_string()).unwrap_or_default(),
                    updated_at: String::new(),
                };
                let saved = self.e.save_template(&me, t)?;
                if let Some(old) = orig.as_ref().filter(|o| **o != saved.name) {
                    self.e.delete_template(&me, old)?;
                }
                Ok(format!("Saved template “{}”", saved.name))
            }
            FormKind::Contact { orig, base } => {
                let mut c = base.clone();
                c.email = form.val("Email");
                c.name = form.val("Name");
                c.company = form.val("Company");
                c.role = form.val("Role");
                c.relationship = form.val("Relationship");
                c.tone = form.val("Tone");
                c.tags = form.val("Tags").split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
                c.fields = form
                    .val("Fields")
                    .split(';')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .collect();
                c.notes = form.fields.iter().find(|f| f.label == "Notes").map(|f| f.value.trim_end().to_string()).unwrap_or_default();
                c.learned =
                    form.fields.iter().find(|f| f.label == "Learned by Claude").map(|f| f.value.trim_end().to_string()).unwrap_or_default();
                let saved = self.e.replace_contact(&me, c)?;
                if let Some(old) = orig.as_ref().filter(|o| !o.eq_ignore_ascii_case(&saved.email)) {
                    self.e.delete_contact(&me, old)?;
                }
                Ok(format!("Saved {}", saved.email))
            }
        }
    }
}
