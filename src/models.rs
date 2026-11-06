use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub snippet: String,
    /// Unix millis of the latest message.
    pub date: i64,
    pub message_count: usize,
    pub unread: bool,
    pub starred: bool,
    /// Latest message isn't from us and isn't bulk mail.
    pub needs_reply: bool,
    /// Newsletters / notifications (List-Id, List-Unsubscribe, no-reply…).
    pub is_bulk: bool,
    pub labels: Vec<String>,
    /// Which signed-in account it lives in.
    #[serde(default)]
    pub account: String,
    /// Who the user last wrote to in this thread (for the Sent view).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    /// RFC822 Message-ID, used for In-Reply-To.
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: i64,
    /// Body with quoted history + signatures stripped.
    pub clean_body: String,
    /// Full plain-text body (HTML converted when there's no text part).
    pub body: String,
    pub attachments: Vec<Attachment>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDetail {
    pub id: String,
    pub subject: String,
    pub messages: Vec<Message>,
    /// Whole conversation flattened as `sender · date\nclean body` blocks —
    /// the cheapest way to hand a thread to an LLM.
    pub transcript: String,
    #[serde(default)]
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub id: String,
    pub name: String,
}

/// Someone you can write to: a saved profile and/or an address seen in your mail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Person {
    pub email: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub company: String,
    /// Has a saved contact profile.
    pub profile: bool,
    /// How often you trade mail with them (emails you sent them count triple).
    pub score: i64,
    /// Unix millis of the latest message with them, 0 if only a profile.
    pub last_seen: i64,
}

impl Person {
    /// "Jane Doe <jane@x.com>", or just the address when there's no real name.
    pub fn address(&self) -> String {
        if self.name.is_empty() || self.name.eq_ignore_ascii_case(&self.email) {
            self.email.clone()
        } else if self.name.contains([',', '"', '<', '>', ';']) {
            format!("\"{}\" <{}>", self.name.replace('"', ""), self.email)
        } else {
            format!("{} <{}>", self.name, self.email)
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Contact {
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub role: String,
    /// How we know them: client, family, venue, vendor…
    #[serde(default)]
    pub relationship: String,
    /// How to write to them (e.g. "formal, no emoji").
    #[serde(default)]
    pub tone: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub notes: String,
    /// Free-form extra fields, usable in templates as {{contact.<key>}}.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// What Claude has learned about them from past mail; editable like any other field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub learned: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Subject line; may use {{placeholders}}. Empty = keep thread subject.
    #[serde(default)]
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoutineAction {
    /// Add a user label (created if missing).
    Label { name: String },
    Archive,
    Star,
    MarkRead,
    /// Reply draft from a template, addressed to the sender.
    Draft {
        template: String,
        #[serde(default)]
        vars: BTreeMap<String, String>,
    },
    /// Reply from a template and send it (needs agent `send`).
    Send {
        template: String,
        #[serde(default)]
        vars: BTreeMap<String, String>,
    },
    /// Desktop notification via notify-send.
    Notify {
        #[serde(default)]
        message: String,
    },
    /// Run a shell command with the thread as JSON on stdin and
    /// SUNBOX_THREAD_ID / SUNBOX_FROM / SUNBOX_SUBJECT in env (needs `exec`).
    Exec { command: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Gmail search query selecting threads, e.g. "from:venue.org is:unread".
    pub query: String,
    /// Run at most every N minutes when `sunbox routines tick` fires.
    #[serde(default = "default_every")]
    pub every_minutes: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub actions: Vec<RoutineAction>,
    /// Which signed-in account it watches; the default account when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default)]
    pub last_run: Option<String>,
}

fn default_every() -> u32 {
    15
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: String,
    pub actor: String,
    pub action: String,
    pub detail: String,
    pub ok: bool,
}

/// Claude's read of the inbox: what matters, what to do, and templates worth having.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Briefing {
    pub created_at: String,
    pub accounts: Vec<String>,
    pub summary: String,
    /// The categories Claude chose for this inbox, in display order.
    #[serde(default)]
    pub categories: Vec<String>,
    pub items: Vec<BriefItem>,
    #[serde(default)]
    pub template_ideas: Vec<TemplateIdea>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BriefItem {
    pub thread_id: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub subject: String,
    /// urgent · high · normal · low
    pub priority: String,
    #[serde(default)]
    pub category: String,
    pub why: String,
    /// reply · follow_up · read · archive · none
    pub action: String,
    /// One-press buttons Claude picked for this email.
    #[serde(default)]
    pub actions: Vec<SuggestedAction>,
    /// The buttons came from reading the whole thread, not just the snippet.
    #[serde(default)]
    pub planned: bool,
}

/// A button Claude suggests. `kind` picks what runs; the other fields are its details
/// ("" when they don't apply).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuggestedAction {
    /// calendar · task · link · unsubscribe · archive · star · label · forward
    pub kind: String,
    /// Button text, e.g. "Add rehearsal Sat 3pm".
    pub label: String,
    /// Event / task title, label name.
    #[serde(default)]
    pub title: String,
    /// Local "2026-11-08T15:00" or an all-day "2026-11-08"; for tasks, the due date.
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub url: String,
    /// Forward recipient.
    #[serde(default)]
    pub to: String,
    /// Event description, task notes, forward note.
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateIdea {
    pub name: String,
    pub description: String,
    pub subject: String,
    pub body: String,
    pub why: String,
}

/// A reply Claude wrote, waiting for the user to check it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiDraft {
    pub thread_id: String,
    pub account: String,
    pub to: String,
    #[serde(default)]
    pub cc: String,
    pub subject: String,
    pub body: String,
    /// Things to verify before sending (dates, prices, [[gaps]] Claude couldn't fill).
    #[serde(default)]
    pub check: Vec<String>,
    #[serde(default)]
    pub template_used: String,
    pub created_at: String,
    /// ready · sent · saved · dismissed
    pub status: String,
}

/// An email the user marked as handled another way (a call, in person, elsewhere): important,
/// but taken care of. Claude reads these as signals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Handled {
    pub thread_id: String,
    pub account: String,
    pub from_name: String,
    pub from_email: String,
    pub subject: String,
    pub note: String,
    pub at: String,
}
