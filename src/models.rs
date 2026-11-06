use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
