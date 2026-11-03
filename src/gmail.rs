//! Thin blocking Gmail REST client + payload parsing.

use crate::cleaning::clean_message;
use crate::models::*;
use anyhow::{bail, Result};
use base64::alphabet::URL_SAFE;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use serde_json::{json, Value};

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const PARALLEL: usize = 10;

/// Gmail emits base64url both with and without padding.
const B64URL: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub struct Gmail {
    token: String,
    agent: ureq::Agent,
}

impl Gmail {
    pub fn new(token: String) -> Self {
        Self { token, agent: ureq::AgentBuilder::new().build() }
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let mut req = self.agent.get(&format!("{API}{path}")).set("Authorization", &format!("Bearer {}", self.token));
        for (k, v) in query {
            req = req.query(k, v);
        }
        check(req.call())
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let req = self.agent.post(&format!("{API}{path}")).set("Authorization", &format!("Bearer {}", self.token));
        check(req.send_json(body))
    }

    pub fn profile_email(&self) -> Result<String> {
        Ok(self.get("/profile", &[])?["emailAddress"].as_str().unwrap_or_default().to_string())
    }

    /// Thread summaries matching a Gmail search query, newest first.
    pub fn list_threads(&self, query: &str, max: usize, me: &str) -> Result<Vec<ThreadSummary>> {
        let max_s = max.min(500).to_string();
        let mut q: Vec<(&str, &str)> = vec![("maxResults", &max_s)];
        if !query.is_empty() {
            q.push(("q", query));
        }
        let list = self.get("/threads", &q)?;
        let ids: Vec<String> = list["threads"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t["id"].as_str().map(String::from)).collect())
            .unwrap_or_default();

        // Fetch metadata PARALLEL-at-a-time; sequential is ~5x slower.
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(PARALLEL) {
            let results: Vec<Result<ThreadSummary>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk.iter().map(|id| s.spawn(|| self.thread_summary(id, me))).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for r in results {
                out.push(r?);
            }
        }
        Ok(out)
    }

    fn thread_summary(&self, id: &str, me: &str) -> Result<ThreadSummary> {
        let mut q = vec![("format", "metadata")];
        for h in ["From", "To", "Subject", "Date", "List-Id", "List-Unsubscribe", "Precedence", "Auto-Submitted"] {
            q.push(("metadataHeaders", h));
        }
        let t = self.get(&format!("/threads/{id}"), &q)?;
        let msgs = t["messages"].as_array().cloned().unwrap_or_default();
        let first = msgs.first().cloned().unwrap_or(Value::Null);
        let last = msgs.last().cloned().unwrap_or(Value::Null);
        let labels: Vec<String> = msgs
            .iter()
            .flat_map(|m| m["labelIds"].as_array().cloned().unwrap_or_default())
            .filter_map(|l| l.as_str().map(String::from))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let (from_name, from_email) = split_address(&header(&last, "From"));
        // Gmail's own tabs are the strongest signal: anything outside Primary is bulk.
        let categorized = labels.iter().any(|l| {
            matches!(l.as_str(), "CATEGORY_PROMOTIONS" | "CATEGORY_UPDATES" | "CATEGORY_SOCIAL" | "CATEGORY_FORUMS")
        });
        let is_bulk = categorized || is_bulk(&last, &from_email);
        Ok(ThreadSummary {
            id: id.to_string(),
            subject: header(&first, "Subject"),
            snippet: html_unescape(last["snippet"].as_str().unwrap_or_default()),
            date: last["internalDate"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0),
            message_count: msgs.len(),
            unread: labels.iter().any(|l| l == "UNREAD"),
            starred: labels.iter().any(|l| l == "STARRED"),
            needs_reply: !is_bulk && !from_email.eq_ignore_ascii_case(me) && !from_email.is_empty(),
            is_bulk,
            from_name,
            from_email,
            labels,
        })
    }

    pub fn thread(&self, id: &str) -> Result<ThreadDetail> {
        let t = self.get(&format!("/threads/{id}"), &[("format", "full")])?;
        let messages: Vec<Message> = t["messages"]
            .as_array()
            .map(|a| a.iter().map(parse_message).collect())
            .unwrap_or_default();
        let subject = messages.first().map(|m| m.subject.clone()).unwrap_or_default();
        let transcript = messages
            .iter()
            .map(|m| {
                let when = chrono::DateTime::from_timestamp_millis(m.date)
                    .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                format!("{} · {}\n{}", m.from, when, m.clean_body.trim())
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        Ok(ThreadDetail { id: id.to_string(), subject, messages, transcript })
    }

    pub fn send(&self, raw: &str, thread_id: Option<&str>) -> Result<String> {
        let mut body = json!({ "raw": raw });
        if let Some(t) = thread_id {
            body["threadId"] = json!(t);
        }
        Ok(self.post("/messages/send", body)?["id"].as_str().unwrap_or_default().to_string())
    }

    pub fn create_draft(&self, raw: &str, thread_id: Option<&str>) -> Result<String> {
        let mut msg = json!({ "raw": raw });
        if let Some(t) = thread_id {
            msg["threadId"] = json!(t);
        }
        Ok(self.post("/drafts", json!({ "message": msg }))?["id"].as_str().unwrap_or_default().to_string())
    }

    pub fn modify_thread(&self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        self.post(&format!("/threads/{id}/modify"), json!({ "addLabelIds": add, "removeLabelIds": remove }))?;
        Ok(())
    }

    pub fn labels(&self) -> Result<Vec<Label>> {
        let v = self.get("/labels", &[])?;
        let mut labels: Vec<Label> = v["labels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|l| l["type"] == "user")
                    .map(|l| Label {
                        id: l["id"].as_str().unwrap_or_default().into(),
                        name: l["name"].as_str().unwrap_or_default().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        labels.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(labels)
    }

    pub fn create_label(&self, name: &str) -> Result<Label> {
        let v = self.post("/labels", json!({ "name": name }))?;
        Ok(Label { id: v["id"].as_str().unwrap_or_default().into(), name: name.into() })
    }
}

fn check(resp: Result<ureq::Response, ureq::Error>) -> Result<Value> {
    match resp {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(401, _)) => bail!("Gmail rejected the token (401) — run `crayon auth login`"),
        Err(ureq::Error::Status(403, r)) => {
            let body = r.into_string().unwrap_or_default();
            if body.contains("insufficient") || body.contains("SCOPE") {
                bail!("Gmail permission missing — run `crayon auth login` to re-consent");
            }
            bail!("Gmail HTTP 403: {body}")
        }
        Err(ureq::Error::Status(code, r)) => bail!("Gmail HTTP {code}: {}", r.into_string().unwrap_or_default()),
        Err(e) => Err(e.into()),
    }
}

// ---------- parsing ----------

fn header(msg: &Value, name: &str) -> String {
    msg["payload"]["headers"]
        .as_array()
        .and_then(|hs| hs.iter().find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))))
        .and_then(|h| h["value"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// "Jane Doe <jane@x.com>" → ("Jane Doe", "jane@x.com").
pub fn split_address(s: &str) -> (String, String) {
    let s = s.trim();
    if let (Some(lt), Some(gt)) = (s.rfind('<'), s.rfind('>')) {
        if lt < gt {
            let name = s[..lt].trim().trim_matches('"').to_string();
            let email = s[lt + 1..gt].trim().to_lowercase();
            return (if name.is_empty() { email.clone() } else { name }, email);
        }
    }
    (s.to_string(), s.to_lowercase())
}

fn is_bulk(msg: &Value, from_email: &str) -> bool {
    let has = |h: &str| !header(msg, h).is_empty();
    let prec = header(msg, "Precedence").to_lowercase();
    let auto = header(msg, "Auto-Submitted").to_lowercase();
    let local = from_email.split('@').next().unwrap_or_default();
    has("List-Id")
        || has("List-Unsubscribe")
        || matches!(prec.as_str(), "bulk" | "list" | "junk")
        || (!auto.is_empty() && auto != "no")
        || ["noreply", "no-reply", "donotreply", "do-not-reply", "notifications", "mailer-daemon"]
            .iter()
            .any(|p| local.contains(p))
}

fn parse_message(m: &Value) -> Message {
    let mut plain = String::new();
    let mut html = String::new();
    let mut attachments = Vec::new();
    walk(&m["payload"], &mut plain, &mut html, &mut attachments);
    let body = if !plain.trim().is_empty() {
        plain
    } else if !html.is_empty() {
        html2text::from_read(html.as_bytes(), 100)
    } else {
        String::new()
    };
    Message {
        id: m["id"].as_str().unwrap_or_default().into(),
        thread_id: m["threadId"].as_str().unwrap_or_default().into(),
        message_id: header(m, "Message-ID"),
        from: header(m, "From"),
        to: header(m, "To"),
        cc: header(m, "Cc"),
        subject: header(m, "Subject"),
        date: m["internalDate"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0),
        clean_body: clean_message(&body),
        body,
        attachments,
        labels: m["labelIds"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }
}

fn walk(part: &Value, plain: &mut String, html: &mut String, atts: &mut Vec<Attachment>) {
    let mime = part["mimeType"].as_str().unwrap_or_default();
    let filename = part["filename"].as_str().unwrap_or_default();
    if !filename.is_empty() {
        atts.push(Attachment {
            filename: filename.into(),
            mime_type: mime.into(),
            size: part["body"]["size"].as_u64().unwrap_or(0),
        });
        return;
    }
    if let Some(parts) = part["parts"].as_array() {
        for p in parts {
            walk(p, plain, html, atts);
        }
        return;
    }
    let data = part["body"]["data"].as_str().unwrap_or_default();
    if data.is_empty() {
        return;
    }
    let text = B64URL.decode(data).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    match mime {
        "text/plain" if plain.is_empty() => *plain = text,
        "text/html" if html.is_empty() => *html = text,
        _ => {}
    }
}

fn html_unescape(s: &str) -> String {
    s.replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

// ---------- outgoing ----------

pub struct Outgoing<'a> {
    pub to: &'a str,
    pub cc: &'a str,
    pub bcc: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub in_reply_to: &'a str,
}

/// RFC822 message, base64url-encoded for the Gmail API `raw` field.
pub fn build_raw(o: &Outgoing) -> String {
    let mut msg = String::new();
    msg.push_str(&format!("To: {}\r\n", o.to));
    if !o.cc.trim().is_empty() {
        msg.push_str(&format!("Cc: {}\r\n", o.cc));
    }
    if !o.bcc.trim().is_empty() {
        msg.push_str(&format!("Bcc: {}\r\n", o.bcc));
    }
    msg.push_str(&format!("Subject: {}\r\n", encode_header(o.subject)));
    if !o.in_reply_to.is_empty() {
        msg.push_str(&format!("In-Reply-To: {0}\r\nReferences: {0}\r\n", o.in_reply_to));
    }
    msg.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    let body = STANDARD.encode(o.body.replace("\r\n", "\n").replace('\n', "\r\n"));
    for chunk in body.as_bytes().chunks(76) {
        msg.push_str(std::str::from_utf8(chunk).unwrap());
        msg.push_str("\r\n");
    }
    B64URL.encode(msg)
}

fn encode_header(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_addresses() {
        assert_eq!(split_address("\"Jane Doe\" <Jane@X.com>"), ("Jane Doe".into(), "jane@x.com".into()));
        assert_eq!(split_address("bob@y.org"), ("bob@y.org".into(), "bob@y.org".into()));
    }

    #[test]
    fn raw_round_trips() {
        let raw = build_raw(&Outgoing { to: "a@b.c", cc: "", bcc: "", subject: "Héllo", body: "hi\nthere", in_reply_to: "" });
        let decoded = String::from_utf8(B64URL.decode(raw).unwrap()).unwrap();
        assert!(decoded.contains("Subject: =?UTF-8?B?"));
        assert!(decoded.contains("To: a@b.c\r\n"));
    }
}
