//! Interactive inbox: Tempo-style tabs + list + reader, $EDITOR for writing.

use crate::engine::{Actor, Engine, SendReq};
use crate::models::{Template, ThreadDetail, ThreadSummary};
use crate::{edit_text, fmt_date};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

const TABS: [&str; 5] = ["To do", "Primary", "Starred", "Newsletters", "All"];
const ACCENT: Color = Color::Rgb(59, 91, 255);
const CRAYON: Color = Color::Rgb(255, 200, 40);
const MUTED: Color = Color::Rgb(142, 142, 153);
const AVATARS: [Color; 6] = [
    Color::Rgb(239, 108, 87),
    Color::Rgb(94, 172, 112),
    Color::Rgb(91, 130, 230),
    Color::Rgb(201, 120, 220),
    Color::Rgb(232, 168, 56),
    Color::Rgb(70, 180, 190),
];

enum Msg {
    Inbox(Result<Vec<ThreadSummary>>),
    Searched(String, Result<Vec<ThreadSummary>>),
    Thread(Result<ThreadDetail>),
    /// An action finished; refresh the inbox afterwards when `reload`.
    Done { status: Result<String>, reload: bool },
}

enum View {
    List,
    Reader { t: ThreadDetail, scroll: u16, full: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum InputKind {
    Search,
    Label,
}

/// A message written in $EDITOR, waiting for y/d/n.
struct Pending {
    req: SendReq,
    preview: String,
}

enum Overlay {
    None,
    Help,
    Templates { items: Vec<Template>, sel: usize, reply_to: Option<String> },
    Confirm(Pending),
    Input { kind: InputKind, buf: String },
}

/// What needs the real terminal (editor) — handled by the main loop.
enum EditorJob {
    Compose { req: SendReq, template: Option<Template> },
}

struct App {
    e: Arc<Engine>,
    me: String,
    tx: Sender<Msg>,
    inbox: Vec<ThreadSummary>,
    search: Option<(String, Vec<ThreadSummary>)>,
    tab: usize,
    table: TableState,
    view: View,
    overlay: Overlay,
    status: String,
    loading: bool,
    job: Option<EditorJob>,
    quit: bool,
}

pub fn run(engine: Engine) -> Result<()> {
    let me = engine.account()?;
    let e = Arc::new(engine);
    let (tx, rx) = channel();
    let mut app = App {
        e,
        me,
        tx,
        inbox: vec![],
        search: None,
        tab: 0,
        table: TableState::default().with_selected(0),
        view: View::List,
        overlay: Overlay::None,
        status: String::new(),
        loading: false,
        job: None,
        quit: false,
    };
    app.reload();

    let mut term = ratatui::init();
    let res = main_loop(&mut term, &mut app, &rx);
    ratatui::restore();
    res
}

fn main_loop(term: &mut ratatui::DefaultTerminal, app: &mut App, rx: &Receiver<Msg>) -> Result<()> {
    while !app.quit {
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
        }
        term.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.on_key(k);
                }
            }
        }
        if let Some(job) = app.job.take() {
            // Hand the terminal to $EDITOR, then come back.
            disable_raw_mode()?;
            std::io::stdout().execute(LeaveAlternateScreen)?;
            let edited = app.run_editor(job);
            std::io::stdout().execute(EnterAlternateScreen)?;
            enable_raw_mode()?;
            term.clear()?;
            match edited {
                Ok(Some(p)) => app.overlay = Overlay::Confirm(p),
                Ok(None) => app.status = "Discarded empty message".into(),
                Err(err) => app.status = format!("Error: {err:#}"),
            }
        }
    }
    Ok(())
}

impl App {
    fn spawn(&self, f: impl FnOnce(&Engine) -> Msg + Send + 'static) {
        let e = self.e.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f(&e));
        });
    }

    fn reload(&mut self) {
        self.loading = true;
        self.spawn(|e| Msg::Inbox(e.search(&Actor::User, "in:inbox", 60)));
    }

    fn do_action(&mut self, label: &'static str, id: String, f: impl FnOnce(&Engine, &str) -> Result<()> + Send + 'static) {
        self.status = format!("{label}…");
        self.spawn(move |e| Msg::Done { status: f(e, &id).map(|_| label.to_string()), reload: true });
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Inbox(r) => {
                self.loading = false;
                match r {
                    Ok(ts) => {
                        self.inbox = ts;
                        if self.search.is_none() && self.status.ends_with('…') {
                            self.status.clear();
                        }
                        // Land on a non-empty tab the first time.
                        if self.visible().is_empty() && self.tab == 0 && !self.inbox.is_empty() {
                            self.tab = 1;
                        }
                        self.clamp();
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Searched(q, r) => {
                self.loading = false;
                match r {
                    Ok(ts) => {
                        self.status = format!("{} result(s) for “{q}” — Esc to clear", ts.len());
                        self.search = Some((q, ts));
                        self.table.select(Some(0));
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Thread(r) => {
                self.loading = false;
                match r {
                    Ok(t) => {
                        self.status.clear();
                        self.view = View::Reader { t, scroll: 0, full: false };
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Done { status, reload } => {
                match status {
                    Ok(s) => self.status = format!("✓ {s}"),
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
                if reload {
                    self.reload();
                }
            }
        }
    }

    fn visible(&self) -> Vec<&ThreadSummary> {
        if let Some((_, ts)) = &self.search {
            return ts.iter().collect();
        }
        self.inbox
            .iter()
            .filter(|t| match self.tab {
                0 => t.needs_reply,
                1 => !t.is_bulk,
                2 => t.starred,
                3 => t.is_bulk,
                _ => true,
            })
            .collect()
    }

    fn counts(&self) -> [usize; 5] {
        let c = |f: fn(&ThreadSummary) -> bool| self.inbox.iter().filter(|t| f(t)).count();
        [c(|t| t.needs_reply), c(|t| !t.is_bulk), c(|t| t.starred), c(|t| t.is_bulk), self.inbox.len()]
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        let i = self.table.selected().unwrap_or(0).min(n.saturating_sub(1));
        self.table.select(Some(i));
    }

    fn selected(&self) -> Option<ThreadSummary> {
        if let View::Reader { t, .. } = &self.view {
            return self.inbox.iter().chain(self.search.iter().flat_map(|s| s.1.iter())).find(|s| s.id == t.id).cloned()
                .or_else(|| Some(ThreadSummary {
                    id: t.id.clone(), subject: t.subject.clone(), from_name: String::new(), from_email: String::new(),
                    snippet: String::new(), date: 0, message_count: t.messages.len(), unread: false, starred: false,
                    needs_reply: false, is_bulk: false, labels: vec![],
                }));
        }
        let v = self.visible();
        self.table.selected().and_then(|i| v.get(i).map(|t| (*t).clone()))
    }

    // ---------- keys ----------

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::None => {}
            Overlay::Help => return,
            Overlay::Input { kind, mut buf } => {
                match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.submit_input(kind, buf),
                    KeyCode::Backspace => {
                        buf.pop();
                        self.overlay = Overlay::Input { kind, buf };
                    }
                    KeyCode::Char(c) => {
                        buf.push(c);
                        self.overlay = Overlay::Input { kind, buf };
                    }
                    _ => self.overlay = Overlay::Input { kind, buf },
                }
                return;
            }
            Overlay::Templates { items, mut sel, reply_to } => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Down | KeyCode::Char('j') => {
                        sel = (sel + 1).min(items.len().saturating_sub(1));
                        self.overlay = Overlay::Templates { items, sel, reply_to };
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        sel = sel.saturating_sub(1);
                        self.overlay = Overlay::Templates { items, sel, reply_to };
                    }
                    KeyCode::Enter => {
                        if let Some(t) = items.get(sel).cloned() {
                            let req = SendReq { reply_to_thread: reply_to, ..Default::default() };
                            self.job = Some(EditorJob::Compose { req, template: Some(t) });
                        }
                    }
                    _ => self.overlay = Overlay::Templates { items, sel, reply_to },
                }
                return;
            }
            Overlay::Confirm(p) => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.status = "Sending…".into();
                        let req = p.req;
                        self.spawn(move |e| Msg::Done { status: e.send(&Actor::User, &req).map(|_| "Sent".into()), reload: true });
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        self.status = "Saving draft…".into();
                        let req = p.req;
                        self.spawn(move |e| Msg::Done {
                            status: e.draft(&Actor::User, &req).map(|_| "Draft saved".into()),
                            reload: false,
                        });
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        self.job = Some(EditorJob::Compose { req: p.req, template: None });
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.status = "Discarded".into(),
                    _ => self.overlay = Overlay::Confirm(p),
                }
                return;
            }
        }

        // Keys shared by list + reader.
        let sel = self.selected();
        match k.code {
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                return;
            }
            KeyCode::Char('c') => {
                self.job = Some(EditorJob::Compose { req: SendReq::default(), template: None });
                return;
            }
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input { kind: InputKind::Search, buf: String::new() };
                return;
            }
            KeyCode::Char('g') => {
                self.status = "Refreshing…".into();
                self.reload();
                return;
            }
            _ => {}
        }
        if let Some(t) = &sel {
            let id = t.id.clone();
            match k.code {
                KeyCode::Char('r') => {
                    self.job = Some(EditorJob::Compose {
                        req: SendReq { reply_to_thread: Some(id), ..Default::default() },
                        template: None,
                    });
                    return;
                }
                KeyCode::Char('T') => {
                    match self.e.templates(&Actor::User) {
                        Ok(items) if items.is_empty() => {
                            self.status = "No templates yet — `crayon templates set <name>` or ask an agent".into()
                        }
                        Ok(items) => self.overlay = Overlay::Templates { items, sel: 0, reply_to: Some(id) },
                        Err(e) => self.status = format!("Error: {e:#}"),
                    }
                    return;
                }
                KeyCode::Char('f') => {
                    if let View::Reader { t: detail, .. } = &self.view {
                        let last = detail.messages.last();
                        let quoted = last
                            .map(|m| format!("\n\n---------- Forwarded message ---------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n{}", m.from, fmt_date(m.date, true), m.subject, m.to, m.body))
                            .unwrap_or_default();
                        let req = SendReq { subject: format!("Fwd: {}", detail.subject), body: quoted, ..Default::default() };
                        self.job = Some(EditorJob::Compose { req, template: None });
                    } else {
                        self.status = "Open the thread first (Enter), then f to forward".into();
                    }
                    return;
                }
                KeyCode::Char('e') => {
                    self.leave_reader();
                    self.do_action("Archived", id, |e, id| e.archive(&Actor::User, id));
                    return;
                }
                KeyCode::Char('#') => {
                    self.leave_reader();
                    self.do_action("Moved to trash", id, |e, id| e.trash(&Actor::User, id));
                    return;
                }
                KeyCode::Char('s') => {
                    let on = !t.starred;
                    self.do_action(if on { "Starred" } else { "Unstarred" }, id, move |e, id| e.star(&Actor::User, id, on));
                    return;
                }
                KeyCode::Char('u') => {
                    let read = t.unread;
                    self.do_action(if read { "Marked read" } else { "Marked unread" }, id, move |e, id| e.mark_read(&Actor::User, id, read));
                    return;
                }
                KeyCode::Char('l') => {
                    self.overlay = Overlay::Input { kind: InputKind::Label, buf: String::new() };
                    return;
                }
                _ => {}
            }
        }

        match &mut self.view {
            View::Reader { scroll, full, .. } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => self.leave_reader(),
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Char(' ') | KeyCode::PageDown => *scroll = scroll.saturating_add(15),
                KeyCode::PageUp | KeyCode::Char('b') => *scroll = scroll.saturating_sub(15),
                KeyCode::Char('p') => {
                    *full = !*full;
                    *scroll = 0;
                }
                _ => {}
            },
            View::List => match k.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc => {
                    if self.search.take().is_some() {
                        self.status.clear();
                        self.clamp();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::Char('G') | KeyCode::End => self.move_sel(i32::MAX / 2),
                KeyCode::Home => self.table.select(Some(0)),
                KeyCode::Tab | KeyCode::Right => self.set_tab((self.tab + 1) % TABS.len()),
                KeyCode::BackTab | KeyCode::Left => self.set_tab((self.tab + TABS.len() - 1) % TABS.len()),
                KeyCode::Char(c @ '1'..='5') => self.set_tab(c as usize - '1' as usize),
                KeyCode::Enter | KeyCode::Char('o') => {
                    if let Some(t) = sel {
                        self.loading = true;
                        self.status = "Opening…".into();
                        let id = t.id.clone();
                        self.spawn(move |e| {
                            let r = e.thread(&Actor::User, &id);
                            if r.is_ok() && t.unread {
                                let _ = e.mark_read(&Actor::User, &id, true);
                            }
                            Msg::Thread(r)
                        });
                        if t.unread {
                            if let Some(x) = self.inbox.iter_mut().find(|x| x.id == t.id) {
                                x.unread = false;
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn leave_reader(&mut self) {
        self.view = View::List;
        self.clamp();
    }

    fn set_tab(&mut self, t: usize) {
        self.search = None;
        self.tab = t;
        self.table.select(Some(0));
    }

    fn move_sel(&mut self, d: i32) {
        let n = self.visible().len() as i32;
        if n == 0 {
            return;
        }
        let i = (self.table.selected().unwrap_or(0) as i32 + d).clamp(0, n - 1);
        self.table.select(Some(i as usize));
    }

    fn submit_input(&mut self, kind: InputKind, buf: String) {
        let buf = buf.trim().to_string();
        if buf.is_empty() {
            return;
        }
        match kind {
            InputKind::Search => {
                self.loading = true;
                self.status = format!("Searching “{buf}”…");
                self.spawn(move |e| {
                    let r = e.search(&Actor::User, &buf, 50);
                    Msg::Searched(buf, r)
                });
            }
            InputKind::Label => {
                if let Some(t) = self.selected() {
                    self.status = format!("Labelling {buf}…");
                    let id = t.id;
                    self.spawn(move |e| Msg::Done {
                        status: e.label(&Actor::User, &id, &buf, false).map(|_| format!("Labelled {buf}")),
                        reload: true,
                    });
                }
            }
        }
    }

    // ---------- editor ----------

    fn run_editor(&mut self, job: EditorJob) -> Result<Option<Pending>> {
        let EditorJob::Compose { mut req, template } = job;
        if let Some(t) = &template {
            req.template = Some(t.name.clone());
        }
        // Resolve recipients/subject/template up front so the editor shows the real thing.
        let (to, cc, subject, body) = if req.reply_to_thread.is_some() || req.template.is_some() {
            let mut probe = req.clone();
            if probe.body.is_empty() && probe.template.is_none() {
                probe.body = " ".into();
            }
            if probe.to.is_empty() && probe.reply_to_thread.is_none() {
                probe.to = "recipient@example.com".into();
            }
            let c = self.e.compose(&probe)?;
            let to = if req.to.is_empty() && req.reply_to_thread.is_none() { String::new() } else { c.to };
            (to, c.cc, c.subject, c.body.trim().to_string())
        } else {
            (req.to.clone(), req.cc.clone(), req.subject.clone(), req.body.clone())
        };

        let draft = format!(
            "To: {to}\nCc: {cc}\nBcc: \nSubject: {subject}\n# ── write below; lines starting with # are ignored ──\n{}\n",
            body
        );
        let text = edit_text(&draft)?;

        let mut out = SendReq { reply_to_thread: req.reply_to_thread.clone(), ..Default::default() };
        let mut body_lines = vec![];
        let mut in_headers = true;
        for line in text.lines() {
            if line.starts_with('#') {
                in_headers = false;
                continue;
            }
            if in_headers {
                if let Some((k, v)) = line.split_once(':') {
                    let v = v.trim().to_string();
                    match k.trim().to_lowercase().as_str() {
                        "to" => out.to = v,
                        "cc" => out.cc = v,
                        "bcc" => out.bcc = v,
                        "subject" => out.subject = v,
                        _ => body_lines.push(line),
                    }
                    continue;
                }
                in_headers = false;
            }
            body_lines.push(line);
        }
        out.body = body_lines.join("\n").trim().to_string();
        if out.body.is_empty() {
            return Ok(None);
        }
        let preview = format!(
            "To: {}\n{}Subject: {}\n\n{}",
            out.to,
            if out.cc.is_empty() { String::new() } else { format!("Cc: {}\n", out.cc) },
            out.subject,
            out.body
        );
        Ok(Some(Pending { req: out, preview }))
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        let [top, main, bottom] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        self.draw_top(f, top);
        match &self.view {
            View::List => self.draw_list(f, main),
            View::Reader { .. } => self.draw_reader(f, main),
        }
        self.draw_bottom(f, bottom);
        self.draw_overlay(f);
    }

    fn draw_top(&self, f: &mut Frame, area: Rect) {
        let counts = self.counts();
        let mut spans = vec![
            Span::styled(" ▌", Style::new().fg(CRAYON)),
            Span::styled("Crayon Box ", Style::new().bold()),
            Span::raw("  "),
        ];
        if let Some((q, ts)) = &self.search {
            spans.push(Span::styled(format!(" Search: {q} ({}) ", ts.len()), Style::new().fg(Color::White).bg(ACCENT)));
        } else {
            for (i, name) in TABS.iter().enumerate() {
                let label = format!(" {name} {} ", counts[i]);
                spans.push(if i == self.tab {
                    Span::styled(label, Style::new().fg(ACCENT).bold().underlined())
                } else {
                    Span::styled(label, Style::new().fg(MUTED))
                });
            }
        }
        let right = format!("{}{} ", if self.loading { "⟳ " } else { "" }, self.me);
        let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right.chars().count() as u16)]).areas(area);
        f.render_widget(
            Paragraph::new(Line::from(spans)).block(Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(Color::Rgb(60, 60, 66)))),
            l,
        );
        f.render_widget(
            Paragraph::new(Span::styled(right, Style::new().fg(MUTED))).block(Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(Color::Rgb(60, 60, 66)))),
            r,
        );
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .visible()
            .iter()
            .map(|t| {
                let initial = t.from_name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
                let color = AVATARS[t.from_email.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize)) % AVATARS.len()];
                let dot = if t.unread { Span::styled("●", Style::new().fg(ACCENT)) } else { Span::raw(" ") };
                let name_style = if t.unread { Style::new().bold() } else { Style::new() };
                let mut from = vec![Span::styled(t.from_name.clone(), name_style)];
                if t.message_count > 1 {
                    from.push(Span::styled(format!(" ({})", t.message_count), Style::new().fg(MUTED)));
                }
                let mut subj = vec![];
                if t.starred {
                    subj.push(Span::styled("★ ", Style::new().fg(CRAYON)));
                }
                if t.needs_reply {
                    subj.push(Span::styled("Reply ", Style::new().fg(Color::Black).bg(CRAYON)));
                    subj.push(Span::raw(" "));
                }
                subj.push(Span::styled(t.subject.clone(), name_style));
                subj.push(Span::styled(format!(" — {}", t.snippet), Style::new().fg(MUTED)));
                Row::new(vec![
                    Cell::from(Line::from(vec![dot, Span::raw(" "), Span::styled(format!(" {initial} "), Style::new().fg(Color::Black).bg(color).bold())])),
                    Cell::from(Line::from(from)),
                    Cell::from(Line::from(subj)),
                    Cell::from(Line::from(Span::styled(fmt_date(t.date, false), Style::new().fg(MUTED))).alignment(Alignment::Right)),
                ])
            })
            .collect();
        if rows.is_empty() {
            let msg = if self.loading { "Loading…" } else { "Nothing here. ✏️" };
            f.render_widget(Paragraph::new(msg).fg(MUTED).alignment(Alignment::Center).block(Block::new().padding(Padding::top(area.height / 3))), area);
            return;
        }
        let table = Table::new(rows, [Constraint::Length(6), Constraint::Length(24), Constraint::Fill(1), Constraint::Length(8)])
            .row_highlight_style(Style::new().bg(Color::Rgb(38, 40, 52)))
            .column_spacing(1);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_reader(&mut self, f: &mut Frame, area: Rect) {
        let View::Reader { t, scroll, full } = &mut self.view else { return };
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(t.subject.clone(), Style::new().bold().fg(Color::White))),
            Line::from(Span::styled(
                format!("{} message(s) · {}", t.messages.len(), if *full { "full view (p: clean)" } else { "clean view (p: full)" }),
                Style::new().fg(MUTED),
            )),
            Line::raw(""),
        ];
        for (i, m) in t.messages.iter().enumerate() {
            let (name, email) = crate::gmail::split_address(&m.from);
            lines.push(Line::from(vec![
                Span::styled(name, Style::new().bold()),
                Span::styled(format!("  <{email}>"), Style::new().fg(MUTED)),
                Span::styled(format!("  {}", fmt_date(m.date, true)), Style::new().fg(MUTED)),
            ]));
            if *full {
                lines.push(Line::from(Span::styled(format!("to {}", m.to), Style::new().fg(MUTED))));
                if !m.cc.is_empty() {
                    lines.push(Line::from(Span::styled(format!("cc {}", m.cc), Style::new().fg(MUTED))));
                }
            }
            lines.push(Line::raw(""));
            let body = if *full { &m.body } else { &m.clean_body };
            for l in body.trim().lines() {
                lines.push(Line::raw(l.to_string()));
            }
            for a in &m.attachments {
                lines.push(Line::from(Span::styled(format!("📎 {} ({} KB)", a.filename, a.size / 1024), Style::new().fg(ACCENT))));
            }
            if i + 1 < t.messages.len() {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled("─".repeat(40), Style::new().fg(Color::Rgb(60, 60, 66)))));
                lines.push(Line::raw(""));
            }
        }
        let width = area.width.min(100);
        let x = area.x + (area.width - width) / 2;
        let card = Rect { x, width, ..area };
        let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().padding(Padding::new(2, 2, 1, 0)));
        let max = para.line_count(card.width.saturating_sub(4)) as u16;
        *scroll = (*scroll).min(max.saturating_sub(card.height / 2));
        f.render_widget(para.scroll((*scroll, 0)), card);
    }

    fn draw_bottom(&self, f: &mut Frame, area: Rect) {
        let help = match self.view {
            View::List => "↵ open · r reply · T template · c compose · e archive · s star · u read · l label · / search · g refresh · ? help · q quit",
            View::Reader { .. } => "esc back · j/k scroll · r reply · T template · f forward · e archive · s star · l label · p full/clean · ? help",
        };
        let text = if self.status.is_empty() {
            Span::styled(format!(" {help}"), Style::new().fg(MUTED))
        } else if self.status.starts_with("Error") {
            Span::styled(format!(" {}", self.status), Style::new().fg(Color::Rgb(239, 108, 87)))
        } else {
            Span::styled(format!(" {}", self.status), Style::new().fg(CRAYON))
        };
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_overlay(&self, f: &mut Frame) {
        let area = f.area();
        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                let keys = [
                    ("Tab / 1-5", "switch tab (To do · Primary · Starred · Newsletters · All)"),
                    ("j k ↑ ↓", "move / scroll"),
                    ("↵  o", "open thread"),
                    ("esc", "back / clear search"),
                    ("c", "compose new mail in $EDITOR"),
                    ("r", "reply in $EDITOR"),
                    ("T", "reply from a template"),
                    ("f", "forward (in reader)"),
                    ("e", "archive"),
                    ("#", "trash"),
                    ("s", "star / unstar (to do)"),
                    ("u", "mark read / unread"),
                    ("l", "add label"),
                    ("/", "search (Gmail syntax)"),
                    ("p", "reader: clean ↔ full view"),
                    ("g", "refresh"),
                    ("q", "quit"),
                ];
                let lines: Vec<Line> = keys
                    .iter()
                    .map(|(k, d)| Line::from(vec![Span::styled(format!("{k:>10}  "), Style::new().fg(CRAYON).bold()), Span::raw(*d)]))
                    .collect();
                let r = centered(area, 70, lines.len() as u16 + 4);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(popup(" Keys — any key to close ")), r);
            }
            Overlay::Input { kind, buf } => {
                let title = match kind {
                    InputKind::Search => " Search (Gmail syntax: from:, is:unread, newer_than:7d…) ",
                    InputKind::Label => " Add label (created if new) ",
                };
                let r = centered(area, 70, 3);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(format!("{buf}▏")).block(popup(title)), r);
            }
            Overlay::Templates { items, sel, .. } => {
                let lines: Vec<Line> = items
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let style = if i == *sel { Style::new().fg(Color::Black).bg(CRAYON) } else { Style::new() };
                        let desc = if t.description.is_empty() { t.body.lines().next().unwrap_or("") } else { &t.description };
                        Line::from(vec![Span::styled(format!(" {:<20} ", t.name), style), Span::styled(format!(" {desc}"), Style::new().fg(MUTED))])
                    })
                    .collect();
                let r = centered(area, 72, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(popup(" Reply with template — ↵ choose, esc cancel ")), r);
            }
            Overlay::Confirm(p) => {
                let mut lines: Vec<Line> = p.preview.lines().take(18).map(|l| Line::raw(l.to_string())).collect();
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(" y ", Style::new().fg(Color::Black).bg(CRAYON).bold()),
                    Span::raw(" send   "),
                    Span::styled(" d ", Style::new().fg(Color::Black).bg(Color::Gray).bold()),
                    Span::raw(" save draft   "),
                    Span::styled(" e ", Style::new().fg(Color::Black).bg(Color::Gray).bold()),
                    Span::raw(" edit again   "),
                    Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Gray).bold()),
                    Span::raw(" discard"),
                ]));
                let r = centered(area, 80, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(popup(" Ready to send ")), r);
            }
        }
    }
}

fn popup(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(CRAYON))
        .title(Span::styled(title, Style::new().bold()))
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h }
}
