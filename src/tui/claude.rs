//! The ✦ Claude tab: what matters in the inbox, replies Claude drafted for you to check and
//! send, and template ideas. Claude never sends; ↵ opens its draft in the composer.

use super::form::Form;
use super::{App, ComposeForm, InputKind, Msg, Overlay, CLAUDE_TAB};
use crate::models::{AiDraft, Briefing, BriefItem, Contact, Template, TemplateIdea, ThreadSummary};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{HashMap, HashSet, VecDeque};

/// Replies Claude drafts on its own after a briefing (urgent/high items that need a reply).
const AUTO_DRAFTS: usize = 3;

#[derive(Default)]
pub struct ClaudeState {
    briefing: Option<Briefing>,
    drafts: HashMap<String, AiDraft>,
    briefing_now: bool,
    /// Threads with a draft being written.
    busy: HashSet<String>,
    /// Automatic drafts wait here and run one at a time.
    queue: VecDeque<(String, String)>,
    auto_running: Option<String>,
    sel: usize,
    offset: usize,
    scroll: u16,
}

enum Row<'a> {
    Item(&'a BriefItem),
    Idea(&'a TemplateIdea),
}

impl App {
    pub(super) fn load_ai(&mut self) {
        self.claude.briefing = self.e.last_briefing().ok().flatten();
        self.load_ai_drafts();
    }

    pub(super) fn load_ai_drafts(&mut self) {
        self.claude.drafts = self.e.ai_drafts().unwrap_or_default().into_iter().map(|d| (d.thread_id.clone(), d)).collect();
    }

    /// Drafts waiting to be checked, for the tab badge.
    pub(super) fn ready_drafts(&self) -> usize {
        self.claude
            .drafts
            .values()
            .filter(|d| d.status == "ready" && (self.inbox.is_empty() || self.inbox.iter().any(|t| t.id == d.thread_id)))
            .count()
    }

    fn rows(&self) -> Vec<Row<'_>> {
        match &self.claude.briefing {
            Some(b) => self.rows_of(b),
            None => vec![],
        }
    }

    fn rows_of<'a>(&self, b: &'a Briefing) -> Vec<Row<'a>> {
        // Threads that left the inbox (archived, answered elsewhere) drop off.
        let mut rows: Vec<Row> = b
            .items
            .iter()
            .filter(|i| self.inbox.is_empty() || self.inbox.iter().any(|t| t.id == i.thread_id))
            .map(Row::Item)
            .collect();
        rows.extend(b.template_ideas.iter().map(Row::Idea));
        rows
    }

    fn thread_of(&self, id: &str) -> Option<&ThreadSummary> {
        self.inbox.iter().find(|t| t.id == id)
    }

    pub(super) fn claude_selected_thread(&self) -> Option<ThreadSummary> {
        let rows = self.rows();
        let Some(Row::Item(i)) = rows.get(self.claude.sel) else { return None };
        self.thread_of(&i.thread_id).cloned().or_else(|| {
            Some(ThreadSummary { id: i.thread_id.clone(), subject: i.subject.clone(), account: i.account.clone(), ..Default::default() })
        })
    }

    pub(super) fn claude_move(&mut self, d: i32) {
        let n = self.rows().len() as i32;
        if n > 0 {
            self.claude.sel = (self.claude.sel as i32 + d).clamp(0, n - 1) as usize;
            self.claude.scroll = 0;
        }
    }

    pub(super) fn claude_click(&mut self, y: u16) {
        let i = self.claude.offset + (y / 2) as usize;
        if i < self.rows().len() {
            self.claude.sel = i;
            self.claude.scroll = 0;
        }
    }

    /// Brief automatically the first time the tab opens, or when the shown inboxes changed.
    pub(super) fn maybe_auto_brief(&mut self) {
        if self.tab != CLAUDE_TAB || self.claude.briefing_now || self.inbox.is_empty() || self.loading {
            return;
        }
        let stale = match &self.claude.briefing {
            None => true,
            Some(b) => {
                let mut a = b.accounts.clone();
                let mut s = self.shown.clone();
                a.sort();
                s.sort();
                a != s
            }
        };
        if stale {
            self.run_brief();
        }
    }

    fn run_brief(&mut self) {
        if self.claude.briefing_now {
            return;
        }
        if self.inbox.is_empty() {
            self.status = "Inbox is still loading…".into();
            return;
        }
        self.claude.briefing_now = true;
        let threads = self.inbox.clone();
        let accounts = self.shown.clone();
        self.status = format!("Claude is reading {} threads…", threads.len());
        self.spawn(move |e| Msg::Briefed(e.brief(&threads, &accounts)));
    }

    pub(super) fn on_briefed(&mut self, r: anyhow::Result<Briefing>) {
        self.claude.briefing_now = false;
        match r {
            Ok(b) => {
                self.status = format!("✓ Claude ranked {} threads", b.items.len());
                for i in b.items.iter().filter(|i| i.action == "reply" && matches!(i.priority.as_str(), "urgent" | "high")).take(AUTO_DRAFTS) {
                    let has = self.claude.drafts.get(&i.thread_id).is_some_and(|d| d.status != "dismissed");
                    if !has && !self.claude.busy.contains(&i.thread_id) {
                        self.claude.queue.push_back((i.thread_id.clone(), i.account.clone()));
                    }
                }
                self.claude.briefing = Some(b);
                self.claude.sel = 0;
                self.claude.scroll = 0;
                self.pump_drafts();
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    fn pump_drafts(&mut self) {
        if self.claude.auto_running.is_some() {
            return;
        }
        while let Some((id, account)) = self.claude.queue.pop_front() {
            if self.claude.busy.contains(&id) {
                continue;
            }
            self.claude.auto_running = Some(id.clone());
            self.claude.busy.insert(id.clone());
            self.spawn_on(self.eng(&account), move |e| {
                let r = e.ai_draft(&id, None);
                Msg::Drafted(id, r)
            });
            return;
        }
    }

    /// Draft (or redraft with a note) now.
    pub(super) fn start_draft(&mut self, thread_id: String, account: String, note: Option<String>) {
        if self.claude.busy.contains(&thread_id) {
            self.status = "Claude is already drafting that one…".into();
            return;
        }
        self.claude.busy.insert(thread_id.clone());
        self.status = "Claude is drafting a reply…".into();
        self.spawn_on(self.eng(&account), move |e| {
            let r = e.ai_draft(&thread_id, note.as_deref());
            Msg::Drafted(thread_id, r)
        });
    }

    pub(super) fn on_drafted(&mut self, id: String, r: anyhow::Result<AiDraft>) {
        self.claude.busy.remove(&id);
        if self.claude.auto_running.as_deref() == Some(id.as_str()) {
            self.claude.auto_running = None;
        }
        match r {
            Ok(d) => {
                self.status = format!("✓ Draft ready: {}", d.subject);
                self.claude.drafts.insert(id, d);
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
        self.pump_drafts();
    }

    pub(super) fn on_learned(&mut self, r: anyhow::Result<Contact>) {
        match r {
            Ok(c) => {
                let who = if c.name.is_empty() { c.email.clone() } else { c.name.clone() };
                self.status = format!("✓ Claude updated what it knows about {who}");
                self.refresh_library();
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    /// Open Claude's draft in the composer to check, edit and send.
    fn review_draft(&mut self, d: AiDraft) {
        self.overlay = Overlay::Compose(ComposeForm {
            title: " Check Claude's reply ".into(),
            fields: [d.to, d.cc, String::new(), d.subject, d.body],
            focus: 4,
            reply_to: Some(d.thread_id.clone()),
            from: if d.account.is_empty() { self.me.clone() } else { d.account },
            ai_thread: Some(d.thread_id),
            armed_discard: false,
        });
        self.status = "Ctrl+S sends · Ctrl+D saves it as a Gmail draft · edit anything first".into();
    }

    /// Keys for the Claude tab. Returns false to let the shared handler take the key.
    pub(super) fn claude_key(&mut self, k: KeyEvent) -> bool {
        enum Pick {
            Item(BriefItem),
            Idea(TemplateIdea),
            None,
        }
        let pick = match self.rows().get(self.claude.sel) {
            Some(Row::Item(i)) => Pick::Item((*i).clone()),
            Some(Row::Idea(t)) => Pick::Idea((*t).clone()),
            None => Pick::None,
        };
        let draft = match &pick {
            Pick::Item(i) => self.claude.drafts.get(&i.thread_id).filter(|d| d.status != "dismissed").cloned(),
            _ => None,
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.claude_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.claude_move(-1),
            KeyCode::Home => self.claude.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.claude_move(i32::MAX / 2),
            KeyCode::Char('J') | KeyCode::PageDown => self.claude.scroll = self.claude.scroll.saturating_add(3),
            KeyCode::Char('K') | KeyCode::PageUp => self.claude.scroll = self.claude.scroll.saturating_sub(3),
            KeyCode::Char('R') => self.run_brief(),
            KeyCode::Enter => match pick {
                Pick::Idea(t) => {
                    let tpl = Template { name: t.name, description: t.description, subject: t.subject, body: t.body, updated_at: String::new() };
                    self.overlay = Overlay::Form(Form::template(Some(tpl), None));
                }
                Pick::Item(i) => match draft {
                    Some(d) => self.review_draft(d),
                    None if i.action == "reply" => self.start_draft(i.thread_id, i.account, None),
                    None => {
                        if let Some(t) = self.claude_selected_thread() {
                            self.open_thread(t);
                        }
                    }
                },
                Pick::None => {}
            },
            KeyCode::Char('o') => {
                if let Some(t) = self.claude_selected_thread() {
                    self.open_thread(t);
                }
            }
            KeyCode::Char('D') => {
                if let Pick::Item(i) = pick {
                    self.start_draft(i.thread_id, i.account, None);
                }
            }
            KeyCode::Char('w') => {
                if let Pick::Item(i) = pick {
                    self.overlay = Overlay::Input {
                        kind: InputKind::ClaudeNote { thread_id: i.thread_id, account: i.account },
                        buf: String::new(),
                    };
                }
            }
            KeyCode::Char('x') => {
                if let (Pick::Item(i), Some(_)) = (&pick, &draft) {
                    let _ = self.e.set_ai_draft_status(&i.thread_id, "dismissed");
                    self.load_ai_drafts();
                    self.status = "Dismissed Claude's draft".into();
                }
            }
            KeyCode::Char('L') => {
                if let Pick::Item(i) = &pick {
                    match self.thread_of(&i.thread_id).map(|t| t.from_email.clone()) {
                        Some(email) if !email.is_empty() => self.learn(email),
                        _ => self.status = "Can't tell who this thread is from".into(),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn draw_claude(&mut self, f: &mut Frame, area: Rect) {
        let th = self.th.clone();
        let Some(b) = self.claude.briefing.clone() else {
            let msg = if self.claude.briefing_now {
                format!("✦ Claude is reading {} threads…", self.inbox.len())
            } else if self.inbox.is_empty() {
                "Loading your inbox…".to_string()
            } else {
                "Press R and Claude will read your inbox, rank what matters and draft replies.".to_string()
            };
            f.render_widget(Paragraph::new(msg).fg(th.muted).alignment(Alignment::Center).block(Block::new().padding(Padding::top(area.height / 3))), area);
            return;
        };

        let when = chrono::DateTime::parse_from_rfc3339(&b.created_at).map(|d| d.format("%a %-I:%M %p").to_string()).unwrap_or_default();
        let header = vec![
            Line::from(vec![
                Span::styled("✦ ", Style::new().fg(th.sun)),
                Span::styled(b.summary.clone(), Style::new().fg(th.bright)),
            ]),
            Line::from(Span::styled(
                format!(
                    "  {}updated {when} · {} · R refresh",
                    if self.claude.briefing_now { "⟳ refreshing… · " } else { "" },
                    self.e.claude_backend()
                ),
                Style::new().fg(th.muted),
            )),
        ];
        let head = Paragraph::new(header).wrap(Wrap { trim: true }).block(Block::new().padding(Padding::new(1, 1, 0, 1)));
        let head_h = (head.line_count(area.width.saturating_sub(2)) as u16).min(6);
        let [top, body] = Layout::vertical([Constraint::Length(head_h), Constraint::Fill(1)]).areas(area);
        f.render_widget(head, top);

        let list_w = (body.width * 2 / 5).clamp(44.min(body.width), 70);
        let [left, right] = Layout::horizontal([Constraint::Length(list_w), Constraint::Fill(1)]).areas(body);

        let multi = self.shown.len() > 1;
        let rows = self.rows_of(&b);
        let pill = |p: &str| {
            let c = match p {
                "urgent" => th.error,
                "high" => th.sun,
                "normal" => th.accent,
                _ => th.muted,
            };
            Span::styled(format!(" {:<6} ", p), Style::new().fg(th.on_chip).bg(c).bold())
        };
        let items: Vec<ListItem> = rows
            .iter()
            .map(|r| match r {
                Row::Item(i) => {
                    let badge = if self.claude.busy.contains(&i.thread_id) {
                        Span::styled("  … drafting", Style::new().fg(th.muted))
                    } else {
                        match self.claude.drafts.get(&i.thread_id).map(|d| d.status.as_str()) {
                            Some("ready") => Span::styled("  ✎ draft ready", Style::new().fg(th.sun).bold()),
                            Some("saved") => Span::styled("  ✎ in Gmail drafts", Style::new().fg(th.muted)),
                            Some("sent") => Span::styled("  ✓ sent", Style::new().fg(th.muted)),
                            _ => Span::styled(format!("  {}", i.action.replace('_', " ")), Style::new().fg(th.muted)),
                        }
                    };
                    let mut l1 = vec![pill(&i.priority), Span::styled(format!(" {}", i.from), Style::new().fg(th.bright).bold()), badge];
                    if multi {
                        l1.push(Span::styled(format!("  ● {}", super::account_tag(&i.account, &self.shown)), Style::new().fg(th.avatar(&i.account))));
                    }
                    ListItem::new(vec![Line::from(l1), Line::from(Span::styled(format!("         {}", i.subject), Style::new().fg(th.muted)))])
                }
                Row::Idea(t) => ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(" idea   ", Style::new().fg(th.on_chip).bg(th.accent).bold()),
                        Span::styled(format!(" template “{}”", t.name), Style::new().fg(th.bright).bold()),
                    ]),
                    Line::from(Span::styled(format!("         {}", t.description), Style::new().fg(th.muted))),
                ]),
            })
            .collect();
        let empty = items.is_empty();
        self.claude.sel = self.claude.sel.min(rows.len().saturating_sub(1));
        let mut state = ListState::default().with_selected(Some(self.claude.sel)).with_offset(self.claude.offset);
        f.render_stateful_widget(List::new(items).highlight_style(Style::new().bg(th.selection)), left, &mut state);
        self.claude.offset = state.offset();
        self.hits.list = left;
        self.hits.row_height = 2;
        if empty {
            f.render_widget(Paragraph::new("  Nothing left from this briefing. R to refresh.").fg(th.muted), left);
        }

        let mut lines: Vec<Line> = vec![];
        let label = |s: &str| Span::styled(format!("{s:<9}"), Style::new().fg(th.muted));
        match rows.get(self.claude.sel) {
            Some(Row::Item(i)) => {
                lines.push(Line::from(Span::styled(i.subject.clone(), Style::new().fg(th.bright).bold())));
                let mut meta = vec![pill(&i.priority), Span::styled(format!("  {} · {}", i.from, i.category), Style::new().fg(th.muted))];
                if multi {
                    meta.push(Span::styled(format!(" · in {}", i.account), Style::new().fg(th.muted)));
                }
                lines.push(Line::from(meta));
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(i.why.clone(), Style::new().fg(th.fg))));
                lines.push(Line::raw(""));
                let draft = self.claude.drafts.get(&i.thread_id).filter(|d| d.status != "dismissed");
                if self.claude.busy.contains(&i.thread_id) {
                    lines.push(Line::from(Span::styled("✦ Claude is writing a reply…", Style::new().fg(th.sun))));
                } else if let Some(d) = draft {
                    let state = match d.status.as_str() {
                        "sent" => " (sent)",
                        "saved" => " (saved to Gmail drafts)",
                        _ => "",
                    };
                    lines.push(Line::from(Span::styled(format!("✎ Claude's draft{state}"), Style::new().fg(th.sun).bold())));
                    lines.push(Line::from(vec![label("To"), Span::raw(d.to.clone())]));
                    if !d.cc.is_empty() {
                        lines.push(Line::from(vec![label("Cc"), Span::raw(d.cc.clone())]));
                    }
                    lines.push(Line::from(vec![label("Subject"), Span::raw(d.subject.clone())]));
                    if !d.template_used.is_empty() {
                        lines.push(Line::from(vec![label("Template"), Span::styled(d.template_used.clone(), Style::new().fg(th.accent))]));
                    }
                    lines.push(Line::raw(""));
                    for l in d.body.lines() {
                        let gap = l.contains("[[");
                        lines.push(Line::from(Span::styled(l.to_string(), Style::new().fg(if gap { th.sun } else { th.fg }))));
                    }
                    if !d.check.is_empty() {
                        lines.push(Line::raw(""));
                        for c in &d.check {
                            lines.push(Line::from(vec![Span::styled("⚠ ", Style::new().fg(th.sun)), Span::styled(c.clone(), Style::new().fg(th.muted))]));
                        }
                    }
                    lines.push(Line::raw(""));
                    lines.push(Line::from(Span::styled("↵ check & send · w redraft with a note · x dismiss", Style::new().fg(th.muted))));
                } else if i.action == "reply" {
                    lines.push(Line::from(Span::styled("↵ or D: have Claude draft a reply · w: draft with a note", Style::new().fg(th.muted))));
                } else {
                    lines.push(Line::from(Span::styled("↵ open · D draft a reply anyway · e archive", Style::new().fg(th.muted))));
                }
                // What Claude knows about the sender.
                if let Some(c) = self.thread_of(&i.thread_id).and_then(|t| self.e.store.contact(&t.from_email.to_lowercase()).ok().flatten()) {
                    if !c.learned.is_empty() {
                        lines.push(Line::raw(""));
                        lines.push(Line::from(Span::styled(format!("✦ About {}", if c.name.is_empty() { &c.email } else { &c.name }), Style::new().fg(th.sun).bold())));
                        lines.extend(c.learned.lines().map(|l| Line::from(Span::styled(l.to_string(), Style::new().fg(th.muted)))));
                    }
                }
            }
            Some(Row::Idea(t)) => {
                lines.push(Line::from(Span::styled(format!("Template idea: {}", t.name), Style::new().fg(th.bright).bold())));
                lines.push(Line::from(Span::styled(t.why.clone(), Style::new().fg(th.muted))));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![label("Subject"), Span::raw(t.subject.clone())]));
                lines.push(Line::raw(""));
                lines.extend(t.body.lines().map(|l| Line::raw(l.to_string())));
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled("↵ edit and save it as a template", Style::new().fg(th.muted))));
            }
            None => {}
        }
        let detail = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::LEFT).border_style(Style::new().fg(th.rule)).padding(Padding::new(3, 3, 0, 0)));
        let max = detail.line_count(right.width.saturating_sub(7)) as u16;
        self.claude.scroll = self.claude.scroll.min(max.saturating_sub(right.height / 2));
        f.render_widget(detail.scroll((self.claude.scroll, 0)), right);
    }
}
