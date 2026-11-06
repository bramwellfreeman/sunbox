//! Templates and People: browse, edit, and have Claude write templates or learn about someone.

use super::form::Form;
use super::{App, InputKind, Msg, Overlay, View};
use crate::engine::Actor;
use crate::models::{Contact, Template};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
pub enum LibKind {
    Templates,
    People,
}

pub struct Library {
    pub kind: LibKind,
    sel: usize,
    templates: Vec<Template>,
    people: Vec<Contact>,
    armed_delete: bool,
}

impl Library {
    fn len(&self) -> usize {
        match self.kind {
            LibKind::Templates => self.templates.len(),
            LibKind::People => self.people.len(),
        }
    }

    pub fn move_sel(&mut self, d: i32) {
        let n = self.len() as i32;
        if n > 0 {
            self.sel = (self.sel as i32 + d).clamp(0, n - 1) as usize;
        }
    }
}

impl App {
    pub(super) fn open_library(&mut self, kind: LibKind) {
        self.view = View::Library(Library { kind, sel: 0, templates: vec![], people: vec![], armed_delete: false });
        self.refresh_library();
    }

    /// Re-read templates and profiles (after an edit, or when Claude learned something).
    pub(super) fn refresh_library(&mut self) {
        let templates = self.e.templates(&Actor::User);
        let people = self.e.contacts(&Actor::User, None);
        if let View::Library(lib) = &mut self.view {
            lib.templates = templates.unwrap_or_default();
            let mut people = people.unwrap_or_default();
            people.sort_by_key(|c| (if c.name.is_empty() { c.email.clone() } else { c.name.clone() }).to_lowercase());
            lib.people = people;
            lib.sel = lib.sel.min(lib.len().saturating_sub(1));
        }
    }

    pub(super) fn library_key(&mut self, k: KeyEvent) {
        let View::Library(lib) = &mut self.view else { return };
        if k.code != KeyCode::Char('x') {
            lib.armed_delete = false;
        }
        let kind = lib.kind;
        let tpl = lib.templates.get(lib.sel).cloned();
        let person = lib.people.get(lib.sel).cloned();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h') => {
                self.view = View::List;
                self.clamp();
            }
            KeyCode::Down | KeyCode::Char('j') => lib.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => lib.move_sel(-1),
            KeyCode::Tab => {
                let other = if kind == LibKind::Templates { LibKind::People } else { LibKind::Templates };
                self.open_library(other);
            }
            KeyCode::Char('n') => {
                self.overlay = Overlay::Form(match kind {
                    LibKind::Templates => Form::template(None, None),
                    LibKind::People => Form::contact(None),
                })
            }
            KeyCode::Enter | KeyCode::Char('e') => match kind {
                LibKind::Templates => {
                    if let Some(t) = tpl {
                        let name = t.name.clone();
                        self.overlay = Overlay::Form(Form::template(Some(t), Some(name)));
                    }
                }
                LibKind::People => {
                    if let Some(c) = person {
                        self.overlay = Overlay::Form(Form::contact(Some(c)));
                    }
                }
            },
            KeyCode::Char('w') if kind == LibKind::Templates => {
                self.overlay = Overlay::Input { kind: InputKind::TemplateAsk { base: None }, buf: String::new() };
            }
            KeyCode::Char('W') if kind == LibKind::Templates => {
                if let Some(t) = tpl {
                    self.overlay = Overlay::Input { kind: InputKind::TemplateAsk { base: Some(t) }, buf: String::new() };
                }
            }
            KeyCode::Char('L') if kind == LibKind::People => {
                if let Some(c) = person {
                    self.learn(c.email);
                }
            }
            KeyCode::Char('x') => {
                let target = match kind {
                    LibKind::Templates => tpl.map(|t| t.name),
                    LibKind::People => person.map(|c| c.email),
                };
                let Some(target) = target else { return };
                if !lib.armed_delete {
                    lib.armed_delete = true;
                    self.status = format!("Press x again to delete {target}");
                    return;
                }
                let res = match kind {
                    LibKind::Templates => self.e.delete_template(&Actor::User, &target),
                    LibKind::People => self.e.delete_contact(&Actor::User, &target),
                };
                self.status = match res {
                    Ok(()) => format!("✓ Deleted {target}"),
                    Err(e) => format!("Error: {e:#}"),
                };
                self.refresh_library();
            }
            _ => {}
        }
    }

    /// Have Claude read recent mail with `email` and update their profile. Tries the shown
    /// inboxes first, then the rest, and uses the first one that has mail with them.
    pub(super) fn learn(&mut self, email: String) {
        let mut order: Vec<String> = self.shown.clone();
        let mut rest: Vec<String> = self.engines.keys().filter(|a| !order.contains(a)).cloned().collect();
        rest.sort();
        order.extend(rest);
        let engines: Vec<Arc<crate::engine::Engine>> = order.iter().map(|a| self.eng(a)).collect();
        self.status = format!("Claude is reading your mail with {email}…");
        self.spawn(move |_| {
            let mut last = None;
            for e in engines {
                match e.learn_contact(&email) {
                    Ok(c) => return Msg::Learned(Ok(c)),
                    Err(err) => last = Some(err),
                }
            }
            Msg::Learned(Err(last.unwrap_or_else(|| anyhow::anyhow!("no inboxes"))))
        });
    }

    pub(super) fn draw_library(&mut self, f: &mut Frame, area: Rect) {
        let View::Library(lib) = &self.view else { return };
        let th = &self.th;
        let [left, right] = Layout::horizontal([Constraint::Length(44.min(area.width / 2)), Constraint::Fill(1)]).areas(area);
        let title = |name: &str, n: usize| {
            Line::from(vec![
                Span::styled(format!(" {name} "), Style::new().fg(th.bright).bold()),
                Span::styled(format!(" {n} "), Style::new().fg(th.on_chip).bg(th.accent)),
                Span::styled("   Tab switches", Style::new().fg(th.muted)),
            ])
        };
        let (header, rows): (Line, Vec<Line>) = match lib.kind {
            LibKind::Templates => (
                title("Templates", lib.templates.len()),
                lib.templates
                    .iter()
                    .map(|t| Line::from(vec![Span::styled(format!(" {}", t.name), Style::new().fg(th.fg).bold())]))
                    .collect(),
            ),
            LibKind::People => (
                title("People", lib.people.len()),
                lib.people
                    .iter()
                    .map(|c| {
                        let name = if c.name.is_empty() { c.email.clone() } else { c.name.clone() };
                        let mut l = vec![
                            Span::styled(format!(" {initial} ", initial = name.chars().next().unwrap_or('?').to_uppercase()), Style::new().fg(th.on_chip).bg(th.avatar(&c.email)).bold()),
                            Span::styled(format!(" {name}"), Style::new().fg(th.fg).bold()),
                        ];
                        if !c.relationship.is_empty() {
                            l.push(Span::styled(format!("  {}", c.relationship), Style::new().fg(th.muted)));
                        }
                        if !c.learned.is_empty() {
                            l.push(Span::styled(" ✦", Style::new().fg(th.sun)));
                        }
                        Line::from(l)
                    })
                    .collect(),
            ),
        };
        let empty = rows.is_empty();
        let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
        let list = List::new(items)
            .block(Block::new().title(header).padding(Padding::top(1)))
            .highlight_style(Style::new().bg(th.selection));
        let mut state = ListState::default().with_selected(Some(lib.sel));
        f.render_stateful_widget(list, left, &mut state);
        if empty {
            let hint = match lib.kind {
                LibKind::Templates => "No templates yet.\n\nn  write one yourself\nw  describe it and Claude writes it",
                LibKind::People => "No profiles yet.\n\nClaude starts one whenever it drafts a reply,\nor press n to add someone.",
            };
            f.render_widget(Paragraph::new(hint).fg(th.muted).block(Block::new().padding(Padding::new(2, 0, 3, 0))), left);
        }

        let label = |k: &str| Span::styled(format!("{k:<13}"), Style::new().fg(th.muted));
        let mut lines: Vec<Line> = vec![];
        match lib.kind {
            LibKind::Templates => {
                if let Some(t) = lib.templates.get(lib.sel) {
                    lines.push(Line::from(Span::styled(t.name.clone(), Style::new().fg(th.bright).bold())));
                    if !t.description.is_empty() {
                        lines.push(Line::from(Span::styled(t.description.clone(), Style::new().fg(th.muted))));
                    }
                    lines.push(Line::raw(""));
                    lines.push(Line::from(vec![label("Subject"), Span::raw(if t.subject.is_empty() { "(keeps the thread's Re: subject)".into() } else { t.subject.clone() })]));
                    lines.push(Line::raw(""));
                    for l in t.body.lines() {
                        lines.push(highlight_placeholders(l, th.fg, th.accent));
                    }
                }
            }
            LibKind::People => {
                if let Some(c) = lib.people.get(lib.sel) {
                    let name = if c.name.is_empty() { c.email.clone() } else { c.name.clone() };
                    lines.push(Line::from(Span::styled(name, Style::new().fg(th.bright).bold())));
                    lines.push(Line::from(Span::styled(c.email.clone(), Style::new().fg(th.muted))));
                    lines.push(Line::raw(""));
                    for (k, v) in [("Company", &c.company), ("Role", &c.role), ("Relationship", &c.relationship), ("Tone", &c.tone)] {
                        if !v.is_empty() {
                            lines.push(Line::from(vec![label(k), Span::raw(v.clone())]));
                        }
                    }
                    if !c.tags.is_empty() {
                        lines.push(Line::from(vec![label("Tags"), Span::raw(c.tags.join(", "))]));
                    }
                    for (k, v) in &c.fields {
                        lines.push(Line::from(vec![label(k), Span::raw(v.clone())]));
                    }
                    if !c.notes.is_empty() {
                        lines.push(Line::raw(""));
                        lines.push(Line::from(Span::styled("Notes", Style::new().fg(th.muted).bold())));
                        lines.extend(c.notes.lines().map(|l| Line::raw(l.to_string())));
                    }
                    lines.push(Line::raw(""));
                    lines.push(Line::from(Span::styled("✦ What Claude has learned", Style::new().fg(th.sun).bold())));
                    if c.learned.is_empty() {
                        lines.push(Line::from(Span::styled("Nothing yet — press L to have Claude read your mail with them.", Style::new().fg(th.muted))));
                    } else {
                        lines.extend(c.learned.lines().map(|l| Line::raw(l.to_string())));
                    }
                }
            }
        }
        let detail = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::LEFT).border_style(Style::new().fg(th.rule)).padding(Padding::new(3, 3, 1, 0)));
        f.render_widget(detail, right);
    }
}

/// Show {{placeholders}} in the accent colour.
fn highlight_placeholders(line: &str, fg: Color, accent: Color) -> Line<'static> {
    let mut spans = vec![];
    let mut rest = line;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start..].find("}}") else { break };
        spans.push(Span::styled(rest[..start].to_string(), Style::new().fg(fg)));
        spans.push(Span::styled(rest[start..start + len + 2].to_string(), Style::new().fg(accent)));
        rest = &rest[start + len + 2..];
    }
    spans.push(Span::styled(rest.to_string(), Style::new().fg(fg)));
    Line::from(spans)
}
