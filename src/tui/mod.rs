//! Interactive inbox: Tempo-style tabs + list + reader, $EDITOR for writing.

use crate::engine::{Actor, Engine, SendReq};
use crate::models::{Template, ThreadDetail, ThreadSummary};
use crate::{edit_text, fmt_date};
use anyhow::Result;
mod theme;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::Theme;

const TABS: [&str; 5] = ["To do", "Primary", "Starred", "Newsletters", "All"];
/// Terminals at least this wide get the list + preview split.
const SPLIT_MIN_WIDTH: u16 = 120;
/// How long the selection has to rest before the preview is fetched.
const PREVIEW_DELAY: Duration = Duration::from_millis(150);

enum Msg {
    Inbox(Result<Vec<ThreadSummary>>),
    Searched(String, Result<Vec<ThreadSummary>>),
    Thread(Result<ThreadDetail>),
    /// Background fetch for the preview pane.
    Preview(String, Result<ThreadDetail>),
    /// An action finished; refresh the inbox afterwards when `reload`.
    Done { status: Result<String>, reload: bool },
    /// Browser sign-in for another account finished.
    SignedIn(Result<String>),
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

/// The in-app compose window.
struct ComposeForm {
    title: String,
    /// to, cc, bcc, subject, body
    fields: [String; 5],
    focus: usize,
    reply_to: Option<String>,
    armed_discard: bool,
}

const FIELD_NAMES: [&str; 5] = ["To", "Cc", "Bcc", "Subject", ""];

enum Overlay {
    None,
    Compose(ComposeForm),
    Help,
    Templates { items: Vec<Template>, sel: usize, reply_to: Option<String> },
    Input { kind: InputKind, buf: String },
    Accounts(AccountsList),
}

struct AccountsList {
    items: Vec<String>,
    sel: usize,
    default: Option<String>,
    /// First `x` arms, second signs out.
    armed_remove: bool,
}

/// What needs the real terminal (editor) — handled by the main loop.
enum EditorJob {
    Compose { req: SendReq, template: Option<Template> },
}

struct App {
    e: Arc<Engine>,
    me: String,
    tx: Sender<(u64, Msg)>,
    /// Bumped on account switch so replies for the old account are dropped.
    epoch: u64,
    /// A browser sign-in is waiting on Google.
    signing_in: bool,
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
    th: Theme,
    /// Threads already fetched, for the preview pane and instant opening.
    cache: HashMap<String, ThreadDetail>,
    fetching: HashSet<String>,
    preview: Preview,
    /// Where things were last drawn, for mouse hits.
    hits: Hits,
}

#[derive(Default)]
struct Preview {
    id: Option<String>,
    since: Option<Instant>,
    scroll: u16,
}

#[derive(Default)]
struct Hits {
    split: bool,
    tabs: Vec<(u16, u16, usize)>,
    account: Rect,
    list: Rect,
    row_height: u16,
    preview: Rect,
    last_click: Option<(usize, Instant)>,
}

pub fn run(engine: Engine) -> Result<()> {
    let me = engine.account()?;
    let e = Arc::new(engine);
    let (tx, rx) = channel();
    let mut app = App {
        e,
        me,
        tx,
        epoch: 0,
        signing_in: false,
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
        th: Theme::load(),
        cache: HashMap::new(),
        fetching: HashSet::new(),
        preview: Preview::default(),
        hits: Hits::default(),
    };
    app.reload();

    let mut term = ratatui::init();
    let _ = std::io::stdout().execute(EnableMouseCapture);
    let res = main_loop(&mut term, &mut app, &rx);
    let _ = std::io::stdout().execute(DisableMouseCapture);
    ratatui::restore();
    res
}

fn main_loop(term: &mut ratatui::DefaultTerminal, app: &mut App, rx: &Receiver<(u64, Msg)>) -> Result<()> {
    let mut theme_checked = Instant::now();
    while !app.quit {
        while let Ok((epoch, m)) = rx.try_recv() {
            if epoch == app.epoch || matches!(m, Msg::SignedIn(_)) {
                app.on_msg(m);
            }
        }
        if theme_checked.elapsed() > Duration::from_secs(2) {
            theme_checked = Instant::now();
            if app.th.refresh() {
                term.clear()?;
            }
        }
        app.sync_preview();
        term.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
        }
        if let Some(job) = app.job.take() {
            // Hand the terminal to $EDITOR, then come back.
            std::io::stdout().execute(DisableMouseCapture)?;
            disable_raw_mode()?;
            std::io::stdout().execute(LeaveAlternateScreen)?;
            let edited = app.run_editor(job);
            std::io::stdout().execute(EnterAlternateScreen)?;
            enable_raw_mode()?;
            std::io::stdout().execute(EnableMouseCapture)?;
            term.clear()?;
            match edited {
                Ok(Some(p)) => {
                    let title = if p.reply_to_thread.is_some() { " Reply " } else { " New message " };
                    let r = p;
                    app.overlay = Overlay::Compose(ComposeForm {
                        title: title.into(),
                        fields: [r.to, r.cc, r.bcc, r.subject, r.body],
                        focus: 4,
                        reply_to: r.reply_to_thread,
                        armed_discard: false,
                    });
                }
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
        let epoch = self.epoch;
        std::thread::spawn(move || {
            let _ = tx.send((epoch, f(&e)));
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
                        // Drop cached threads that went away or grew new messages.
                        self.cache.retain(|id, t| ts.iter().any(|s| &s.id == id && s.message_count == t.messages.len()));
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
                        self.cache.insert(t.id.clone(), t.clone());
                        self.view = View::Reader { t, scroll: 0, full: false };
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Preview(id, r) => {
                self.fetching.remove(&id);
                if let Ok(t) = r {
                    self.cache.insert(id, t);
                }
            }
            Msg::SignedIn(r) => {
                self.signing_in = false;
                match r {
                    Ok(acct) => {
                        self.switch_account(&acct);
                        self.status = format!("✓ Signed in as {acct}");
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Done { status, reload } => {
                match status {
                    Ok(s) if s.is_empty() => {}
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
            Overlay::Compose(form) => {
                self.compose_key(form, k);
                return;
            }
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
            Overlay::Accounts(list) => {
                self.accounts_key(list, k);
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
                            self.open_compose(req, Some(t));
                        }
                    }
                    _ => self.overlay = Overlay::Templates { items, sel, reply_to },
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
                self.open_compose(SendReq::default(), None);
                return;
            }
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input { kind: InputKind::Search, buf: String::new() };
                return;
            }
            KeyCode::Char('a') => {
                self.open_accounts();
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
                    self.open_compose(SendReq { reply_to_thread: Some(id), ..Default::default() }, None);
                    return;
                }
                KeyCode::Char('T') => {
                    match self.e.templates(&Actor::User) {
                        Ok(items) if items.is_empty() => {
                            self.status = "No templates yet — `sunbox templates set <name>` or ask an agent".into()
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
                        self.open_compose(req, None);
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
                KeyCode::Char('J') => self.preview.scroll = self.preview.scroll.saturating_add(3),
                KeyCode::Char('K') => self.preview.scroll = self.preview.scroll.saturating_sub(3),
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::Char('G') | KeyCode::End => self.move_sel(i32::MAX / 2),
                KeyCode::Home => self.table.select(Some(0)),
                KeyCode::Tab | KeyCode::Right => self.set_tab((self.tab + 1) % TABS.len()),
                KeyCode::BackTab | KeyCode::Left => self.set_tab((self.tab + TABS.len() - 1) % TABS.len()),
                KeyCode::Char(c @ '1'..='5') => self.set_tab(c as usize - '1' as usize),
                KeyCode::Enter | KeyCode::Char('o') => {
                    if let Some(t) = sel {
                        self.open_thread(t);
                    }
                }
                _ => {}
            },
        }
    }

    fn open_accounts(&mut self) {
        match (self.e.accounts(), self.e.default_account()) {
            (Ok(items), Ok(default)) => {
                let sel = items.iter().position(|a| a == &self.me).unwrap_or(0);
                self.overlay = Overlay::Accounts(AccountsList { items, sel, default, armed_remove: false });
            }
            (Err(e), _) | (_, Err(e)) => self.status = format!("Error: {e:#}"),
        }
    }

    fn accounts_key(&mut self, mut list: AccountsList, k: KeyEvent) {
        if k.code != KeyCode::Char('x') {
            list.armed_remove = false;
        }
        let picked = list.items.get(list.sel).cloned();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Down | KeyCode::Char('j') => list.sel = (list.sel + 1).min(list.items.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => list.sel = list.sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(a) = picked {
                    if a != self.me {
                        self.switch_account(&a);
                    }
                }
                return;
            }
            KeyCode::Char('n') | KeyCode::Char('+') => {
                if self.signing_in {
                    self.status = "Already waiting on a sign-in in your browser".into();
                    return;
                }
                self.signing_in = true;
                self.status = "Finish signing in in your browser — pick the account to add…".into();
                self.spawn(|e| Msg::SignedIn(e.login(true, true)));
                return;
            }
            KeyCode::Char('*') | KeyCode::Char('d') => {
                if let Some(a) = picked {
                    match self.e.set_default_account(&a) {
                        Ok(a) => {
                            self.status = format!("✓ {a} is now the default");
                            list.default = Some(a);
                        }
                        Err(e) => self.status = format!("Error: {e:#}"),
                    }
                }
            }
            KeyCode::Char('x') => {
                let Some(a) = picked else { return };
                if !list.armed_remove {
                    list.armed_remove = true;
                    self.status = format!("Press x again to sign out {a}");
                } else {
                    match self.e.logout(Some(&a)) {
                        Ok(gone) => {
                            self.status = format!("✓ Signed out {gone}");
                            if gone == self.me {
                                match self.e.default_account() {
                                    Ok(Some(next)) => self.switch_account(&next),
                                    _ => {
                                        self.status = "Signed out of every account — `sunbox auth login` to come back".into();
                                        self.quit = true;
                                    }
                                }
                                return;
                            }
                            self.open_accounts();
                            return;
                        }
                        Err(e) => self.status = format!("Error: {e:#}"),
                    }
                    list.armed_remove = false;
                }
            }
            _ => {}
        }
        self.overlay = Overlay::Accounts(list);
    }

    /// Point everything at another account and start over on its inbox.
    fn switch_account(&mut self, account: &str) {
        match self.e.for_account(account) {
            Ok(e) => {
                self.e = Arc::new(e);
                self.me = account.to_string();
                self.epoch += 1;
                self.inbox.clear();
                self.search = None;
                self.cache.clear();
                self.fetching.clear();
                self.preview = Preview::default();
                self.view = View::List;
                self.overlay = Overlay::None;
                self.table.select(Some(0));
                self.status = format!("Switched to {account}");
                self.reload();
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    fn open_thread(&mut self, t: ThreadSummary) {
        let id = t.id.clone();
        if let Some(detail) = self.cache.get(&id) {
            self.view = View::Reader { t: detail.clone(), scroll: 0, full: false };
            if t.unread {
                self.spawn(move |e| Msg::Done { status: e.mark_read(&Actor::User, &id, true).map(|_| String::new()), reload: false });
            }
        } else {
            self.loading = true;
            self.status = "Opening…".into();
            self.spawn(move |e| {
                let r = e.thread(&Actor::User, &id);
                if r.is_ok() && t.unread {
                    let _ = e.mark_read(&Actor::User, &id, true);
                }
                Msg::Thread(r)
            });
        }
        if t.unread {
            if let Some(x) = self.inbox.iter_mut().chain(self.search.iter_mut().flat_map(|s| s.1.iter_mut())).find(|x| x.id == t.id) {
                x.unread = false;
            }
        }
    }

    /// Follow the selection with the preview pane, fetching once the cursor settles.
    fn sync_preview(&mut self) {
        if !self.hits.split || !matches!(self.view, View::List) {
            return;
        }
        let id = self.selected().map(|t| t.id);
        if id != self.preview.id {
            self.preview = Preview { id, since: Some(Instant::now()), scroll: 0 };
            return;
        }
        let Some(id) = id else { return };
        let settled = self.preview.since.is_some_and(|t| t.elapsed() >= PREVIEW_DELAY);
        if settled && !self.cache.contains_key(&id) && !self.fetching.contains(&id) {
            self.fetching.insert(id.clone());
            self.spawn(move |e| {
                let r = e.thread(&Actor::User, &id);
                Msg::Preview(id, r)
            });
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        match (&self.overlay, m.kind) {
            (Overlay::Help, MouseEventKind::Down(_)) => {
                self.overlay = Overlay::None;
                return;
            }
            (Overlay::None, _) => {}
            _ => return,
        }
        let reading = matches!(self.view, View::Reader { .. });
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if let View::Reader { scroll, .. } = &mut self.view {
                    *scroll = if down { scroll.saturating_add(3) } else { scroll.saturating_sub(3) };
                } else if self.hits.split && self.hits.preview.contains(at) {
                    let s = &mut self.preview.scroll;
                    *s = if down { s.saturating_add(3) } else { s.saturating_sub(3) };
                } else {
                    self.move_sel(if down { 1 } else { -1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) if !reading => {
                if self.hits.account.contains(at) {
                    self.open_accounts();
                    return;
                }
                if let Some(&(_, _, i)) = self.hits.tabs.iter().find(|(x0, x1, _)| m.row <= 1 && (*x0..*x1).contains(&m.column)) {
                    self.set_tab(i);
                    return;
                }
                let list = self.hits.list;
                if !list.contains(at) || self.hits.row_height == 0 {
                    return;
                }
                let i = self.table.offset() + ((m.row - list.y) / self.hits.row_height) as usize;
                if i >= self.visible().len() {
                    return;
                }
                let double = self.hits.last_click.is_some_and(|(j, t)| j == i && t.elapsed() < Duration::from_millis(400));
                self.table.select(Some(i));
                self.hits.last_click = Some((i, Instant::now()));
                if double {
                    self.hits.last_click = None;
                    if let Some(t) = self.selected() {
                        self.open_thread(t);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) if reading => self.leave_reader(),
            _ => {}
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

    /// Resolve recipients/subject/template so the composer shows the real message.
    fn prefill(&self, req: &SendReq) -> Result<(String, String, String, String)> {
        if req.reply_to_thread.is_none() && req.template.is_none() {
            return Ok((req.to.clone(), req.cc.clone(), req.subject.clone(), req.body.clone()));
        }
        let mut probe = req.clone();
        if probe.body.is_empty() && probe.template.is_none() {
            probe.body = " ".into();
        }
        if probe.to.is_empty() && probe.reply_to_thread.is_none() {
            probe.to = "recipient@example.com".into();
        }
        let c = self.e.compose(&probe)?;
        let to = if req.to.is_empty() && req.reply_to_thread.is_none() { String::new() } else { c.to };
        Ok((to, c.cc, c.subject, c.body.trim().to_string()))
    }

    fn open_compose(&mut self, mut req: SendReq, template: Option<Template>) {
        if let Some(t) = &template {
            req.template = Some(t.name.clone());
        }
        let title = match (&req.reply_to_thread, &template, req.subject.starts_with("Fwd:")) {
            (_, Some(t), _) => format!(" Reply · template “{}” ", t.name),
            (Some(_), None, _) => " Reply ".to_string(),
            (None, None, true) => " Forward ".to_string(),
            _ => " New message ".to_string(),
        };
        match self.prefill(&req) {
            Ok((to, cc, subject, body)) => {
                let focus = if to.is_empty() { 0 } else if subject.is_empty() { 3 } else { 4 };
                self.overlay = Overlay::Compose(ComposeForm {
                    title,
                    fields: [to, cc, String::new(), subject, body],
                    focus,
                    reply_to: req.reply_to_thread,
                    armed_discard: false,
                });
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    fn compose_key(&mut self, mut form: ComposeForm, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let req = |f: &ComposeForm| SendReq {
            to: f.fields[0].trim().to_string(),
            cc: f.fields[1].trim().to_string(),
            bcc: f.fields[2].trim().to_string(),
            subject: f.fields[3].trim().to_string(),
            body: f.fields[4].clone(),
            reply_to_thread: f.reply_to.clone(),
            ..Default::default()
        };
        if k.code != KeyCode::Esc {
            form.armed_discard = false;
        }
        match k.code {
            KeyCode::Char('s') if ctrl => {
                if form.fields[0].trim().is_empty() || form.fields[4].trim().is_empty() {
                    self.status = "Need a recipient and a message before sending".into();
                    self.overlay = Overlay::Compose(form);
                    return;
                }
                self.status = "Sending…".into();
                let r = req(&form);
                self.spawn(move |e| Msg::Done { status: e.send(&Actor::User, &r).map(|_| "Sent".into()), reload: true });
            }
            KeyCode::Char('d') if ctrl => {
                self.status = "Saving draft…".into();
                let r = req(&form);
                self.spawn(move |e| Msg::Done { status: e.draft(&Actor::User, &r).map(|_| "Draft saved".into()), reload: false });
            }
            KeyCode::Char('e') if ctrl => {
                self.job = Some(EditorJob::Compose { req: req(&form), template: None });
            }
            KeyCode::Esc => {
                let empty = form.fields[4].trim().is_empty();
                if empty || form.armed_discard {
                    self.status = "Discarded".into();
                } else {
                    form.armed_discard = true;
                    self.status = "Press Esc again to discard, Ctrl+D to save a draft".into();
                    self.overlay = Overlay::Compose(form);
                }
                return;
            }
            KeyCode::Tab | KeyCode::Down if form.focus < 4 || k.code == KeyCode::Tab => {
                form.focus = (form.focus + 1) % 5;
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::BackTab | KeyCode::Up if form.focus < 4 || k.code == KeyCode::BackTab => {
                form.focus = (form.focus + 4) % 5;
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Enter => {
                if form.focus == 4 {
                    form.fields[4].push('\n');
                } else {
                    form.focus += 1;
                }
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Backspace => {
                if ctrl {
                    let f = &mut form.fields[form.focus];
                    while f.ends_with(char::is_whitespace) {
                        f.pop();
                    }
                    while f.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        f.pop();
                    }
                } else {
                    form.fields[form.focus].pop();
                }
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Char(c) if !ctrl => {
                form.fields[form.focus].push(c);
                self.overlay = Overlay::Compose(form);
            }
            _ => self.overlay = Overlay::Compose(form),
        }
    }

    fn draw_compose(&self, f: &mut Frame, form: &ComposeForm) {
        let area = f.area();
        let w = area.width.saturating_sub(4).min(100);
        let h = area.height.saturating_sub(2);
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + 1, width: w, height: h };
        f.render_widget(Clear, r);
        let block = self.th.popup(&form.title).title_bottom(
            Line::from(" Ctrl+S send · Ctrl+D save draft · Ctrl+E open in $EDITOR · Tab next field · Esc discard ")
                .fg(self.th.muted),
        );
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [hdr, sep, body] =
            Layout::vertical([Constraint::Length(4), Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        let cursor = Span::styled("▏", Style::new().fg(self.th.sun));
        let header: Vec<Line> = (0..4)
            .map(|i| {
                let focused = form.focus == i;
                let label = Span::styled(
                    format!("{:>8}  ", FIELD_NAMES[i]),
                    if focused { Style::new().fg(self.th.sun).bold() } else { Style::new().fg(self.th.muted) },
                );
                let mut l = vec![label, Span::raw(form.fields[i].clone())];
                if focused {
                    l.push(cursor.clone());
                }
                Line::from(l)
            })
            .collect();
        f.render_widget(Paragraph::new(header), hdr);
        f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).fg(self.th.rule), sep);
        let mut lines: Vec<Line> = form.fields[4].split('\n').map(|l| Line::raw(l.to_string())).collect();
        if form.focus == 4 {
            if let Some(last) = lines.last_mut() {
                last.spans.push(cursor);
            }
        }
        if form.fields[4].is_empty() && form.focus != 4 {
            lines = vec![Line::from(Span::styled("Write your message…", Style::new().fg(self.th.muted)))];
        }
        let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().padding(Padding::horizontal(1)));
        let total = para.line_count(body.width.saturating_sub(2)) as u16;
        // Keep the cursor (end of body) in view while typing.
        let scroll = if form.focus == 4 { total.saturating_sub(body.height) } else { 0 };
        f.render_widget(para.scroll((scroll, 0)), body);
    }

    // ---------- editor ----------

    fn run_editor(&mut self, job: EditorJob) -> Result<Option<SendReq>> {
        let EditorJob::Compose { mut req, template } = job;
        if let Some(t) = &template {
            req.template = Some(t.name.clone());
        }
        let (to, cc, subject, body) = self.prefill(&req)?;

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
        Ok(Some(out))
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        let [top, main, bottom] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        self.draw_top(f, top);
        match &self.view {
            View::List => {
                self.hits.split = main.width >= SPLIT_MIN_WIDTH;
                if self.hits.split {
                    let list_w = (main.width * 2 / 5).clamp(52, 72);
                    let [l, r] = Layout::horizontal([Constraint::Length(list_w), Constraint::Fill(1)]).areas(main);
                    self.draw_list(f, l);
                    self.draw_preview(f, r);
                } else {
                    self.hits.preview = Rect::default();
                    self.draw_list(f, main);
                }
            }
            View::Reader { .. } => self.draw_reader(f, main),
        }
        self.draw_bottom(f, bottom);
        self.draw_overlay(f);
    }

    fn draw_top(&mut self, f: &mut Frame, area: Rect) {
        let counts = self.counts();
        let rule = Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(self.th.rule));
        let mut spans = vec![
            Span::styled(" ▌", Style::new().fg(self.th.sun)),
            Span::styled("Sunbox ", Style::new().fg(self.th.bright).bold()),
            Span::raw("  "),
        ];
        self.hits.tabs.clear();
        if let Some((q, ts)) = &self.search {
            spans.push(Span::styled(format!(" Search: {q} ({}) ", ts.len()), Style::new().fg(self.th.on_chip).bg(self.th.accent)));
        } else {
            let mut x = area.x + spans.iter().map(|s| s.width() as u16).sum::<u16>();
            for (i, name) in TABS.iter().enumerate() {
                let active = i == self.tab;
                let name_style = if active { Style::new().fg(self.th.accent).bold() } else { Style::new().fg(self.th.muted) };
                let count_style = if active { Style::new().fg(self.th.on_chip).bg(self.th.accent) } else { Style::new().fg(self.th.muted) };
                let tab = [Span::styled(format!(" {name} "), name_style), Span::styled(format!(" {} ", counts[i]), count_style), Span::raw(" ")];
                let w: u16 = tab.iter().map(|s| s.width() as u16).sum();
                self.hits.tabs.push((x, x + w, i));
                x += w;
                spans.extend(tab);
            }
        }
        // The account name gives way to the tabs on narrow terminals (`a` still shows it).
        let me = if area.width >= 120 { self.me.as_str() } else { "" };
        let right = Line::from(vec![
            Span::styled(if self.loading { "⟳ " } else { "" }, Style::new().fg(self.th.muted)),
            Span::styled(me, Style::new().fg(self.th.muted)),
            Span::styled(" ⇄ ", Style::new().fg(self.th.accent)),
        ]);
        let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right.width() as u16)]).areas(area);
        self.hits.account = r;
        f.render_widget(Paragraph::new(Line::from(spans)).block(rule.clone()), l);
        f.render_widget(Paragraph::new(right).block(rule), r);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        self.hits.list = area;
        let split = self.hits.split;
        let th = &self.th;
        let rows: Vec<Row> = self
            .visible()
            .iter()
            .map(|t| {
                let initial = t.from_name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
                let bar = if t.unread { Span::styled("▌", Style::new().fg(th.accent)) } else { Span::raw(" ") };
                let avatar = Span::styled(format!(" {initial} "), Style::new().fg(th.on_chip).bg(th.avatar(&t.from_email)).bold());
                let (name_style, subj_style) = if t.unread {
                    (Style::new().fg(th.bright).bold(), Style::new().fg(th.bright).bold())
                } else {
                    (Style::new().fg(th.fg), Style::new().fg(th.fg))
                };
                let mut from = vec![Span::styled(t.from_name.clone(), name_style)];
                if t.message_count > 1 {
                    from.push(Span::styled(format!(" {}", t.message_count), Style::new().fg(th.muted)));
                }
                let mut subj = vec![];
                if t.starred {
                    subj.push(Span::styled("★ ", Style::new().fg(th.sun)));
                }
                if t.needs_reply {
                    subj.push(Span::styled(" reply ", Style::new().fg(th.on_chip).bg(th.sun)));
                    subj.push(Span::raw(" "));
                }
                subj.push(Span::styled(t.subject.clone(), subj_style));
                let date = Span::styled(fmt_date(t.date, false), Style::new().fg(if t.unread { th.accent } else { th.muted }));
                if split {
                    // Two-line card: sender + date, then subject, then a muted snippet.
                    Row::new(vec![
                        Cell::from(Text::from(vec![Line::from(vec![bar.clone(), Span::raw(" "), avatar]), Line::from(bar.clone()), Line::from(bar)])),
                        Cell::from(Text::from(vec![
                            Line::from(from),
                            Line::from(subj),
                            Line::from(Span::styled(t.snippet.clone(), Style::new().fg(th.muted))),
                        ])),
                        Cell::from(Line::from(date).alignment(Alignment::Right)),
                    ])
                    .height(3)
                    .bottom_margin(1)
                } else {
                    subj.push(Span::styled(format!(" — {}", t.snippet), Style::new().fg(th.muted)));
                    Row::new(vec![
                        Cell::from(Line::from(vec![bar, Span::raw(" "), avatar])),
                        Cell::from(Line::from(from)),
                        Cell::from(Line::from(subj)),
                        Cell::from(Line::from(date).alignment(Alignment::Right)),
                    ])
                }
            })
            .collect();
        self.hits.row_height = if split { 4 } else { 1 };
        if rows.is_empty() {
            let msg = if self.loading { "Loading…" } else { "Nothing here. ✏️" };
            f.render_widget(Paragraph::new(msg).fg(th.muted).alignment(Alignment::Center).block(Block::new().padding(Padding::top(area.height / 3))), area);
            return;
        }
        let widths: &[Constraint] = if split {
            &[Constraint::Length(5), Constraint::Fill(1), Constraint::Length(8)]
        } else {
            &[Constraint::Length(5), Constraint::Length(24), Constraint::Fill(1), Constraint::Length(8)]
        };
        let table = Table::new(rows, widths.to_vec())
            .row_highlight_style(Style::new().bg(th.selection))
            .column_spacing(1);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    /// The conversation as styled lines, shared by the reader and the preview pane.
    fn thread_lines(&self, t: &ThreadDetail, full: bool, hint: bool) -> Vec<Line<'static>> {
        let th = &self.th;
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(t.subject.clone(), Style::new().bold().fg(th.bright)))];
        let mut meta = format!("{} message{}", t.messages.len(), if t.messages.len() == 1 { "" } else { "s" });
        if hint {
            meta.push_str(if full { " · full view (p: clean)" } else { " · clean view (p: full)" });
        }
        lines.push(Line::from(Span::styled(meta, Style::new().fg(th.muted))));
        lines.push(Line::raw(""));
        for (i, m) in t.messages.iter().enumerate() {
            let (name, email) = crate::gmail::split_address(&m.from);
            let initial = name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
            lines.push(Line::from(vec![
                Span::styled(format!(" {initial} "), Style::new().fg(th.on_chip).bg(th.avatar(&email)).bold()),
                Span::raw(" "),
                Span::styled(name, Style::new().fg(th.bright).bold()),
                Span::styled(format!("  {email}"), Style::new().fg(th.muted)),
            ]));
            lines.push(Line::from(Span::styled(format!("    {}", fmt_date(m.date, true)), Style::new().fg(th.muted))));
            if full {
                lines.push(Line::from(Span::styled(format!("    to {}", m.to), Style::new().fg(th.muted))));
                if !m.cc.is_empty() {
                    lines.push(Line::from(Span::styled(format!("    cc {}", m.cc), Style::new().fg(th.muted))));
                }
            }
            lines.push(Line::raw(""));
            let body = if full { &m.body } else { &m.clean_body };
            for l in body.trim().lines() {
                // Quoted text that survives cleaning reads as an aside.
                if l.starts_with('>') {
                    lines.push(Line::from(Span::styled(l.to_string(), Style::new().fg(th.muted).italic())));
                } else {
                    lines.push(Line::from(Span::styled(l.to_string(), Style::new().fg(th.fg))));
                }
            }
            if !m.attachments.is_empty() {
                lines.push(Line::raw(""));
            }
            for a in &m.attachments {
                lines.push(Line::from(vec![
                    Span::styled(" 📎 ", Style::new().fg(th.accent)),
                    Span::styled(a.filename.clone(), Style::new().fg(th.accent)),
                    Span::styled(format!("  {} KB", a.size / 1024), Style::new().fg(th.muted)),
                ]));
            }
            if i + 1 < t.messages.len() {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled("┄".repeat(40), Style::new().fg(th.rule))));
                lines.push(Line::raw(""));
            }
        }
        lines
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        self.hits.preview = area;
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(self.th.rule))
            .padding(Padding::new(3, 3, 1, 0));
        let id = self.preview.id.clone();
        let Some(t) = id.as_ref().and_then(|id| self.cache.get(id)) else {
            let msg = if id.is_some() { "…" } else { "" };
            f.render_widget(Paragraph::new(msg).fg(self.th.muted).block(block), area);
            return;
        };
        let lines = self.thread_lines(t, false, false);
        let inner = block.inner(area);
        let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(block);
        let max = para.line_count(inner.width) as u16;
        self.preview.scroll = self.preview.scroll.min(max.saturating_sub(inner.height / 2));
        f.render_widget(para.scroll((self.preview.scroll, 0)), area);
    }

    fn draw_reader(&mut self, f: &mut Frame, area: Rect) {
        let View::Reader { t, full, .. } = &self.view else { return };
        let lines = self.thread_lines(t, *full, true);
        let width = area.width.min(100);
        let card = Rect { x: area.x + (area.width - width) / 2, width, ..area };
        let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().padding(Padding::new(2, 2, 1, 0)));
        let max = para.line_count(card.width.saturating_sub(4)) as u16;
        let View::Reader { scroll, .. } = &mut self.view else { return };
        *scroll = (*scroll).min(max.saturating_sub(card.height / 2));
        f.render_widget(para.scroll((*scroll, 0)), card);
    }
    fn draw_bottom(&self, f: &mut Frame, area: Rect) {
        let help = match self.view {
            View::List => "c compose · ↵ open · r reply · T template · e archive · s star · u read · l label · / search · a accounts · ? help · q quit",
            View::Reader { .. } => "esc back · j/k scroll · r reply · T template · f forward · e archive · s star · l label · p full/clean · ? help",
        };
        let text = if self.status.is_empty() {
            Span::styled(format!(" {help}"), Style::new().fg(self.th.muted))
        } else if self.status.starts_with("Error") {
            Span::styled(format!(" {}", self.status), Style::new().fg(self.th.error))
        } else {
            Span::styled(format!(" {}", self.status), Style::new().fg(self.th.sun))
        };
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_overlay(&self, f: &mut Frame) {
        let area = f.area();
        match &self.overlay {
            Overlay::None => {}
            Overlay::Compose(form) => self.draw_compose(f, form),
            Overlay::Help => {
                let keys = [
                    ("Tab / 1-5", "switch tab (To do · Primary · Starred · Newsletters · All)"),
                    ("j k ↑ ↓", "move / scroll"),
                    ("↵  o", "open thread"),
                    ("esc", "back / clear search"),
                    ("c", "compose new mail (Ctrl+S send · Ctrl+D draft · Ctrl+E $EDITOR)"),
                    ("r", "reply"),
                    ("T", "reply from a template"),
                    ("f", "forward (in reader)"),
                    ("e", "archive"),
                    ("#", "trash"),
                    ("s", "star / unstar (to do)"),
                    ("u", "mark read / unread"),
                    ("l", "add label"),
                    ("/", "search (Gmail syntax)"),
                    ("p", "reader: clean ↔ full view"),
                    ("J K", "scroll the preview pane (wide terminals)"),
                    ("mouse", "click to select, double-click to open, wheel scrolls, right-click back"),
                    ("a", "accounts: switch · add · default · sign out"),
                    ("g", "refresh"),
                    ("q", "quit"),
                ];
                let lines: Vec<Line> = keys
                    .iter()
                    .map(|(k, d)| Line::from(vec![Span::styled(format!("{k:>10}  "), Style::new().fg(self.th.sun).bold()), Span::raw(*d)]))
                    .collect();
                let r = centered(area, 70, lines.len() as u16 + 4);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(self.th.popup(" Keys — any key to close ")), r);
            }
            Overlay::Input { kind, buf } => {
                let title = match kind {
                    InputKind::Search => " Search (Gmail syntax: from:, is:unread, newer_than:7d…) ",
                    InputKind::Label => " Add label (created if new) ",
                };
                let r = centered(area, 70, 3);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(format!("{buf}▏")).block(self.th.popup(title)), r);
            }
            Overlay::Accounts(list) => {
                let mut lines: Vec<Line> = list
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let style = if i == list.sel { Style::new().fg(self.th.on_chip).bg(self.th.sun) } else { Style::new().fg(self.th.fg) };
                        let current = if *a == self.me { Span::styled(" ● ", Style::new().fg(self.th.accent)) } else { Span::raw("   ") };
                        let default = if list.default.as_deref() == Some(a.as_str()) { Span::styled("  default", Style::new().fg(self.th.muted)) } else { Span::raw("") };
                        Line::from(vec![current, Span::styled(format!(" {:<40} ", a), style), default])
                    })
                    .collect();
                lines.push(Line::raw(""));
                let hint = if self.signing_in {
                    "  waiting on your browser…".to_string()
                } else {
                    "  ↵ switch · n add account · d make default · x sign out".to_string()
                };
                lines.push(Line::from(Span::styled(hint, Style::new().fg(self.th.muted))));
                let r = centered(area, 64, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(self.th.popup(" Accounts — ● current ")), r);
            }
            Overlay::Templates { items, sel, .. } => {
                let lines: Vec<Line> = items
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let style = if i == *sel { Style::new().fg(self.th.on_chip).bg(self.th.sun) } else { Style::new() };
                        let desc = if t.description.is_empty() { t.body.lines().next().unwrap_or("") } else { &t.description };
                        Line::from(vec![Span::styled(format!(" {:<20} ", t.name), style), Span::styled(format!(" {desc}"), Style::new().fg(self.th.muted))])
                    })
                    .collect();
                let r = centered(area, 72, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(self.th.popup(" Reply with template — ↵ choose, esc cancel ")), r);
            }
        }
    }
}


fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h }
}
