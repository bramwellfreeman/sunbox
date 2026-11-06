//! UI-agnostic facade shared by the TUI, the CLI, the MCP server and routines.
//! Every mutation names an [`Actor`]; non-human actors are permission-checked
//! against `[agent] allow` and written to the audit log.

use crate::config::Config;
use crate::gmail::{self, Gmail, Outgoing};
use crate::models::*;
use crate::oauth;
use crate::store::{now, Store, Tokens};
use crate::templates;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub enum Actor {
    /// A human at the TUI or CLI — never restricted.
    User,
    /// An AI agent over MCP (name = client name).
    Agent(String),
    /// A routine run (name = routine name).
    Routine(String),
}

impl Actor {
    pub fn label(&self) -> String {
        match self {
            Actor::User => "user".into(),
            Actor::Agent(n) => format!("agent:{n}"),
            Actor::Routine(n) => format!("routine:{n}"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendReq {
    /// Recipients (comma-separated). Optional when replying: defaults to the sender.
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    /// Optional when replying (defaults to "Re: <thread subject>") or using a template.
    #[serde(default)]
    pub subject: String,
    /// Plain-text body. Optional when `template` is set.
    #[serde(default)]
    pub body: String,
    /// Reply within this thread.
    #[serde(default)]
    pub reply_to_thread: Option<String>,
    /// Render this template (its body replaces/precedes `body`).
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct Composed {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub thread_id: Option<String>,
    #[serde(skip)]
    pub in_reply_to: String,
    /// Template placeholders that had no value.
    pub missing_vars: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RoutineReport {
    pub routine: String,
    pub skipped: Option<String>,
    pub matched: usize,
    pub results: Vec<String>,
}

pub struct Engine {
    pub cfg: Config,
    pub store: Store,
    /// The account this engine works as; `None` follows the default account.
    acct: Option<String>,
    token: Mutex<Option<(String, i64)>>,
}

impl Engine {
    /// Works as `account`, else `$SUNBOX_ACCOUNT`, else the default account.
    pub fn open(account: Option<String>) -> Result<Self> {
        let acct = account.or_else(|| std::env::var("SUNBOX_ACCOUNT").ok()).filter(|a| !a.trim().is_empty());
        Ok(Self { cfg: Config::load()?, store: Store::open()?, acct, token: Mutex::new(None) })
    }

    /// Another engine on the same config and data, working as `account`.
    pub fn for_account(&self, account: &str) -> Result<Engine> {
        Ok(Self { cfg: self.cfg.clone(), store: Store::open()?, acct: Some(account.to_string()), token: Mutex::new(None) })
    }

    // ---------- auth ----------

    pub fn login(&self, open_browser: bool, quiet: bool) -> Result<String> {
        let tok = oauth::sign_in(&self.cfg, open_browser, quiet)?;
        let refresh = tok
            .refresh_token
            .ok_or_else(|| anyhow!("Google returned no refresh token; revoke access and retry"))?;
        let account = Gmail::new(tok.access_token.clone()).profile_email()?;
        let expires_at = chrono::Utc::now().timestamp() + tok.expires_in;
        self.store.save_tokens(&Tokens {
            account: account.clone(),
            refresh_token: refresh,
            access_token: tok.access_token.clone(),
            expires_at,
        })?;
        // The cached token belongs to whichever account this engine was working as.
        *self.token.lock().unwrap() = None;
        self.store.audit("user", "login", &account, true);
        Ok(account)
    }

    /// Signs `account` (or this engine's account) out and returns which one it was.
    pub fn logout(&self, account: Option<&str>) -> Result<String> {
        let account = match account {
            Some(a) => a.to_string(),
            None => self.account()?,
        };
        if !self.store.remove_tokens(&account)? {
            bail!("{account} isn't signed in");
        }
        *self.token.lock().unwrap() = None;
        self.store.audit("user", "logout", &account, true);
        Ok(account)
    }

    pub fn account(&self) -> Result<String> {
        Ok(self.store.tokens(self.acct.as_deref())?.ok_or_else(|| not_signed_in(self.acct.as_deref()))?.account)
    }

    pub fn accounts(&self) -> Result<Vec<String>> {
        self.store.accounts()
    }

    /// Small UI preferences kept next to the data (e.g. which inboxes the TUI shows).
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.store.setting(key)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.store.set_setting(key, value)
    }

    pub fn default_account(&self) -> Result<Option<String>> {
        self.store.default_account()
    }

    pub fn set_default_account(&self, account: &str) -> Result<String> {
        let found = self.store.tokens(Some(account))?.ok_or_else(|| not_signed_in(Some(account)))?;
        self.store.set_default_account(&found.account)?;
        Ok(found.account)
    }

    fn gmail(&self) -> Result<Gmail> {
        let now = chrono::Utc::now().timestamp();
        let mut cached = self.token.lock().unwrap();
        if let Some((tok, exp)) = cached.as_ref() {
            if *exp - 60 > now {
                return Ok(Gmail::new(tok.clone()));
            }
        }
        let stored = self.store.tokens(self.acct.as_deref())?.ok_or_else(|| not_signed_in(self.acct.as_deref()))?;
        if stored.expires_at - 60 > now && !stored.access_token.is_empty() {
            *cached = Some((stored.access_token.clone(), stored.expires_at));
            return Ok(Gmail::new(stored.access_token));
        }
        let fresh = oauth::refresh(&self.cfg, &stored.refresh_token)?;
        let exp = now + fresh.expires_in;
        self.store.update_access(&stored.account, &fresh.access_token, exp)?;
        *cached = Some((fresh.access_token.clone(), exp));
        Ok(Gmail::new(fresh.access_token))
    }

    /// Permission check + audit wrapper for anything an actor does.
    pub(crate) fn act<T>(&self, actor: &Actor, perm: &str, action: &str, detail: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !matches!(actor, Actor::User) {
            if let Err(e) = self.cfg.require(perm) {
                self.store.audit(&actor.label(), action, &format!("DENIED {detail}"), false);
                return Err(e);
            }
        }
        let res = f();
        let quiet = matches!(actor, Actor::User) && perm == "read";
        if !quiet {
            let detail = match &res {
                Ok(_) => detail.to_string(),
                Err(e) => format!("{detail} — error: {e}"),
            };
            self.store.audit(&actor.label(), action, &detail, res.is_ok());
        }
        res
    }

    // ---------- mail ----------

    pub fn search(&self, actor: &Actor, query: &str, max: usize) -> Result<Vec<ThreadSummary>> {
        self.act(actor, "read", "search", query, || {
            let me = self.account()?;
            let mut ts = self.gmail()?.list_threads(query, max, &me)?;
            for t in &mut ts {
                t.account = me.clone();
            }
            Ok(ts)
        })
    }

    pub fn thread(&self, actor: &Actor, id: &str) -> Result<ThreadDetail> {
        self.act(actor, "read", "read_thread", id, || {
            let mut t = self.gmail()?.thread(id)?;
            t.account = self.account()?;
            Ok(t)
        })
    }

    pub fn labels(&self, actor: &Actor) -> Result<Vec<Label>> {
        self.act(actor, "read", "list_labels", "", || self.gmail()?.labels())
    }

    /// Resolve recipients/subject/template into a ready-to-send message.
    pub fn compose(&self, req: &SendReq) -> Result<Composed> {
        let me = self.account()?;
        let mut to = req.to.clone();
        let mut subject = req.subject.clone();
        let mut in_reply_to = String::new();
        let mut thread_subject = String::new();

        if let Some(tid) = &req.reply_to_thread {
            let t = self.gmail()?.thread(tid)?;
            thread_subject = t.subject.clone();
            let last_external = t
                .messages
                .iter()
                .rev()
                .find(|m| !gmail::split_address(&m.from).1.eq_ignore_ascii_case(&me))
                .or(t.messages.last())
                .ok_or_else(|| anyhow!("thread {tid} has no messages"))?;
            in_reply_to = last_external.message_id.clone();
            if to.trim().is_empty() {
                to = if gmail::split_address(&last_external.from).1.eq_ignore_ascii_case(&me) {
                    last_external.to.clone()
                } else {
                    last_external.from.clone()
                };
            }
            if subject.trim().is_empty() {
                subject = if t.subject.to_lowercase().starts_with("re:") {
                    t.subject.clone()
                } else {
                    format!("Re: {}", t.subject)
                };
            }
        }
        if to.trim().is_empty() {
            bail!("no recipient: pass `to` or `reply_to_thread`");
        }

        let mut body = req.body.clone();
        let mut missing = Vec::new();
        if let Some(name) = &req.template {
            let tpl = self.store.template(name)?.ok_or_else(|| anyhow!("no template named '{name}'"))?;
            let first = to.split(',').next().unwrap_or_default();
            let (to_name, to_email) = gmail::split_address(first);
            let contact = self.store.contact(&to_email)?;
            let ctx = templates::context(contact.as_ref(), &to_email, &to_name, &me, &thread_subject, &req.vars);
            let r = templates::render(&tpl.subject, &tpl.body, &ctx);
            // Replies keep "Re: …"; fresh mail takes the template subject unless one was given.
            if req.reply_to_thread.is_none() && req.subject.trim().is_empty() && !r.subject.trim().is_empty() {
                subject = r.subject;
            }
            body = if body.trim().is_empty() { r.body } else { format!("{}\n\n{}", body.trim_end(), r.body) };
            missing = r.missing;
        }
        if body.trim().is_empty() {
            bail!("empty body: pass `body` or `template`");
        }
        Ok(Composed {
            to,
            cc: req.cc.clone(),
            bcc: req.bcc.clone(),
            subject,
            body,
            thread_id: req.reply_to_thread.clone(),
            in_reply_to,
            missing_vars: missing,
        })
    }

    fn raw(c: &Composed) -> String {
        gmail::build_raw(&Outgoing {
            to: &c.to,
            cc: &c.cc,
            bcc: &c.bcc,
            subject: &c.subject,
            body: &c.body,
            in_reply_to: &c.in_reply_to,
        })
    }

    pub fn send(&self, actor: &Actor, req: &SendReq) -> Result<String> {
        let detail = format!("to={} subject={:?}", req.to, req.subject);
        self.act(actor, "send", "send", &detail, || {
            let c = self.compose(req)?;
            if !c.missing_vars.is_empty() {
                bail!("template placeholders unfilled: {} — pass them in `vars`", c.missing_vars.join(", "));
            }
            self.gmail()?.send(&Self::raw(&c), c.thread_id.as_deref())
        })
    }

    pub fn draft(&self, actor: &Actor, req: &SendReq) -> Result<(String, Composed)> {
        let detail = format!("to={} subject={:?}", req.to, req.subject);
        self.act(actor, "draft", "draft", &detail, || {
            let c = self.compose(req)?;
            let id = self.gmail()?.create_draft(&Self::raw(&c), c.thread_id.as_deref())?;
            Ok((id, c))
        })
    }

    fn modify(&self, actor: &Actor, action: &str, id: &str, add: &[&str], remove: &[&str]) -> Result<()> {
        self.act(actor, "modify", action, id, || {
            let add: Vec<String> = add.iter().map(|s| s.to_string()).collect();
            let remove: Vec<String> = remove.iter().map(|s| s.to_string()).collect();
            self.gmail()?.modify_thread(id, &add, &remove)
        })
    }

    pub fn archive(&self, a: &Actor, id: &str) -> Result<()> {
        self.modify(a, "archive", id, &[], &["INBOX"])
    }
    pub fn star(&self, a: &Actor, id: &str, on: bool) -> Result<()> {
        if on {
            self.modify(a, "star", id, &["STARRED"], &[])
        } else {
            self.modify(a, "unstar", id, &[], &["STARRED"])
        }
    }
    pub fn mark_read(&self, a: &Actor, id: &str, read: bool) -> Result<()> {
        if read {
            self.modify(a, "mark_read", id, &[], &["UNREAD"])
        } else {
            self.modify(a, "mark_unread", id, &["UNREAD"], &[])
        }
    }
    pub fn trash(&self, a: &Actor, id: &str) -> Result<()> {
        self.modify(a, "trash", id, &["TRASH"], &["INBOX"])
    }

    /// Add (or with `remove`, take off) a user label by name; creates it if needed.
    pub fn label(&self, actor: &Actor, id: &str, name: &str, remove: bool) -> Result<()> {
        let action = if remove { "unlabel" } else { "label" };
        self.act(actor, "modify", action, &format!("{id} {name}"), || {
            let g = self.gmail()?;
            let existing = g.labels()?.into_iter().find(|l| l.name.eq_ignore_ascii_case(name));
            let label = match existing {
                Some(l) => l,
                None if remove => bail!("no label named '{name}'"),
                None => g.create_label(name)?,
            };
            if remove {
                g.modify_thread(id, &[], &[label.id])
            } else {
                g.modify_thread(id, &[label.id], &[])
            }
        })
    }

    // ---------- contacts ----------

    pub fn contacts(&self, actor: &Actor, query: Option<&str>) -> Result<Vec<Contact>> {
        self.act(actor, "read", "list_contacts", query.unwrap_or(""), || {
            let all = self.store.contacts()?;
            Ok(match query.map(str::to_lowercase) {
                Some(q) if !q.is_empty() => all
                    .into_iter()
                    .filter(|c| serde_json::to_string(c).unwrap_or_default().to_lowercase().contains(&q))
                    .collect(),
                _ => all,
            })
        })
    }

    pub fn contact(&self, actor: &Actor, email: &str) -> Result<Option<Contact>> {
        self.act(actor, "read", "get_contact", email, || self.store.contact(email))
    }

    /// Merge `patch` into the stored profile (non-empty fields win; tags and
    /// custom fields are merged; a field set to "" in `fields` removes it).
    pub fn upsert_contact(&self, actor: &Actor, patch: Contact) -> Result<Contact> {
        let email = patch.email.trim().to_lowercase();
        self.act(actor, "manage", "upsert_contact", &email, || {
            if !email.contains('@') {
                bail!("contact email '{email}' is not an address");
            }
            let mut c = self.store.contact(&email)?.unwrap_or_default();
            c.email = email.clone();
            for (dst, src) in [
                (&mut c.name, patch.name),
                (&mut c.company, patch.company),
                (&mut c.role, patch.role),
                (&mut c.relationship, patch.relationship),
                (&mut c.tone, patch.tone),
                (&mut c.notes, patch.notes),
                (&mut c.learned, patch.learned),
            ] {
                if !src.is_empty() {
                    *dst = src;
                }
            }
            for t in patch.tags {
                if !c.tags.contains(&t) {
                    c.tags.push(t);
                }
            }
            for (k, v) in patch.fields {
                if v.is_empty() {
                    c.fields.remove(&k);
                } else {
                    c.fields.insert(k, v);
                }
            }
            c.updated_at = now();
            self.store.save_contact(&c)?;
            Ok(c)
        })
    }

    /// Store `c` exactly as given (the profile editor, where clearing a field means clearing it).
    pub fn replace_contact(&self, actor: &Actor, mut c: Contact) -> Result<Contact> {
        c.email = c.email.trim().to_lowercase();
        self.act(actor, "manage", "replace_contact", &c.email.clone(), || {
            if !c.email.contains('@') {
                bail!("contact email '{}' is not an address", c.email);
            }
            c.updated_at = now();
            self.store.save_contact(&c)?;
            Ok(c)
        })
    }

    pub fn delete_contact(&self, actor: &Actor, email: &str) -> Result<()> {
        self.act(actor, "manage", "delete_contact", email, || self.store.delete_contact(email))
    }

    /// Frequent real (non-bulk) correspondents that don't have a profile yet.
    pub fn suggest_contacts(&self, actor: &Actor, scan: usize) -> Result<Vec<serde_json::Value>> {
        let threads = self.search(actor, "-category:promotions -category:social", scan)?;
        let mut counts: BTreeMap<String, (String, usize, String)> = BTreeMap::new();
        for t in threads.iter().filter(|t| !t.is_bulk) {
            let e = counts.entry(t.from_email.clone()).or_insert((t.from_name.clone(), 0, t.subject.clone()));
            e.1 += 1;
        }
        let me = self.account()?;
        let mut out: Vec<_> = counts
            .into_iter()
            .filter(|(email, _)| !email.eq_ignore_ascii_case(&me) && !email.is_empty())
            .filter(|(email, _)| self.store.contact(email).ok().flatten().is_none())
            .map(|(email, (name, n, subj))| serde_json::json!({"email": email, "name": name, "threads": n, "latest_subject": subj}))
            .collect();
        out.sort_by(|a, b| b["threads"].as_u64().cmp(&a["threads"].as_u64()));
        Ok(out)
    }

    /// Rebuild the address book for this account from recent mail: people you
    /// wrote to (sent mail) and real senders who wrote to you.
    pub fn sync_people(&self, actor: &Actor) -> Result<usize> {
        self.act(actor, "read", "sync_people", "", || {
            let g = self.gmail()?;
            let me = self.account()?;
            let mut seen = g.correspondents("in:sent", 400, &me)?;
            seen.extend(g.correspondents("-in:sent -category:promotions -category:social -category:updates -category:forums", 200, &me)?);
            let mine = self.store.accounts()?;
            let mut by: BTreeMap<String, Person> = BTreeMap::new();
            for (name, email, date, from_me) in seen {
                if mine.iter().any(|a| a.eq_ignore_ascii_case(&email)) {
                    continue;
                }
                let p = by.entry(email.clone()).or_insert_with(|| Person { email: email.clone(), ..Default::default() });
                p.score += if from_me { 3 } else { 1 };
                // Keep the newest real name (sent-mail headers are often bare addresses).
                let real = !name.is_empty() && !name.eq_ignore_ascii_case(&email);
                if real && (date >= p.last_seen || p.name.is_empty()) {
                    p.name = name;
                }
                p.last_seen = p.last_seen.max(date);
            }
            let people: Vec<Person> = by.into_values().collect();
            self.store.save_people(&me, &people)?;
            self.store.set_setting(&format!("people_synced:{me}"), &chrono::Utc::now().timestamp().to_string())?;
            Ok(people.len())
        })
    }

    /// True when this account's address book is missing or over `hours` old.
    pub fn people_stale(&self, hours: i64) -> bool {
        let Ok(me) = self.account() else { return false };
        let at = self.store.setting(&format!("people_synced:{me}")).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        at.is_none_or(|t| chrono::Utc::now().timestamp() - t > hours * 3600)
    }

    /// Sync first if this account has never been scanned (CLI / agent entry points).
    pub fn ensure_people(&self, actor: &Actor) -> Result<()> {
        if self.people_stale(24 * 365 * 100) {
            self.sync_people(actor)?;
        }
        Ok(())
    }

    /// Search saved profiles plus everyone seen in mail, best match first.
    /// Every word of `query` must appear in the name, address or company; an
    /// empty query lists everyone by how often you trade mail.
    pub fn find_people(&self, actor: &Actor, query: &str, limit: usize) -> Result<Vec<Person>> {
        self.act(actor, "read", "find_people", query, || {
            let mut by: BTreeMap<String, Person> = self.store.people()?.into_iter().map(|p| (p.email.clone(), p)).collect();
            for c in self.store.contacts()? {
                let p = by.entry(c.email.clone()).or_insert_with(|| Person { email: c.email.clone(), ..Default::default() });
                p.profile = true;
                if !c.name.is_empty() {
                    p.name = c.name;
                }
                p.company = c.company;
            }
            let mine = self.store.accounts()?;
            let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
            let mut hits: Vec<(i64, Person)> = by
                .into_values()
                .filter(|p| !mine.iter().any(|a| a.eq_ignore_ascii_case(&p.email)))
                .filter_map(|p| {
                    let hay = format!("{} {} {}", p.name, p.email, p.company).to_lowercase();
                    if !words.iter().all(|w| hay.contains(w.as_str())) {
                        return None;
                    }
                    // Word-start matches ("jan" → "Jane", "jane@…") beat mid-word ones.
                    let starts = words.first().is_some_and(|w| {
                        hay.split(|c: char| !c.is_alphanumeric()).any(|part| part.starts_with(w.as_str()))
                    });
                    let rank = if starts { 1_000_000 } else { 0 } + if p.profile { 100 } else { 0 } + p.score;
                    Some((rank, p))
                })
                .collect();
            hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.last_seen.cmp(&a.1.last_seen)));
            Ok(hits.into_iter().take(limit).map(|(_, p)| p).collect())
        })
    }

    // ---------- templates ----------

    pub fn templates(&self, actor: &Actor) -> Result<Vec<Template>> {
        self.act(actor, "read", "list_templates", "", || self.store.templates())
    }

    pub fn template(&self, actor: &Actor, name: &str) -> Result<Option<Template>> {
        self.act(actor, "read", "get_template", name, || self.store.template(name))
    }

    pub fn save_template(&self, actor: &Actor, mut t: Template) -> Result<Template> {
        self.act(actor, "manage", "save_template", &t.name.clone(), || {
            if t.name.trim().is_empty() || t.body.trim().is_empty() {
                bail!("a template needs a name and a body");
            }
            t.updated_at = now();
            self.store.save_template(&t)?;
            Ok(t)
        })
    }

    pub fn delete_template(&self, actor: &Actor, name: &str) -> Result<()> {
        self.act(actor, "manage", "delete_template", name, || self.store.delete_template(name))
    }

    // ---------- routines ----------

    pub fn routines(&self, actor: &Actor) -> Result<Vec<Routine>> {
        self.act(actor, "read", "list_routines", "", || self.store.routines())
    }

    pub fn save_routine(&self, actor: &Actor, mut r: Routine) -> Result<Routine> {
        self.act(actor, "manage", "save_routine", &r.name.clone(), || {
            if r.name.trim().is_empty() || r.query.trim().is_empty() || r.actions.is_empty() {
                bail!("a routine needs a name, a Gmail query and at least one action");
            }
            if let Some(a) = &r.account {
                r.account = Some(self.store.tokens(Some(a))?.ok_or_else(|| not_signed_in(Some(a)))?.account);
            }
            for a in &r.actions {
                match a {
                    RoutineAction::Draft { template, .. } | RoutineAction::Send { template, .. } => {
                        if self.store.template(template)?.is_none() {
                            bail!("routine references unknown template '{template}'");
                        }
                    }
                    _ => {}
                }
            }
            if let Some(old) = self.store.routine(&r.name)? {
                r.last_run = old.last_run;
            }
            r.every_minutes = r.every_minutes.max(1);
            self.store.save_routine(&r)?;
            Ok(r)
        })
    }

    pub fn delete_routine(&self, actor: &Actor, name: &str) -> Result<()> {
        self.act(actor, "manage", "delete_routine", name, || self.store.delete_routine(name))
    }

    /// Run routines. `force` ignores schedules; `dry_run` only reports matches.
    pub fn run_routines(&self, only: Option<&str>, force: bool, dry_run: bool) -> Result<Vec<RoutineReport>> {
        let mut reports = Vec::new();
        for mut r in self.store.routines()? {
            if only.is_some_and(|n| n != r.name) {
                continue;
            }
            let mut rep = RoutineReport { routine: r.name.clone(), skipped: None, matched: 0, results: vec![] };
            if !r.enabled && only.is_none() {
                rep.skipped = Some("disabled".into());
                reports.push(rep);
                continue;
            }
            let due = r
                .last_run
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| chrono::Local::now().signed_duration_since(t).num_minutes() >= r.every_minutes as i64)
                .unwrap_or(true);
            if !due && !force {
                rep.skipped = Some("not due".into());
                reports.push(rep);
                continue;
            }
            let actor = Actor::Routine(r.name.clone());
            // A routine tied to another account runs as that account.
            let other;
            let eng = match &r.account {
                Some(a) if self.account().map_or(true, |me| !me.eq_ignore_ascii_case(a)) => {
                    other = self.for_account(a)?;
                    &other
                }
                _ => self,
            };
            let threads = match eng.search(&actor, &r.query, 25) {
                Ok(ts) => ts,
                Err(e) => {
                    rep.skipped = Some(format!("error: {e:#}"));
                    reports.push(rep);
                    continue;
                }
            };
            for t in threads {
                if self.store.routine_seen(&r.name, &t.id)?.is_some_and(|d| d >= t.date) {
                    continue;
                }
                rep.matched += 1;
                if dry_run {
                    rep.results.push(format!("would act on {} — {} ({})", t.id, t.subject, t.from_email));
                    continue;
                }
                for action in &r.actions {
                    let res = eng.run_action(&actor, action, &t);
                    rep.results.push(match res {
                        Ok(msg) => format!("{}: {msg}", t.id),
                        Err(e) => format!("{}: ERROR {e}", t.id),
                    });
                }
                self.store.mark_routine_seen(&r.name, &t.id, t.date)?;
            }
            if !dry_run {
                r.last_run = Some(now());
                self.store.save_routine(&r)?;
            }
            reports.push(rep);
        }
        Ok(reports)
    }

    fn run_action(&self, actor: &Actor, action: &RoutineAction, t: &ThreadSummary) -> Result<String> {
        let reply = |template: &str, vars: &BTreeMap<String, String>| SendReq {
            reply_to_thread: Some(t.id.clone()),
            template: Some(template.to_string()),
            vars: vars.clone(),
            ..Default::default()
        };
        match action {
            RoutineAction::Label { name } => self.label(actor, &t.id, name, false).map(|_| format!("labelled {name}")),
            RoutineAction::Archive => self.archive(actor, &t.id).map(|_| "archived".into()),
            RoutineAction::Star => self.star(actor, &t.id, true).map(|_| "starred".into()),
            RoutineAction::MarkRead => self.mark_read(actor, &t.id, true).map(|_| "marked read".into()),
            RoutineAction::Draft { template, vars } => {
                self.draft(actor, &reply(template, vars)).map(|(id, _)| format!("drafted {id}"))
            }
            RoutineAction::Send { template, vars } => {
                self.send(actor, &reply(template, vars)).map(|id| format!("sent {id}"))
            }
            RoutineAction::Notify { message } => {
                let msg = if message.is_empty() {
                    format!("{}: {}", t.from_name, t.subject)
                } else {
                    message.replace("{{from}}", &t.from_name).replace("{{subject}}", &t.subject)
                };
                std::process::Command::new("notify-send")
                    .args(["-a", "Sunbox", "Sunbox", &msg])
                    .status()
                    .context("running notify-send")?;
                Ok("notified".into())
            }
            RoutineAction::Exec { command } => self.act(actor, "exec", "exec", command, || {
                use std::io::Write;
                let mut child = std::process::Command::new("sh")
                    .args(["-c", command])
                    .env("SUNBOX_THREAD_ID", &t.id)
                    .env("SUNBOX_FROM", &t.from_email)
                    .env("SUNBOX_SUBJECT", &t.subject)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(serde_json::to_string(t)?.as_bytes());
                }
                let out = child.wait_with_output()?;
                let text = String::from_utf8_lossy(&out.stdout).trim().chars().take(300).collect::<String>();
                if !out.status.success() {
                    bail!("exit {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
                }
                Ok(format!("exec ok {text}"))
            }),
        }
    }
}

fn not_signed_in(account: Option<&str>) -> anyhow::Error {
    match account {
        Some(a) => anyhow!("{a} isn't signed in — run `sunbox auth login` and pick it, or see `sunbox auth list`"),
        None => anyhow!("not signed in — run `sunbox auth login`"),
    }
}
