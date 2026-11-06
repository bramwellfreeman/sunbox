//! The ✦ Claude tab: what matters in the inbox, replies Claude drafted for you to check and
//! send, and template ideas. Claude never sends; ↵ opens its draft in the composer.

use super::form::Form;
use super::{App, ComposeForm, InputKind, Msg, Overlay, CLAUDE_TAB};
use crate::actions::Outcome;
use crate::engine::Actor;
use crate::models::{AiDraft, Briefing, BriefItem, Contact, SuggestedAction, Template, TemplateIdea, ThreadSummary};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{HashMap, HashSet, VecDeque};

/// Replies Claude drafts on its own after a briefing (every item that needs a reply, most urgent first).
const AUTO_DRAFTS: usize = 12;
/// Threads Claude reads in full after a briefing to pick their buttons.
const AUTO_PLANS: usize = 12;

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
    /// Threads waiting for a full read to pick their buttons, and the one being read.
    plan_queue: VecDeque<(String, String)>,
    planning: Option<String>,
    /// Full reads that failed this session (not retried on their own).
    plan_failed: HashSet<String>,
    sel: usize,
    offset: usize,
    scroll: u16,
}

enum Row<'a> {
    /// A category heading and how many emails are under it.
    Header(String, usize),
    Item(&'a BriefItem),
    Idea(&'a TemplateIdea),
}

impl Row<'_> {
    fn height(&self) -> usize {
        if matches!(self, Row::Header(..)) { 1 } else { 2 }
    }
}

fn prio_rank(p: &str) -> u8 {
    match p {
        "urgent" => 0,
        "high" => 1,
        "normal" => 2,
        _ => 3,
    }
}

pub(super) fn action_icon(kind: &str) -> &'static str {
    match kind {
        "calendar" => "◷",
        "task" => "☐",
        "link" => "↗",
        "unsubscribe" => "⊘",
        "archive" => "▤",
        "star" => "★",
        "label" => "#",
        "forward" => "→",
        _ => "•",
    }
}

/// The detail shown next to a button: when, where, which site, who.
pub(super) fn action_detail(a: &SuggestedAction) -> String {
    let when = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .map(|d| d.format("%a %b %-d, %-I:%M %p").to_string())
            .or_else(|_| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.format("%a %b %-d").to_string()))
            .unwrap_or_else(|_| s.to_string())
    };
    match a.kind.as_str() {
        "calendar" => [when(&a.start), a.location.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "),
        "task" if !a.start.is_empty() => format!("due {}", when(&a.start)),
        "task" => "TickTick".into(),
        "link" => a.url.split('/').nth(2).unwrap_or_default().to_string(),
        "forward" => a.to.clone(),
        "label" => a.title.clone(),
        _ => String::new(),
    }
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

    /// Items grouped under Claude's categories (its order), most urgent first in each.
    fn rows_of<'a>(&self, b: &'a Briefing) -> Vec<Row<'a>> {
        // Threads that left the inbox (archived, answered elsewhere) drop off.
        let live: Vec<&BriefItem> =
            b.items.iter().filter(|i| self.inbox.is_empty() || self.inbox.iter().any(|t| t.id == i.thread_id)).collect();
        let mut cats: Vec<String> = b.categories.clone();
        for i in &live {
            if !cats.iter().any(|c| c.eq_ignore_ascii_case(&i.category)) {
                cats.push(i.category.clone());
            }
        }
        let mut rows: Vec<Row> = vec![];
        for c in &cats {
            let mut group: Vec<&BriefItem> = live.iter().filter(|i| i.category.eq_ignore_ascii_case(c)).copied().collect();
            if group.is_empty() {
                continue;
            }
            group.sort_by_key(|i| prio_rank(&i.priority));
            rows.push(Row::Header(if c.is_empty() { "Other".into() } else { c.clone() }, group.len()));
            rows.extend(group.into_iter().map(Row::Item));
        }
        if !b.template_ideas.is_empty() {
            rows.push(Row::Header("Template ideas".into(), b.template_ideas.len()));
            rows.extend(b.template_ideas.iter().map(Row::Idea));
        }
        rows
    }

    fn selected_item(&self) -> Option<BriefItem> {
        match self.rows().get(self.claude.sel) {
            Some(Row::Item(i)) => Some((*i).clone()),
            _ => None,
        }
    }

    /// A button from the briefing as it stands now.
    pub(super) fn brief_action(&self, thread_id: &str, idx: usize) -> Option<SuggestedAction> {
        let b = self.claude.briefing.as_ref()?;
        b.items.iter().find(|i| i.thread_id == thread_id)?.actions.get(idx).cloned()
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

    /// Move by `d` emails, stepping over category headings.
    pub(super) fn claude_move(&mut self, d: i32) {
        let rows = self.rows();
        let n = rows.len() as i32;
        if n == 0 {
            return;
        }
        let (step, mut left) = (d.signum(), d.unsigned_abs().min(n as u32));
        let mut i = self.claude.sel as i32;
        while left > 0 {
            let mut j = i + step;
            while (0..n).contains(&j) && matches!(rows[j as usize], Row::Header(..)) {
                j += step;
            }
            if !(0..n).contains(&j) {
                break;
            }
            i = j;
            left -= 1;
        }
        drop(rows);
        self.claude.sel = i as usize;
        self.claude.scroll = 0;
        self.plan_selected();
    }

    pub(super) fn claude_click(&mut self, y: u16) {
        let rows = self.rows();
        let mut top = 0;
        for (i, r) in rows.iter().enumerate().skip(self.claude.offset) {
            if (y as usize) < top + r.height() {
                if !matches!(r, Row::Header(..)) {
                    drop(rows);
                    self.claude.sel = i;
                    self.claude.scroll = 0;
                    self.plan_selected();
                }
                return;
            }
            top += r.height();
        }
    }

    /// Never rest on a heading.
    fn fix_sel(&mut self) {
        let rows = self.rows();
        let n = rows.len();
        let mut i = self.claude.sel.min(n.saturating_sub(1));
        while i < n && matches!(rows[i], Row::Header(..)) {
            i += 1;
        }
        if i >= n {
            i = self.claude.sel.min(n.saturating_sub(1));
            while i > 0 && matches!(rows[i], Row::Header(..)) {
                i -= 1;
            }
        }
        drop(rows);
        self.claude.sel = i;
    }

    // ---------- buttons ----------

    /// The selected email gets its full read next, if it hasn't had one.
    pub(super) fn plan_selected(&mut self) {
        let Some(i) = self.selected_item() else { return };
        if i.planned || self.claude.plan_failed.contains(&i.thread_id) || self.claude.planning.as_deref() == Some(i.thread_id.as_str()) {
            return;
        }
        self.claude.plan_queue.retain(|(id, _)| *id != i.thread_id);
        self.claude.plan_queue.push_front((i.thread_id, i.account));
        self.pump_plans();
    }

    fn pump_plans(&mut self) {
        if self.claude.planning.is_some() {
            return;
        }
        if let Some((id, account)) = self.claude.plan_queue.pop_front() {
            self.claude.planning = Some(id.clone());
            self.spawn_on(self.eng(&account), move |e| {
                let r = e.ai_actions(&id);
                Msg::Planned(id, r)
            });
        }
    }

    pub(super) fn on_planned(&mut self, id: String, r: anyhow::Result<Vec<SuggestedAction>>) {
        self.claude.planning = None;
        match r {
            Ok(actions) => {
                if let Some(i) = self.claude.briefing.as_mut().and_then(|b| b.items.iter_mut().find(|i| i.thread_id == id)) {
                    i.actions = actions;
                    i.planned = true;
                }
            }
            Err(e) => {
                self.claude.plan_failed.insert(id);
                self.status = format!("Error: {e:#}");
            }
        }
        self.pump_plans();
    }

    fn open_actions(&mut self) {
        let Some(i) = self.selected_item() else { return };
        if i.actions.is_empty() {
            self.status = if i.planned { "Claude has no buttons for this one".into() } else { "Claude is still reading this one…".into() };
            self.plan_selected();
            return;
        }
        self.overlay = Overlay::Actions { thread_id: i.thread_id, account: i.account, items: i.actions, sel: 0 };
    }

    /// Run a button. Tasks need the TickTick address once.
    pub(super) fn press_action(&mut self, thread_id: String, account: String, idx: usize, a: SuggestedAction) {
        if a.kind == "task" && crate::config::Config::load().map(|c| c.tasks.email.trim().is_empty()).unwrap_or(true) {
            self.overlay = Overlay::Input { kind: InputKind::TaskEmail { thread_id, account, idx }, buf: String::new() };
            return;
        }
        self.status = format!("{} {}…", action_icon(&a.kind), a.label);
        let acct = account.clone();
        self.spawn_on(self.eng(&account), move |e| {
            let r = e.press_action(&Actor::User, &thread_id, &a);
            Msg::Acted { thread_id, account: acct, idx, r }
        });
    }

    pub(super) fn on_acted(&mut self, thread_id: String, account: String, idx: usize, r: anyhow::Result<Outcome>) {
        match r {
            Ok(Outcome::Done(msg)) => {
                self.status = format!("✓ {msg}");
                let mut gone = false;
                if let Some(a) = self
                    .claude
                    .briefing
                    .as_mut()
                    .and_then(|b| b.items.iter_mut().find(|i| i.thread_id == thread_id))
                    .and_then(|i| i.actions.get_mut(idx))
                {
                    a.done = true;
                    gone = matches!(a.kind.as_str(), "archive" | "unsubscribe");
                }
                let _ = self.e.update_brief_item(&thread_id, |i| {
                    if let Some(a) = i.actions.get_mut(idx) {
                        a.done = true;
                    }
                });
                if gone {
                    self.reload();
                }
            }
            Ok(Outcome::Compose(req)) => self.open_compose(req, None, account),
            Err(e) => self.status = format!("Error: {e:#}"),
        }
    }

    /// Put every briefed email into a Gmail label named after its category (Sunbox/<category>).
    fn file_into_labels(&mut self) {
        let Some(b) = &self.claude.briefing else { return };
        let mut by_account: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
        for i in b.items.iter().filter(|i| !i.category.is_empty()) {
            by_account.entry(i.account.clone()).or_default().push((i.thread_id.clone(), format!("Sunbox/{}", i.category)));
        }
        let n: usize = by_account.values().map(Vec::len).sum();
        self.status = format!("Filing {n} emails into Sunbox/… labels…");
        for (account, items) in by_account {
            self.spawn_on(self.eng(&account), move |e| {
                let failed = items.iter().filter(|(id, label)| e.label(&Actor::User, id, label, false).is_err()).count();
                let status = if failed == 0 {
                    Ok(format!("Filed {} emails into Sunbox/… labels", items.len()))
                } else {
                    Err(anyhow::anyhow!("{failed} of {} emails couldn't be labelled", items.len()))
                };
                Msg::Done { status, reload: false }
            });
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
        } else {
            self.fix_sel();
            self.plan_selected();
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
                self.status = format!("✓ Claude sorted {} threads into {} categories", b.items.len(), b.categories.len());
                let mut by_prio: Vec<&BriefItem> = b.items.iter().collect();
                by_prio.sort_by_key(|i| prio_rank(&i.priority));
                for i in by_prio.iter().filter(|i| i.action == "reply").take(AUTO_DRAFTS) {
                    let has = self.claude.drafts.get(&i.thread_id).is_some_and(|d| d.status != "dismissed");
                    if !has && !self.claude.busy.contains(&i.thread_id) {
                        self.claude.queue.push_back((i.thread_id.clone(), i.account.clone()));
                    }
                }
                self.claude.plan_queue = by_prio
                    .iter()
                    .filter(|i| !i.planned && i.priority != "low")
                    .take(AUTO_PLANS)
                    .map(|i| (i.thread_id.clone(), i.account.clone()))
                    .collect();
                self.claude.briefing = Some(b);
                self.claude.sel = 0;
                self.claude.scroll = 0;
                self.fix_sel();
                self.pump_drafts();
                self.pump_plans();
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
            ..Default::default()
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
            _ => Pick::None,
        };
        let draft = match &pick {
            Pick::Item(i) => self.claude.drafts.get(&i.thread_id).filter(|d| d.status != "dismissed").cloned(),
            _ => None,
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.claude_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.claude_move(-1),
            KeyCode::Home => {
                self.claude.sel = 0;
                self.fix_sel();
            }
            KeyCode::End | KeyCode::Char('G') => self.claude_move(i32::MAX / 2),
            KeyCode::Char('J') | KeyCode::PageDown => self.claude.scroll = self.claude.scroll.saturating_add(3),
            KeyCode::Char('K') | KeyCode::PageUp => self.claude.scroll = self.claude.scroll.saturating_sub(3),
            KeyCode::Char('R') => self.run_brief(),
            KeyCode::Char(' ') | KeyCode::Char('b') => self.open_actions(),
            KeyCode::Char('F') => self.file_into_labels(),
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
                Row::Header(c, n) => ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", c.to_uppercase()), Style::new().fg(th.sun).bold()),
                    Span::styled(format!("{n}"), Style::new().fg(th.muted)),
                ])),
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
                    let icons: String = i.actions.iter().filter(|a| !a.done).map(|a| action_icon(&a.kind)).collect::<Vec<_>>().join(" ");
                    if !icons.is_empty() {
                        l1.push(Span::styled(format!("  {icons}"), Style::new().fg(th.accent)));
                    }
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
        drop(rows);
        self.fix_sel();
        let rows = self.rows_of(&b);
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
                if !i.actions.is_empty() {
                    for (n, a) in i.actions.iter().enumerate() {
                        let chip = if a.done { Style::new().fg(th.muted).bg(th.selection) } else { Style::new().fg(th.on_chip).bg(th.accent).bold() };
                        let mut l = vec![
                            Span::styled(format!(" {} {} {} ", n + 1, action_icon(&a.kind), a.label), chip),
                            Span::styled(format!("  {}", action_detail(a)), Style::new().fg(th.muted)),
                        ];
                        if a.done {
                            l.push(Span::styled("  ✓ done", Style::new().fg(th.muted)));
                        }
                        lines.push(Line::from(l));
                    }
                    let more = if i.planned { "" } else { " (Claude is still reading the whole thread)" };
                    lines.push(Line::from(Span::styled(format!("space: pick a button{more}"), Style::new().fg(th.muted))));
                    lines.push(Line::raw(""));
                } else if self.claude.planning.as_deref() == Some(i.thread_id.as_str()) {
                    lines.push(Line::from(Span::styled("✦ Claude is reading the whole thread for things to do…", Style::new().fg(th.muted))));
                    lines.push(Line::raw(""));
                }
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
            _ => {}
        }
        let detail = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::LEFT).border_style(Style::new().fg(th.rule)).padding(Padding::new(3, 3, 0, 0)));
        let max = detail.line_count(right.width.saturating_sub(7)) as u16;
        self.claude.scroll = self.claude.scroll.min(max.saturating_sub(right.height / 2));
        f.render_widget(detail.scroll((self.claude.scroll, 0)), right);
    }
}
