//! Interactive inbox: Tempo-style tabs + list + reader, $EDITOR for writing.

use crate::engine::{Actor, Engine, SendReq};
use crate::actions::Outcome;
use crate::models::{AiDraft, Briefing, Contact, Person, SuggestedAction, Template, ThreadDetail, ThreadSummary};
use crate::{edit_text, fmt_date};
use anyhow::Result;
mod autocorrect;
mod claude;
mod form;
mod library;
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
use claude::ClaudeState;
use form::Form;
use library::{LibKind, Library};
use theme::Theme;

const TABS: [&str; 7] = ["To do", "Primary", "Starred", "Newsletters", "All", "Sent", "✦ Claude"];
/// Mail you sent (its own fetch, not a filter on the inbox).
const SENT_TAB: usize = 5;
/// The Claude panel's tab.
const CLAUDE_TAB: usize = 6;
/// Terminals at least this wide get the list + preview split.
const SPLIT_MIN_WIDTH: u16 = 120;
/// How long the selection has to rest before the preview is fetched.
const PREVIEW_DELAY: Duration = Duration::from_millis(150);
/// Setting that remembers which inboxes were shown.
const SHOWN_KEY: &str = "tui_shown_accounts";

/// One result per shown account.
type PerAccount = Vec<(String, Result<Vec<ThreadSummary>>)>;

enum Msg {
    Inbox(PerAccount),
    Sent(PerAccount),
    /// Claude wrote (or rewrote) the message in the composer.
    Composed { gen: u64, r: Result<crate::assistant::ComposeHelp> },
    /// Claude's spelling fixes for the composer text it was given.
    Corrected { gen: u64, checked: String, fixes: Result<Vec<crate::assistant::Fix>> },
    Searched(String, PerAccount),
    Thread(Result<ThreadDetail>),
    /// Background fetch for the preview pane.
    Preview(String, Result<ThreadDetail>),
    /// An action finished; refresh the inbox afterwards when `reload`.
    Done { status: Result<String>, reload: bool },
    /// Browser sign-in for another account finished.
    SignedIn(Result<String>),
    /// Claude finished ranking the inbox.
    Briefed(Result<Briefing>),
    /// Claude finished a reply draft for a thread.
    Drafted(String, Result<AiDraft>),
    /// Claude updated a client profile.
    Learned(Result<Contact>),
    /// Claude wrote a template (unsaved, opens in the editor).
    AiTemplate(Result<Template>),
    /// Claude read a whole thread and picked its buttons.
    Planned(String, Result<Vec<SuggestedAction>>),
    /// A suggested-action button finished.
    Acted { thread_id: String, account: String, idx: usize, r: Result<Outcome> },
}

impl Msg {
    /// Results that stay valid when the shown inboxes change.
    fn outlives_epoch(&self) -> bool {
        matches!(self, Msg::SignedIn(_) | Msg::Briefed(_) | Msg::Drafted(..) | Msg::Learned(_) | Msg::AiTemplate(_) | Msg::Planned(..) | Msg::Acted { .. })
    }
}

enum View {
    List,
    /// Templates or people: browse and edit.
    Library(Library),
    Reader { t: ThreadDetail, scroll: u16, full: bool },
}

#[derive(Clone)]
enum InputKind {
    Search,
    Label,
    /// Redraft a reply with an instruction for Claude.
    ClaudeNote { thread_id: String, account: String },
    /// Ask Claude for a new template, or to revise one.
    TemplateAsk { base: Option<Template> },
    /// First "create task" press: ask for the TickTick add-by-email address.
    TaskEmail { thread_id: String, account: String, idx: usize },
    /// "Handled another way": a note for Claude, then archive.
    HandledNote { thread_id: String, account: String },
}

/// The in-app compose window.
#[derive(Default)]
struct ComposeForm {
    title: String,
    /// to, cc, bcc, subject, body
    fields: [String; 5],
    focus: usize,
    reply_to: Option<String>,
    /// The account it goes out from.
    from: String,
    /// Set when this is a Claude draft being checked, so sending marks it done.
    ai_thread: Option<String>,
    armed_discard: bool,
    /// Address suggestions for the word being typed in To/Cc/Bcc.
    sugg: Vec<Person>,
    sugg_sel: usize,
    ac: Autocorrect,
    /// Ctrl+K: what to ask Claude to write (Some while the bar is open).
    ask: Option<String>,
    ask_busy: bool,
    ask_gen: u64,
    /// What Claude says to double-check in what it wrote.
    check: Vec<String>,
}

/// Autocorrect state for one message being written.
#[derive(Default)]
struct Autocorrect {
    /// Last keystroke in the body; Claude looks once typing pauses.
    typed: Option<Instant>,
    /// The text Claude last looked at (so a pause doesn't re-check the same words).
    checked: String,
    /// A check is out; `gen` tells its answer apart from stale ones.
    busy: bool,
    gen: u64,
    /// Body before and after the last correction, for Ctrl+Z.
    undo: Option<(String, String, Vec<String>)>,
    /// Fixes the user undid; never made again in this message.
    rejected: Vec<String>,
    /// Corrected words, highlighted briefly, and what to say about them.
    flash: Vec<String>,
    flash_at: Option<Instant>,
    note: String,
}

const FIELD_NAMES: [&str; 5] = ["To", "Cc", "Bcc", "Subject", ""];

enum Overlay {
    None,
    Compose(ComposeForm),
    Help,
    Templates { items: Vec<Template>, sel: usize, reply_to: Option<String>, from: String },
    Input { kind: InputKind, buf: String },
    Accounts(AccountsList),
    /// Editing a template or a contact profile.
    Form(Form),
    /// The buttons Claude suggested for one email.
    Actions { thread_id: String, account: String, items: Vec<SuggestedAction>, sel: usize },
    /// Contacts search: type to filter, ↵ writes to them.
    People { query: String, items: Vec<Person>, sel: usize },
}

struct AccountsList {
    items: Vec<String>,
    /// Which inboxes are ticked (shown together).
    ticked: Vec<bool>,
    sel: usize,
    default: Option<String>,
    /// First `x` arms, second signs out.
    armed_remove: bool,
}

/// What needs the real terminal (editor) — handled by the main loop.
enum EditorJob {
    Compose { req: SendReq, template: Option<Template>, from: String, ai_thread: Option<String> },
    /// Edit the focused field of a form.
    Field(Form),
}

struct App {
    /// The primary shown account: new mail goes out from here.
    e: Arc<Engine>,
    me: String,
    /// One engine per signed-in account.
    engines: HashMap<String, Arc<Engine>>,
    /// Accounts whose inboxes are merged into the list, primary first.
    shown: Vec<String>,
    tx: Sender<(u64, Msg)>,
    /// Bumped on account switch so replies for the old account are dropped.
    epoch: u64,
    /// A browser sign-in is waiting on Google.
    signing_in: bool,
    inbox: Vec<ThreadSummary>,
    sent: Vec<ThreadSummary>,
    /// Sent is fetched the first time its tab opens, then kept fresh with the inbox.
    sent_loaded: bool,
    sent_loading: bool,
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
    claude: ClaudeState,
    /// Autocorrect while composing (Ctrl+G toggles; `[claude] autocorrect` sets the default).
    autocorrect_on: bool,
    ac_gen: u64,
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
        engines: HashMap::new(),
        shown: vec![],
        tx,
        epoch: 0,
        signing_in: false,
        inbox: vec![],
        sent: vec![],
        sent_loaded: false,
        sent_loading: false,
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
        claude: ClaudeState::default(),
        autocorrect_on: false,
        ac_gen: 0,
    };
    app.autocorrect_on = app.e.cfg.claude.autocorrect;
    app.load_ai();
    app.refresh_engines();
    // Bring back the inboxes shown last time, unless an account was asked for explicitly.
    let saved: Vec<String> = app
        .e
        .setting(SHOWN_KEY)
        .ok()
        .flatten()
        .map(|v| v.split(',').filter(|a| app.engines.contains_key(*a)).map(String::from).collect())
        .unwrap_or_default();
    let explicit = std::env::var("SUNBOX_ACCOUNT").is_ok() || app.e.default_account().ok().flatten().as_deref() != Some(app.me.as_str());
    let shown = if saved.is_empty() || explicit { vec![app.me.clone()] } else { saved };
    app.set_shown(shown, false);
    // Keep each account's address book fresh (quietly, in the background).
    for e in app.engines.values().filter(|e| e.people_stale(12)) {
        app.spawn_on(e.clone(), |e| {
            let _ = e.sync_people(&Actor::User);
            Msg::Done { status: Ok(String::new()), reload: false }
        });
    }

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
            if epoch == app.epoch || m.outlives_epoch() {
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
        app.sync_autocorrect();
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
            app.run_job(job);
            std::io::stdout().execute(EnterAlternateScreen)?;
            enable_raw_mode()?;
            std::io::stdout().execute(EnableMouseCapture)?;
            term.clear()?;
        }
    }
    Ok(())
}

impl App {
    fn spawn(&self, f: impl FnOnce(&Engine) -> Msg + Send + 'static) {
        self.spawn_on(self.e.clone(), f);
    }

    /// Run `f` in the background as a particular account.
    fn spawn_on(&self, e: Arc<Engine>, f: impl FnOnce(&Engine) -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        let epoch = self.epoch;
        std::thread::spawn(move || {
            let _ = tx.send((epoch, f(&e)));
        });
    }

    /// The engine for a thread's account (the primary one if unknown).
    fn eng(&self, account: &str) -> Arc<Engine> {
        self.engines.get(account).cloned().unwrap_or_else(|| self.e.clone())
    }

    fn refresh_engines(&mut self) {
        let accounts = self.e.accounts().unwrap_or_default();
        self.engines.retain(|a, _| accounts.contains(a));
        for a in accounts {
            if !self.engines.contains_key(&a) {
                if let Ok(e) = self.e.for_account(&a) {
                    self.engines.insert(a, Arc::new(e));
                }
            }
        }
    }

    /// Fetch `query` from every shown inbox at once.
    fn spawn_search(&self, query: String, max: usize, done: impl FnOnce(PerAccount) -> Msg + Send + 'static) {
        let engines: Vec<(String, Arc<Engine>)> = self.shown.iter().map(|a| (a.clone(), self.eng(a))).collect();
        self.spawn(move |_| {
            let per = std::thread::scope(|sc| {
                let handles: Vec<_> = engines
                    .iter()
                    .map(|(a, e)| {
                        let q = &query;
                        (a.clone(), sc.spawn(move || e.search(&Actor::User, q, max)))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(a, h)| (a, h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("fetch crashed")))))
                    .collect()
            });
            done(per)
        });
    }

    fn reload(&mut self) {
        self.loading = true;
        self.spawn_search("in:inbox".into(), 60, Msg::Inbox);
        if self.sent_loaded || self.tab == SENT_TAB {
            self.reload_sent();
        }
    }

    fn reload_sent(&mut self) {
        if self.sent_loading {
            return;
        }
        self.sent_loading = true;
        self.spawn_search("in:sent".into(), 60, Msg::Sent);
    }

    fn do_action(&mut self, label: &'static str, t: &ThreadSummary, f: impl FnOnce(&Engine, &str) -> Result<()> + Send + 'static) {
        self.status = format!("{label}…");
        let id = t.id.clone();
        self.spawn_on(self.eng(&t.account), move |e| Msg::Done { status: f(e, &id).map(|_| label.to_string()), reload: true });
    }

    /// Show these inboxes (merged), the first one primary.
    fn set_shown(&mut self, mut shown: Vec<String>, announce: bool) {
        shown.dedup();
        if shown.is_empty() {
            return;
        }
        self.refresh_engines();
        shown.retain(|a| self.engines.contains_key(a));
        let Some(primary) = shown.first().cloned() else { return };
        self.e = self.eng(&primary);
        self.me = primary;
        let _ = self.e.set_setting(SHOWN_KEY, &shown.join(","));
        if announce {
            self.status = if shown.len() == 1 { format!("Showing {}", self.me) } else { format!("Showing {} inboxes", shown.len()) };
        }
        self.shown = shown;
        self.epoch += 1;
        self.inbox.clear();
        self.sent.clear();
        self.sent_loaded = false;
        self.sent_loading = false;
        self.search = None;
        self.cache.clear();
        self.fetching.clear();
        self.preview = Preview::default();
        self.view = View::List;
        self.overlay = Overlay::None;
        self.table.select(Some(0));
        self.reload();
    }

    /// Merge per-account results, newest first; report the first failure.
    fn merge(&mut self, per: PerAccount) -> Vec<ThreadSummary> {
        let mut all = vec![];
        let mut errs = vec![];
        for (a, r) in per {
            match r {
                Ok(ts) => all.extend(ts),
                Err(e) => errs.push(format!("{a}: {e:#}")),
            }
        }
        if let Some(e) = errs.first() {
            self.status = format!("Error: {e}");
        }
        all.sort_by(|a, b| b.date.cmp(&a.date));
        all
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Inbox(per) => {
                self.loading = false;
                if self.search.is_none() && self.status.ends_with('…') {
                    self.status.clear();
                }
                let ts = self.merge(per);
                // Drop cached threads that went away or grew new messages.
                let sent = &self.sent;
                self.cache.retain(|id, t| ts.iter().chain(sent).any(|s| &s.id == id && s.message_count == t.messages.len()));
                self.inbox = ts;
                // Land on a non-empty tab the first time.
                if self.visible().is_empty() && self.tab == 0 && !self.inbox.is_empty() {
                    self.tab = 1;
                }
                self.clamp();
                self.maybe_auto_brief();
            }
            Msg::Corrected { gen, checked, fixes } => self.on_corrected(gen, checked, fixes),
            Msg::Composed { gen, r } => self.on_composed(gen, r),
            Msg::Sent(per) => {
                self.sent_loading = false;
                self.sent_loaded = true;
                self.sent = self.merge(per);
                if self.tab == SENT_TAB {
                    self.clamp();
                }
            }
            Msg::Searched(q, per) => {
                self.loading = false;
                self.status.clear();
                let ts = self.merge(per);
                if self.status.is_empty() {
                    self.status = format!("{} result(s) for “{q}” — Esc to clear", ts.len());
                }
                self.search = Some((q, ts));
                self.table.select(Some(0));
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
                        // A new account joins the inboxes already on screen.
                        let mut shown = self.shown.clone();
                        if !shown.contains(&acct) {
                            shown.push(acct.clone());
                        }
                        self.set_shown(shown, false);
                        self.status = format!("✓ Signed in as {acct}");
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
            Msg::Briefed(r) => self.on_briefed(r),
            Msg::Drafted(id, r) => self.on_drafted(id, r),
            Msg::Learned(r) => self.on_learned(r),
            Msg::Planned(id, r) => self.on_planned(id, r),
            Msg::Acted { thread_id, account, idx, r } => self.on_acted(thread_id, account, idx, r),
            Msg::AiTemplate(r) => match r {
                Ok(t) => {
                    self.status = "✓ Claude wrote a template — check it, then Ctrl+S to save".into();
                    self.overlay = Overlay::Form(Form::template(Some(t), None));
                }
                Err(e) => self.status = format!("Error: {e:#}"),
            },
            Msg::Done { status, reload } => {
                match status {
                    Ok(s) if s.is_empty() => {}
                    Ok(s) => self.status = format!("✓ {s}"),
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
                self.load_ai_drafts();
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
        if self.tab == SENT_TAB {
            return self.sent.iter().collect();
        }
        self.inbox
            .iter()
            .filter(|t| match self.tab {
                0 => t.needs_reply,
                1 => !t.is_bulk,
                2 => t.starred,
                3 => t.is_bulk,
                4 => true,
                _ => false,
            })
            .collect()
    }

    fn counts(&self) -> [usize; 7] {
        let c = |f: fn(&ThreadSummary) -> bool| self.inbox.iter().filter(|t| f(t)).count();
        [c(|t| t.needs_reply), c(|t| !t.is_bulk), c(|t| t.starred), c(|t| t.is_bulk), self.inbox.len(), self.sent.len(), self.ready_drafts()]
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        let i = self.table.selected().unwrap_or(0).min(n.saturating_sub(1));
        self.table.select(Some(i));
    }

    fn selected(&self) -> Option<ThreadSummary> {
        if let View::Reader { t, .. } = &self.view {
            return self.inbox.iter().chain(&self.sent).chain(self.search.iter().flat_map(|s| s.1.iter())).find(|s| s.id == t.id).cloned()
                .or_else(|| Some(ThreadSummary {
                    id: t.id.clone(), subject: t.subject.clone(), from_name: String::new(), from_email: String::new(),
                    snippet: String::new(), date: 0, message_count: t.messages.len(), unread: false, starred: false,
                    needs_reply: false, is_bulk: false, labels: vec![], account: t.account.clone(), to: String::new(),
                }));
        }
        if matches!(self.view, View::List) && self.tab == CLAUDE_TAB && self.search.is_none() {
            return self.claude_selected_thread();
        }
        if matches!(self.view, View::Library(_)) {
            return None;
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
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.overlay = Overlay::Input { kind, buf: String::new() };
                    }
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
            Overlay::Form(form) => {
                self.form_key(form, k);
                return;
            }
            Overlay::Actions { thread_id, account, items, mut sel } => {
                let pick = match k.code {
                    KeyCode::Enter => Some(sel),
                    KeyCode::Char(c @ '1'..='9') => Some(c as usize - '1' as usize).filter(|i| *i < items.len()),
                    _ => None,
                };
                match (pick, k.code) {
                    (Some(i), _) => self.press_action(thread_id, account, i, items[i].clone()),
                    (None, KeyCode::Esc | KeyCode::Char('q')) => {}
                    (None, code) => {
                        match code {
                            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(items.len().saturating_sub(1)),
                            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                            _ => {}
                        }
                        self.overlay = Overlay::Actions { thread_id, account, items, sel };
                    }
                }
                return;
            }
            Overlay::People { mut query, mut items, mut sel } => {
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        if let Some(p) = items.get(sel) {
                            let req = SendReq { to: p.address(), ..Default::default() };
                            self.open_compose(req, None, self.me.clone());
                        }
                        return;
                    }
                    KeyCode::Down => sel = (sel + 1).min(items.len().saturating_sub(1)),
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Backspace => {
                        query.pop();
                        (items, sel) = (self.people(&self.me, &query, PEOPLE_ROWS), 0);
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        (items, sel) = (self.people(&self.me, &query, PEOPLE_ROWS), 0);
                    }
                    _ => {}
                }
                self.overlay = Overlay::People { query, items, sel };
                return;
            }
            Overlay::Templates { items, mut sel, reply_to, from } => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Down | KeyCode::Char('j') => {
                        sel = (sel + 1).min(items.len().saturating_sub(1));
                        self.overlay = Overlay::Templates { items, sel, reply_to, from };
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        sel = sel.saturating_sub(1);
                        self.overlay = Overlay::Templates { items, sel, reply_to, from };
                    }
                    KeyCode::Enter => {
                        if let Some(t) = items.get(sel).cloned() {
                            let req = SendReq { reply_to_thread: reply_to, ..Default::default() };
                            self.open_compose(req, Some(t), from);
                        }
                    }
                    _ => self.overlay = Overlay::Templates { items, sel, reply_to, from },
                }
                return;
            }
        }

        if let View::Library(_) = self.view {
            self.library_key(k);
            return;
        }
        if matches!(self.view, View::List) && self.tab == CLAUDE_TAB && self.search.is_none() && self.claude_key(k) {
            return;
        }

        // Keys shared by list + reader.
        let sel = self.selected();
        match k.code {
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                return;
            }
            KeyCode::Char('c') => {
                self.open_compose(SendReq::default(), None, self.me.clone());
                return;
            }
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input { kind: InputKind::Search, buf: String::new() };
                return;
            }
            KeyCode::Char('C') => {
                let items = self.people(&self.me, "", PEOPLE_ROWS);
                self.overlay = Overlay::People { query: String::new(), items, sel: 0 };
                return;
            }
            KeyCode::Char('a') => {
                self.open_accounts();
                return;
            }
            KeyCode::Char('t') if matches!(self.view, View::List) => {
                self.open_library(LibKind::Templates);
                return;
            }
            KeyCode::Char('P') if matches!(self.view, View::List) => {
                self.open_library(LibKind::People);
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
            let acct = if t.account.is_empty() { self.me.clone() } else { t.account.clone() };
            match k.code {
                KeyCode::Char('r') => {
                    self.open_compose(SendReq { reply_to_thread: Some(id), ..Default::default() }, None, acct);
                    return;
                }
                KeyCode::Char('T') => {
                    match self.e.templates(&Actor::User) {
                        Ok(items) if items.is_empty() => {
                            self.status = "No templates yet — `sunbox templates set <name>` or ask an agent".into()
                        }
                        Ok(items) => self.overlay = Overlay::Templates { items, sel: 0, reply_to: Some(id), from: acct },
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
                        self.open_compose(req, None, acct);
                    } else {
                        self.status = "Open the thread first (Enter), then f to forward".into();
                    }
                    return;
                }
                KeyCode::Char('e') => {
                    self.leave_reader();
                    self.do_action("Archived", t, |e, id| e.archive(&Actor::User, id));
                    return;
                }
                KeyCode::Char('#') => {
                    self.leave_reader();
                    self.do_action("Moved to trash", t, |e, id| e.trash(&Actor::User, id));
                    return;
                }
                KeyCode::Char('s') => {
                    let on = !t.starred;
                    self.do_action(if on { "Starred" } else { "Unstarred" }, t, move |e, id| e.star(&Actor::User, id, on));
                    return;
                }
                KeyCode::Char('u') => {
                    let read = t.unread;
                    self.do_action(if read { "Marked read" } else { "Marked unread" }, t, move |e, id| e.mark_read(&Actor::User, id, read));
                    return;
                }
                KeyCode::Char('l') => {
                    self.overlay = Overlay::Input { kind: InputKind::Label, buf: String::new() };
                    return;
                }
                KeyCode::Char('H') => {
                    self.overlay = Overlay::Input { kind: InputKind::HandledNote { thread_id: id, account: acct }, buf: String::new() };
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
            View::Library(_) => {}
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
                KeyCode::Char(c @ '1'..='7') => self.set_tab(c as usize - '1' as usize),
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
                let ticked = items.iter().map(|a| self.shown.contains(a)).collect();
                self.overlay = Overlay::Accounts(AccountsList { items, ticked, sel, default, armed_remove: false });
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
            // Space ticks inboxes to show together; Enter applies the ticks
            // (or, with only the cursor's account ticked, shows just that one).
            KeyCode::Char(' ') => {
                if let Some(t) = list.ticked.get_mut(list.sel) {
                    *t = !*t;
                }
            }
            KeyCode::Char('A') => {
                let all = list.ticked.iter().all(|t| *t);
                list.ticked.iter_mut().for_each(|t| *t = !all);
            }
            KeyCode::Enter => {
                let mut shown: Vec<String> =
                    list.items.iter().zip(&list.ticked).filter(|(_, t)| **t).map(|(a, _)| a.clone()).collect();
                if shown.is_empty() {
                    shown = picked.into_iter().collect();
                }
                // The cursor's account goes first, so it's the one new mail comes from.
                if let Some(p) = list.items.get(list.sel) {
                    if let Some(i) = shown.iter().position(|a| a == p) {
                        let p = shown.remove(i);
                        shown.insert(0, p);
                    }
                }
                if shown != self.shown {
                    self.set_shown(shown, true);
                }
                return;
            }
            KeyCode::Char('o') => {
                if let Some(a) = picked {
                    self.set_shown(vec![a], true);
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
                            self.refresh_engines();
                            let mut shown: Vec<String> = self.shown.iter().filter(|a| **a != gone).cloned().collect();
                            if shown.is_empty() {
                                match self.e.default_account() {
                                    Ok(Some(next)) => shown.push(next),
                                    _ => {
                                        self.status = "Signed out of every account — `sunbox auth login` to come back".into();
                                        self.quit = true;
                                        return;
                                    }
                                }
                            }
                            if shown != self.shown {
                                self.set_shown(shown, false);
                            }
                            self.status = format!("✓ Signed out {gone}");
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

    fn open_thread(&mut self, t: ThreadSummary) {
        let id = t.id.clone();
        if let Some(detail) = self.cache.get(&id) {
            self.view = View::Reader { t: detail.clone(), scroll: 0, full: false };
            if t.unread {
                self.spawn_on(self.eng(&t.account), move |e| Msg::Done { status: e.mark_read(&Actor::User, &id, true).map(|_| String::new()), reload: false });
            }
        } else {
            self.loading = true;
            self.status = "Opening…".into();
            self.spawn_on(self.eng(&t.account), move |e| {
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
        let sel = self.selected();
        let id = sel.as_ref().map(|t| t.id.clone());
        if id != self.preview.id {
            self.preview = Preview { id, since: Some(Instant::now()), scroll: 0 };
            return;
        }
        let (Some(id), Some(t)) = (id, sel) else { return };
        let settled = self.preview.since.is_some_and(|t| t.elapsed() >= PREVIEW_DELAY);
        if settled && !self.cache.contains_key(&id) && !self.fetching.contains(&id) {
            self.fetching.insert(id.clone());
            self.spawn_on(self.eng(&t.account), move |e| {
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
        // Clicks pick rows only in the mail list; elsewhere right-click goes back.
        let reading = !matches!(self.view, View::List);
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
                if self.tab == CLAUDE_TAB && self.search.is_none() {
                    self.claude_click(m.row - list.y);
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
        if t == SENT_TAB && !self.sent_loaded {
            self.reload_sent();
        }
        if t == CLAUDE_TAB {
            self.maybe_auto_brief();
        }
    }

    fn move_sel(&mut self, d: i32) {
        if let View::Library(lib) = &mut self.view {
            lib.move_sel(d);
            return;
        }
        if self.tab == CLAUDE_TAB && self.search.is_none() {
            self.claude_move(d);
            return;
        }
        let n = self.visible().len() as i32;
        if n == 0 {
            return;
        }
        let i = (self.table.selected().unwrap_or(0) as i32 + d).clamp(0, n - 1);
        self.table.select(Some(i as usize));
    }

    fn submit_input(&mut self, kind: InputKind, buf: String) {
        let buf = buf.trim().to_string();
        // A handled note is optional: ↵ on its own still archives.
        if let InputKind::HandledNote { thread_id, account } = kind {
            self.mark_handled(thread_id, account, buf);
            return;
        }
        if buf.is_empty() {
            return;
        }
        match kind {
            InputKind::Search => {
                self.loading = true;
                self.status = format!("Searching “{buf}”…");
                let q = buf.clone();
                self.spawn_search(buf, 50, move |per| Msg::Searched(q, per));
            }
            InputKind::ClaudeNote { thread_id, account } => self.start_draft(thread_id, account, Some(buf)),
            InputKind::HandledNote { .. } => {} // taken above
            InputKind::TaskEmail { thread_id, account, idx } => {
                if !buf.contains('@') {
                    self.status = "That doesn't look like an email address".into();
                    return;
                }
                let saved = crate::config::Config::load().and_then(|mut c| {
                    c.tasks.email = buf.clone();
                    c.save()
                });
                match saved {
                    Err(e) => self.status = format!("Error: {e:#}"),
                    Ok(()) => {
                        let a = self.brief_action(&thread_id, idx);
                        if let Some(a) = a {
                            self.press_action(thread_id, account, idx, a);
                        }
                    }
                }
            }
            InputKind::TemplateAsk { base } => {
                self.status = match &base {
                    Some(t) => format!("Claude is revising “{}”…", t.name),
                    None => "Claude is writing a template…".into(),
                };
                self.spawn(move |e| Msg::AiTemplate(e.ai_template(&buf, base.as_ref())));
            }
            InputKind::Label => {
                if let Some(t) = self.selected() {
                    self.status = format!("Labelling {buf}…");
                    let id = t.id.clone();
                    self.spawn_on(self.eng(&t.account), move |e| Msg::Done {
                        status: e.label(&Actor::User, &id, &buf, false).map(|_| format!("Labelled {buf}")),
                        reload: true,
                    });
                }
            }
        }
    }

    /// Resolve recipients/subject/template so the composer shows the real message.
    fn prefill(&self, req: &SendReq, from: &str) -> Result<(String, String, String, String)> {
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
        let c = self.eng(from).compose(&probe)?;
        let to = if req.to.is_empty() && req.reply_to_thread.is_none() { String::new() } else { c.to };
        Ok((to, c.cc, c.subject, c.body.trim().to_string()))
    }

    fn open_compose(&mut self, mut req: SendReq, template: Option<Template>, from: String) {
        if let Some(t) = &template {
            req.template = Some(t.name.clone());
        }
        let title = match (&req.reply_to_thread, &template, req.subject.starts_with("Fwd:")) {
            (_, Some(t), _) => format!(" Reply · template “{}” ", t.name),
            (Some(_), None, _) => " Reply ".to_string(),
            (None, None, true) => " Forward ".to_string(),
            _ => " New message ".to_string(),
        };
        match self.prefill(&req, &from) {
            Ok((to, cc, subject, body)) => {
                let focus = if to.is_empty() { 0 } else if subject.is_empty() { 3 } else { 4 };
                self.overlay = Overlay::Compose(ComposeForm {
                    title,
                    fields: [to, cc, String::new(), subject, body],
                    focus,
                    reply_to: req.reply_to_thread,
                    from,
                    ai_thread: None,
                    armed_discard: false,
                    ..Default::default()
                });
            }
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    fn compose_key(&mut self, mut form: ComposeForm, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let req = |f: &ComposeForm| SendReq {
            to: tidy_addresses(&f.fields[0]),
            cc: tidy_addresses(&f.fields[1]),
            bcc: tidy_addresses(&f.fields[2]),
            subject: f.fields[3].trim().to_string(),
            body: f.fields[4].clone(),
            reply_to_thread: f.reply_to.clone(),
            ..Default::default()
        };
        if k.code != KeyCode::Esc {
            form.armed_discard = false;
        }
        if let Some(buf) = form.ask.as_mut() {
            match k.code {
                KeyCode::Esc => form.ask = None,
                KeyCode::Enter => {
                    let instruction = buf.trim().to_string();
                    form.ask = None;
                    if !instruction.is_empty() {
                        let r = req(&form);
                        self.ask_claude(&mut form, instruction, r);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char('u') if ctrl => buf.clear(),
                KeyCode::Char(c) if !ctrl => buf.push(c),
                _ => {}
            }
            self.overlay = Overlay::Compose(form);
            return;
        }
        if !form.sugg.is_empty() {
            match k.code {
                KeyCode::Down => form.sugg_sel = (form.sugg_sel + 1).min(form.sugg.len() - 1),
                KeyCode::Up => form.sugg_sel = form.sugg_sel.saturating_sub(1),
                KeyCode::Tab | KeyCode::Enter => accept_suggestion(&mut form),
                KeyCode::Esc => form.sugg.clear(),
                KeyCode::Char(_) | KeyCode::Backspace => return self.compose_edit(form, k),
                _ => {
                    form.sugg.clear();
                    return self.compose_key(form, k);
                }
            }
            self.overlay = Overlay::Compose(form);
            return;
        }
        match k.code {
            KeyCode::Char('s') if ctrl => {
                if form.fields[0].trim().is_empty() || form.fields[4].trim().is_empty() {
                    self.status = "Need a recipient and a message before sending".into();
                    self.overlay = Overlay::Compose(form);
                    return;
                }
                // Claude marks what it couldn't know as [[…]]; never send those by accident.
                if form.fields[4].contains("[[") && form.fields[4].contains("]]") {
                    self.status = "Fill in or delete the [[…]] gaps Claude left before sending".into();
                    self.overlay = Overlay::Compose(form);
                    return;
                }
                self.status = "Sending…".into();
                let r = req(&form);
                let ai = form.ai_thread.clone();
                self.spawn_on(self.eng(&form.from), move |e| {
                    let sent = e.send(&Actor::User, &r);
                    if let (Ok(_), Some(t)) = (&sent, &ai) {
                        let _ = e.set_ai_draft_status(t, "sent");
                    }
                    Msg::Done { status: sent.map(|_| "Sent".into()), reload: true }
                });
            }
            KeyCode::Char('d') if ctrl => {
                self.status = "Saving draft…".into();
                let r = req(&form);
                let ai = form.ai_thread.clone();
                self.spawn_on(self.eng(&form.from), move |e| {
                    let saved = e.draft(&Actor::User, &r);
                    if let (Ok(_), Some(t)) = (&saved, &ai) {
                        let _ = e.set_ai_draft_status(t, "saved");
                    }
                    Msg::Done { status: saved.map(|_| "Draft saved in Gmail".into()), reload: false }
                });
            }
            KeyCode::Char('e') if ctrl => {
                self.job = Some(EditorJob::Compose { req: req(&form), template: None, from: form.from.clone(), ai_thread: form.ai_thread.clone() });
            }
            KeyCode::Char('k') if ctrl => {
                if form.ask_busy {
                    self.status = "Claude is still writing…".into();
                } else {
                    form.ask = Some(String::new());
                    form.sugg.clear();
                }
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Char('z') if ctrl => {
                self.undo_correction(&mut form);
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Char('g') if ctrl => {
                self.autocorrect_on = !self.autocorrect_on;
                self.status = if self.autocorrect_on { "✦ Autocorrect on".into() } else { "Autocorrect off".into() };
                self.overlay = Overlay::Compose(form);
            }
            // Cycle the sending account (new mail only; replies stay in their thread's account).
            KeyCode::Char('f') if ctrl => {
                let mut accts: Vec<&String> = self.engines.keys().collect();
                accts.sort();
                if form.reply_to.is_some() {
                    self.status = "Replies go out from the account the thread is in".into();
                } else if let Some(i) = accts.iter().position(|a| **a == form.from) {
                    form.from = accts[(i + 1) % accts.len()].clone();
                }
                self.overlay = Overlay::Compose(form);
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
                    self.typed_in_body(&mut form, '\n');
                } else {
                    form.focus += 1;
                }
                self.overlay = Overlay::Compose(form);
            }
            KeyCode::Backspace | KeyCode::Char(_) => self.compose_edit(form, k),
            _ => self.overlay = Overlay::Compose(form),
        }
    }

    /// Typing / deleting in a compose field; address fields get suggestions.
    fn compose_edit(&mut self, mut form: ComposeForm, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Backspace if ctrl => {
                let f = &mut form.fields[form.focus];
                while f.ends_with(char::is_whitespace) {
                    f.pop();
                }
                while f.chars().last().is_some_and(|c| !c.is_whitespace()) {
                    f.pop();
                }
            }
            KeyCode::Backspace => {
                form.fields[form.focus].pop();
            }
            KeyCode::Char(c) if !ctrl => {
                form.fields[form.focus].push(c);
                if form.focus == 4 {
                    self.typed_in_body(&mut form, c);
                }
            }
            _ => {}
        }
        if form.focus == 4 {
            form.ac.typed = Some(Instant::now());
        }
        form.sugg.clear();
        if form.focus < 3 {
            let field = &form.fields[form.focus];
            let word = current_address(field);
            if !word.is_empty() && !word.ends_with('>') {
                let already = field.to_lowercase();
                form.sugg = self
                    .people(&form.from, word, 12)
                    .into_iter()
                    .filter(|p| !already.contains(&format!("<{}>", p.email)) && !already.split(',').any(|a| a.trim() == p.email))
                    .take(6)
                    .collect();
                form.sugg_sel = 0;
            }
        }
        self.overlay = Overlay::Compose(form);
    }

    /// Ctrl+K: Claude writes or rewrites the message, as `instruction` says.
    fn ask_claude(&mut self, form: &mut ComposeForm, instruction: String, req: SendReq) {
        self.ac_gen += 1;
        let gen = self.ac_gen;
        form.ask_busy = true;
        form.ask_gen = gen;
        self.status = "✦ Claude is writing…".into();
        let e = self.eng(&form.from.clone());
        self.spawn_on(e, move |e| Msg::Composed { gen, r: e.ai_compose(&instruction, &req) });
    }

    fn on_composed(&mut self, gen: u64, r: Result<crate::assistant::ComposeHelp>) {
        let Overlay::Compose(form) = &mut self.overlay else { return };
        if form.ask_gen != gen {
            return;
        }
        form.ask_busy = false;
        match r {
            Ok(h) if !h.body.is_empty() => {
                let before = form.fields[4].clone();
                form.fields[4] = h.body;
                if !h.subject.is_empty() {
                    form.fields[3] = h.subject;
                }
                form.focus = 4;
                form.check = h.check;
                // Claude's own text doesn't need a spelling pass straight away.
                form.ac.checked = autocorrect::checkable(&form.fields[4]).to_string();
                form.ac.undo = Some((before, form.fields[4].clone(), vec![]));
                form.ac.flash.clear();
                self.status = if form.fields[4].contains("[[") {
                    "✦ Claude wrote it — fill in the [[gaps]], Ctrl+Z undoes".into()
                } else {
                    "✦ Claude wrote it — read it over, Ctrl+Z undoes, Ctrl+K to change it".into()
                };
            }
            Ok(_) => self.status = "Claude came back empty — try saying more".into(),
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    /// Archive a thread taken care of outside email, with a note Claude learns from.
    fn mark_handled(&mut self, thread_id: String, account: String, note: String) {
        if matches!(self.view, View::Reader { .. }) {
            self.leave_reader();
        }
        self.status = "Archiving as handled…".into();
        self.spawn_on(self.eng(&account), move |e| Msg::Done {
            status: e.mark_handled(&Actor::User, &thread_id, &note).map(|h| {
                let who = if h.from_name.is_empty() { h.from_email } else { h.from_name };
                format!("Handled & archived — Claude will remember this mattered{}", if who.is_empty() { String::new() } else { format!(" to {who}") })
            }),
            reload: true,
        });
    }

    /// After a character lands in the body: fix a just-finished word from the typo list.
    fn typed_in_body(&mut self, form: &mut ComposeForm, c: char) {
        if !self.autocorrect_on || autocorrect::is_word_char(c) {
            return;
        }
        let before = form.fields[4].clone();
        if let Some((typo, fix)) = autocorrect::fix_last_word(&mut form.fields[4]) {
            if form.ac.rejected.contains(&typo) {
                form.fields[4] = before;
                return;
            }
            form.ac.note = format!("{typo} → {fix}");
            form.ac.undo = Some((before, form.fields[4].clone(), vec![typo]));
            form.ac.flash = fix.split(' ').map(String::from).collect();
            form.ac.flash_at = Some(Instant::now());
        }
    }

    /// Ask Claude to look over the body once typing has paused.
    fn sync_autocorrect(&mut self) {
        if !self.autocorrect_on {
            return;
        }
        let Overlay::Compose(form) = &mut self.overlay else { return };
        let text = autocorrect::checkable(&form.fields[4]).to_string();
        let paused = form.ac.typed.is_some_and(|t| t.elapsed().as_millis() >= autocorrect::PAUSE_MS);
        if form.ac.busy || !paused || text == form.ac.checked || text.split_whitespace().count() < 3 {
            return;
        }
        self.ac_gen += 1;
        let gen = self.ac_gen;
        form.ac.busy = true;
        form.ac.gen = gen;
        form.ac.checked = text.clone();
        let from = form.from.clone();
        let e = self.eng(&from);
        self.spawn_on(e, move |e| Msg::Corrected { gen, fixes: e.autocorrect(&text), checked: text });
    }

    fn on_corrected(&mut self, gen: u64, checked: String, fixes: Result<Vec<crate::assistant::Fix>>) {
        let Overlay::Compose(form) = &mut self.overlay else { return };
        if form.ac.gen != gen {
            return;
        }
        form.ac.busy = false;
        let fixes = match fixes {
            Ok(f) => f,
            Err(e) => {
                form.ac.note = format!("autocorrect unavailable: {}", e.to_string().chars().take(60).collect::<String>());
                form.ac.flash_at = Some(Instant::now());
                return;
            }
        };
        let body = form.fields[4].clone();
        let (new, made) = autocorrect::apply(&checked, &body, &fixes, &form.ac.rejected);
        if made.is_empty() {
            return;
        }
        // Nothing typed since the check: the corrected text counts as checked.
        if body == checked || autocorrect::checkable(&body) == checked {
            form.ac.checked = autocorrect::checkable(&new).to_string();
        }
        form.ac.note = made.iter().take(3).map(|(a, b)| format!("{a} → {b}")).collect::<Vec<_>>().join(", ");
        if made.len() > 3 {
            form.ac.note.push_str(&format!(" +{}", made.len() - 3));
        }
        form.ac.flash = made.iter().flat_map(|(_, b)| b.split(' ').map(String::from)).collect();
        form.ac.flash_at = Some(Instant::now());
        form.ac.undo = Some((body, new.clone(), made.into_iter().map(|(a, _)| a).collect()));
        form.fields[4] = new;
    }

    /// Ctrl+Z: put back what the last correction changed (keeping anything typed since).
    fn undo_correction(&mut self, form: &mut ComposeForm) {
        let Some((before, after, typos)) = form.ac.undo.take() else {
            self.status = "Nothing to undo".into();
            return;
        };
        let body = &form.fields[4];
        if let Some(rest) = body.strip_prefix(&after) {
            form.fields[4] = format!("{before}{rest}");
        } else if body == &after {
            form.fields[4] = before;
        } else {
            self.status = "Text changed since the correction — fix it by hand".into();
            return;
        }
        form.ac.rejected.extend(typos);
        form.ac.checked = autocorrect::checkable(&form.fields[4]).to_string();
        form.ac.flash.clear();
        form.ac.note.clear();
        self.status = "Undid the correction".into();
    }

    /// Address book search as an account (local only, so it's instant).
    fn people(&self, account: &str, query: &str, limit: usize) -> Vec<Person> {
        self.eng(account).find_people(&Actor::User, query, limit).unwrap_or_default()
    }

    fn draw_compose(&self, f: &mut Frame, form: &ComposeForm) {
        let area = f.area();
        let w = area.width.saturating_sub(4).min(100);
        let h = area.height.saturating_sub(2);
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + 1, width: w, height: h };
        f.render_widget(Clear, r);
        let multi = self.engines.len() > 1;
        let hint = if multi {
            " Ctrl+K ask Claude · Ctrl+S send · Ctrl+D draft · Ctrl+E $EDITOR · Ctrl+F from · Esc discard "
        } else {
            " Ctrl+K ask Claude · Ctrl+S send · Ctrl+D save draft · Ctrl+E $EDITOR · Esc discard "
        };
        let hint = if form.ask.is_some() { " ↵ ask · Esc close · e.g. “say Saturday works, ask about parking” · “shorter and warmer” " } else { hint };
        let hint = if form.sugg.is_empty() { hint } else { " ↑↓ pick · Tab/↵ add · Esc hide · keep typing to narrow " };
        let mut block = self.th.popup(&form.title).title_bottom(Line::from(hint).fg(self.th.muted));
        let ac = if !self.autocorrect_on {
            Span::styled(" autocorrect off · Ctrl+G ", Style::new().fg(self.th.muted))
        } else if form.ac.flash_at.is_some_and(|t| t.elapsed().as_millis() < autocorrect::FLASH_MS * 2) && !form.ac.note.is_empty() {
            Span::styled(format!(" ✦ {} · Ctrl+Z undo ", form.ac.note), Style::new().fg(self.th.sun))
        } else if form.ac.busy {
            Span::styled(" ✦ checking… ", Style::new().fg(self.th.muted))
        } else {
            Span::styled(" ✦ autocorrect ", Style::new().fg(self.th.muted))
        };
        block = block.title_bottom(Line::from(ac).right_aligned());
        if multi {
            block = block.title(Line::from(format!(" from {} ", form.from)).fg(self.th.accent).right_aligned());
        }
        let inner = block.inner(r);
        f.render_widget(block, r);
        // Bottom of the window: the Ask Claude bar, or what Claude says to check.
        let mut extra: Vec<Line> = vec![];
        if let Some(buf) = &form.ask {
            extra.push(Line::from(vec![
                Span::styled(" ✦ Ask Claude  ", Style::new().fg(self.th.on_chip).bg(self.th.sun).bold()),
                Span::raw(format!(" {buf}")),
                Span::styled("▏", Style::new().fg(self.th.sun)),
            ]));
        } else if form.ask_busy {
            extra.push(Line::from(Span::styled(" ✦ Claude is writing…", Style::new().fg(self.th.sun))));
        } else {
            for c in form.check.iter().take(3) {
                extra.push(Line::from(vec![Span::styled(" ⚠ ", Style::new().fg(self.th.sun)), Span::styled(c.clone(), Style::new().fg(self.th.muted))]));
            }
        }
        let extra_h = if extra.is_empty() { 0 } else { extra.len() as u16 + 1 };
        let [hdr, sep, body, bottom] =
            Layout::vertical([Constraint::Length(4), Constraint::Length(1), Constraint::Fill(1), Constraint::Length(extra_h)]).areas(inner);
        if !extra.is_empty() {
            let b = Block::new().borders(Borders::TOP).border_style(Style::new().fg(self.th.rule));
            f.render_widget(Paragraph::new(extra).wrap(Wrap { trim: true }).block(b), bottom);
        }
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
        let flashing = form.ac.flash_at.is_some_and(|t| t.elapsed().as_millis() < autocorrect::FLASH_MS);
        let hl = Style::new().fg(self.th.sun).underlined();
        let mut lines: Vec<Line> = form.fields[4]
            .split('\n')
            .map(|l| if flashing { highlight_words(l, &form.ac.flash, hl) } else { Line::raw(l.to_string()) })
            .collect();
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

        if !form.sugg.is_empty() && form.focus < 3 {
            let x = hdr.x + 10;
            let w = 72.min(inner.right().saturating_sub(x));
            let drop = Rect { x, y: hdr.y + form.focus as u16 + 1, width: w, height: form.sugg.len() as u16 + 2 };
            let drop = drop.intersection(inner);
            f.render_widget(Clear, drop);
            f.render_widget(Paragraph::new(self.person_lines(&form.sugg, form.sugg_sel, w)).block(Block::bordered().border_style(Style::new().fg(self.th.rule))), drop);
        }
    }

    /// One row per person: ★ for saved profiles, name, address, company.
    fn person_lines(&self, people: &[Person], sel: usize, width: u16) -> Vec<Line<'static>> {
        let name_w = (width as usize / 3).clamp(12, 28);
        people
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let picked = i == sel;
                let main = if picked { Style::new().fg(self.th.on_chip).bg(self.th.sun) } else { Style::new().fg(self.th.fg) };
                let dim = if picked { main } else { Style::new().fg(self.th.muted) };
                let star = Span::styled(if p.profile { "★ " } else { "  " }, if picked { main } else { Style::new().fg(self.th.sun) });
                let name: String = if p.name.is_empty() { p.email.clone() } else { p.name.clone() };
                let name: String = name.chars().take(name_w).collect();
                let extra = if p.company.is_empty() { String::new() } else { format!("  · {}", p.company) };
                Line::from(vec![
                    star,
                    Span::styled(format!("{name:<name_w$}  "), main.bold()),
                    Span::styled(format!("{}{extra} ", p.email), dim),
                ])
            })
            .collect()
    }

    // ---------- editor ----------

    fn run_job(&mut self, job: EditorJob) {
        match job {
            EditorJob::Field(mut form) => {
                if let Err(e) = form.edit_focused_in_editor() {
                    self.status = format!("Error: {e:#}");
                }
                self.overlay = Overlay::Form(form);
            }
            EditorJob::Compose { req, template, from, ai_thread } => match self.run_editor(req, template, &from) {
                Ok(Some(r)) => {
                    let title = if r.reply_to_thread.is_some() { " Reply " } else { " New message " };
                    self.overlay = Overlay::Compose(ComposeForm {
                        title: title.into(),
                        fields: [r.to, r.cc, r.bcc, r.subject, r.body],
                        focus: 4,
                        reply_to: r.reply_to_thread,
                        from,
                        ai_thread,
                        armed_discard: false,
                        ..Default::default()
                    });
                }
                Ok(None) => self.status = "Discarded empty message".into(),
                Err(err) => self.status = format!("Error: {err:#}"),
            },
        }
    }

    fn run_editor(&mut self, mut req: SendReq, template: Option<Template>, from: &str) -> Result<Option<SendReq>> {
        if let Some(t) = &template {
            req.template = Some(t.name.clone());
        }
        let (to, cc, subject, body) = self.prefill(&req, from)?;

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
            View::List if self.tab == CLAUDE_TAB && self.search.is_none() => {
                self.hits.split = false;
                self.draw_claude(f, main);
            }
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
            View::Library(_) => self.draw_library(f, main),
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
        let many = format!("{} inboxes", self.shown.len());
        let me = match (area.width >= 120, self.shown.len() > 1) {
            (false, _) => "",
            (true, true) => many.as_str(),
            (true, false) => self.me.as_str(),
        };
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
        let multi = self.shown.len() > 1;
        let th = &self.th;
        let sent_tab = self.tab == SENT_TAB && self.search.is_none();
        let rows: Vec<Row> = self
            .visible()
            .iter()
            .map(|t| {
                // Sent rows show the person you wrote to.
                let (who, who_email) = if sent_tab {
                    (recipients(&t.to), crate::gmail::split_address(t.to.split(',').next().unwrap_or("")).1)
                } else {
                    (t.from_name.clone(), t.from_email.clone())
                };
                let initial = who.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
                let bar = if t.unread { Span::styled("▌", Style::new().fg(th.accent)) } else { Span::raw(" ") };
                let avatar = Span::styled(format!(" {initial} "), Style::new().fg(th.on_chip).bg(th.avatar(&who_email)).bold());
                let (name_style, subj_style) = if t.unread {
                    (Style::new().fg(th.bright).bold(), Style::new().fg(th.bright).bold())
                } else {
                    (Style::new().fg(th.fg), Style::new().fg(th.fg))
                };
                // In Sent, a row is about who you wrote to, and whether they've answered.
                let mut from = if sent_tab {
                    vec![Span::styled("To ", Style::new().fg(th.muted)), Span::styled(recipients(&t.to), name_style)]
                } else {
                    vec![Span::styled(t.from_name.clone(), name_style)]
                };
                if t.message_count > 1 {
                    from.push(Span::styled(format!(" {}", t.message_count), Style::new().fg(th.muted)));
                }
                let mut subj = vec![];
                if t.starred {
                    subj.push(Span::styled("★ ", Style::new().fg(th.sun)));
                }
                if sent_tab {
                    if t.from_email.eq_ignore_ascii_case(&t.account) {
                        subj.push(Span::styled("no reply yet ", Style::new().fg(th.muted).italic()));
                    } else {
                        subj.push(Span::styled(" replied ", Style::new().fg(th.on_chip).bg(th.accent)));
                        subj.push(Span::raw(" "));
                    }
                } else if t.needs_reply {
                    subj.push(Span::styled(" reply ", Style::new().fg(th.on_chip).bg(th.sun)));
                    subj.push(Span::raw(" "));
                }
                subj.push(Span::styled(t.subject.clone(), subj_style));
                let date = Span::styled(fmt_date(t.date, false), Style::new().fg(if t.unread { th.accent } else { th.muted }));
                // With several inboxes merged, tag each row with the account it's in.
                let tag = multi.then(|| Span::styled(account_tag(&t.account, &self.shown), Style::new().fg(th.avatar(&t.account))));
                if split {
                    // Two-line card: sender + date, then subject, then a muted snippet.
                    Row::new(vec![
                        Cell::from(Text::from(vec![Line::from(vec![bar.clone(), Span::raw(" "), avatar]), Line::from(bar.clone()), Line::from(bar)])),
                        Cell::from(Text::from(vec![
                            Line::from(from),
                            Line::from(subj),
                            Line::from(Span::styled(t.snippet.clone(), Style::new().fg(th.muted))),
                        ])),
                        Cell::from(Text::from(vec![
                            Line::from(date).alignment(Alignment::Right),
                            Line::from(tag.unwrap_or_default()).alignment(Alignment::Right),
                        ])),
                    ])
                    .height(3)
                    .bottom_margin(1)
                } else {
                    subj.push(Span::styled(format!(" — {}", t.snippet), Style::new().fg(th.muted)));
                    if let Some(tag) = tag {
                        from.insert(0, Span::styled("● ", tag.style));
                    }
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
            let msg = if self.loading || (sent_tab && !self.sent_loaded) { "Loading…" } else { "Nothing here. ✏️" };
            f.render_widget(Paragraph::new(msg).fg(th.muted).alignment(Alignment::Center).block(Block::new().padding(Padding::top(area.height / 3))), area);
            return;
        }
        let widths: &[Constraint] = if split {
            &[Constraint::Length(5), Constraint::Fill(1), Constraint::Length(if multi { 11 } else { 8 })]
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
        if self.engines.len() > 1 && !t.account.is_empty() {
            meta.push_str(&format!(" · in {}", t.account));
        }
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
        let help = match &self.view {
            View::List if self.tab == CLAUDE_TAB && self.search.is_none() => {
                "space actions · ↵ check & send · w redraft · D draft · o open · e archive · H handled · x dismiss · F labels · L learn · R refresh"
            }
            View::Library(l) if l.kind == LibKind::Templates => {
                "↵ edit · n new · w Claude writes one · W Claude revises · x delete · esc back"
            }
            View::Library(_) => "↵ edit · n new · L Claude learns from their mail · x delete · esc back",
            View::List => "c compose · ↵ open · r reply · T template · e archive · H handled · s star · / search · C contacts · t templates · P people · a inboxes · ? help",
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
            Overlay::Form(form) => form.draw(f, &self.th),
            Overlay::Actions { items, sel, .. } => {
                let lines: Vec<Line> = items
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let style = if i == *sel { Style::new().fg(self.th.on_chip).bg(self.th.sun) } else { Style::new().fg(self.th.fg) };
                        let tick = if a.done { "✓" } else { " " };
                        Line::from(vec![
                            Span::styled(format!(" {} {tick} ", i + 1), Style::new().fg(self.th.sun).bold()),
                            Span::styled(format!(" {} {} ", claude::action_icon(&a.kind), a.label), style),
                            Span::styled(format!("  {}", claude::action_detail(a)), Style::new().fg(self.th.muted)),
                        ])
                    })
                    .collect();
                let r = centered(area, 86, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(self.th.popup(" Do it — 1-9 or ↵ · esc close ")), r);
            }
            Overlay::Help => {
                let keys = [
                    ("Tab / 1-7", "switch tab (To do · Primary · Starred · Newsletters · All · Sent · ✦ Claude)"),
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
                    ("H", "handled another way: add a note, archive, and Claude remembers it mattered"),
                    ("/", "search (Gmail syntax)"),
                    ("C", "contacts: find someone, ↵ to write to them"),
                    ("p", "reader: clean ↔ full view"),
                    ("J K", "scroll the preview pane (wide terminals)"),
                    ("mouse", "click to select, double-click to open, wheel scrolls, right-click back"),
                    ("a", "inboxes: tick several to show together · add · default · sign out"),
                    ("6", "Sent: what you sent, and who hasn't replied yet"),
                    ("7", "✦ Claude: your inbox sorted into categories, drafted replies to check & send"),
                    ("space", "✦ Claude: the buttons Claude suggests (calendar, task, link, unsubscribe…)"),
                    ("F", "✦ Claude: file every email into a Sunbox/<category> Gmail label"),
                    ("t", "templates: edit yours, or have Claude write one"),
                    ("P", "people: client profiles Claude learns from your mail"),
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
                    InputKind::ClaudeNote { .. } => " Tell Claude how to reply (e.g. “decline politely, offer next week”) ",
                    InputKind::TemplateAsk { base: None } => " Describe the template (e.g. “booking confirmation with date, venue and deposit”) ",
                    InputKind::TemplateAsk { base: Some(_) } => " How should Claude change it? ",
                    InputKind::TaskEmail { .. } => " Your TickTick add-tasks-by-email address (from TickTick's settings) ",
                    InputKind::HandledNote { .. } => " How did you handle it? (e.g. “called her”) · ↵ archives ",
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
                        let tick = if list.ticked.get(i).copied().unwrap_or(false) { "[✓]" } else { "[ ]" };
                        let current = Span::styled(format!(" {tick}"), Style::new().fg(if *a == self.me { self.th.accent } else { self.th.muted }));
                        let default = if list.default.as_deref() == Some(a.as_str()) { Span::styled("  default", Style::new().fg(self.th.muted)) } else { Span::raw("") };
                        Line::from(vec![current, Span::styled(format!(" {:<40} ", a), style), default])
                    })
                    .collect();
                lines.push(Line::raw(""));
                let hint = if self.signing_in {
                    "  waiting on your browser…".to_string()
                } else {
                    "  space tick · A all · ↵ show ticked · o only this · n add · d default · x sign out".to_string()
                };
                lines.push(Line::from(Span::styled(hint, Style::new().fg(self.th.muted))));
                let r = centered(area, 84, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(self.th.popup(" Inboxes — tick the ones to show together ")), r);
            }
            Overlay::People { query, items, sel } => {
                let r = centered(area, 88, PEOPLE_ROWS as u16 + 5);
                let mut lines = vec![
                    Line::from(vec![Span::styled(" Find  ", Style::new().fg(self.th.sun).bold()), Span::raw(query.clone()), Span::styled("▏", Style::new().fg(self.th.sun))]),
                    Line::raw(""),
                ];
                if items.is_empty() {
                    let msg = if query.is_empty() { "  Your address book is still filling in from your mail — give it a moment." } else { "  Nobody matches." };
                    lines.push(Line::from(Span::styled(msg, Style::new().fg(self.th.muted))));
                }
                lines.extend(self.person_lines(items, *sel, r.width.saturating_sub(2)));
                f.render_widget(Clear, r);
                let block = self.th.popup(" Contacts — type to search · ↑↓ · ↵ write to them · esc close ");
                f.render_widget(Paragraph::new(lines).block(block), r);
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


/// A line with some words picked out (corrected words, briefly).
fn highlight_words(line: &str, words: &[String], style: Style) -> Line<'static> {
    let mut spans = vec![];
    let mut word = String::new();
    let flush = |word: &mut String, spans: &mut Vec<Span<'static>>| {
        if !word.is_empty() {
            let w = std::mem::take(word);
            spans.push(if words.iter().any(|x| x == &w) { Span::styled(w, style) } else { Span::raw(w) });
        }
    };
    for c in line.chars() {
        if autocorrect::is_word_char(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut spans);
            spans.push(Span::raw(c.to_string()));
        }
    }
    flush(&mut word, &mut spans);
    Line::from(spans)
}

/// "Jane Doe, sam@x.org" from a To header: names where there are names.
fn recipients(to: &str) -> String {
    if to.trim().is_empty() {
        return "(no recipient)".into();
    }
    to.split(',')
        .map(|a| crate::gmail::split_address(a.trim()))
        .map(|(name, email)| if name.trim().is_empty() { email } else { name })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Short account label for list rows: the part before the @, plus a bit of the
/// domain when another shown account has the same name (bram@b… vs bram@e…).
fn account_tag(account: &str, shown: &[String]) -> String {
    let local = |a: &str| a.split('@').next().unwrap_or(a).to_string();
    let mine = local(account);
    let clash = shown.iter().any(|o| o != account && local(o) == mine);
    let tag = if clash {
        let dom: String = account.split('@').nth(1).unwrap_or("").chars().take(3).collect();
        format!("{}@{dom}", mine.chars().take(6).collect::<String>())
    } else {
        mine
    };
    tag.chars().take(10).collect()
}

const PEOPLE_ROWS: usize = 12;

/// The address being typed: everything after the last comma.
fn current_address(field: &str) -> &str {
    field.rsplit(',').next().unwrap_or("").trim_start()
}

/// Swap the half-typed address for the picked person, ready for the next one.
fn accept_suggestion(form: &mut ComposeForm) {
    let Some(p) = form.sugg.get(form.sugg_sel) else { return };
    let field = &mut form.fields[form.focus];
    let keep = field.rfind(',').map(|i| i + 1).unwrap_or(0);
    field.truncate(keep);
    if keep > 0 {
        field.push(' ');
    }
    field.push_str(&p.address());
    field.push_str(", ");
    form.sugg.clear();
}

/// Trim the trailing ", " a picked suggestion leaves behind.
fn tidy_addresses(s: &str) -> String {
    s.trim().trim_end_matches(',').trim_end().to_string()
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h }
}
