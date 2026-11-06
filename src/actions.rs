//! Running the buttons Claude suggests on a briefing item: calendar events, tasks, links,
//! unsubscribing, and the usual archive / star / label. Everything happens on a press, as
//! the user; nothing here runs on its own.

use crate::engine::{Actor, Engine, SendReq};
use crate::models::SuggestedAction;
use anyhow::{anyhow, bail, Result};
use std::process::{Command, Stdio};

/// What the TUI should do after a press.
pub enum Outcome {
    /// Done; show this.
    Done(String),
    /// Open the composer with this (forwards are checked before sending).
    Compose(SendReq),
}

impl Engine {
    pub fn press_action(&self, actor: &Actor, thread_id: &str, a: &SuggestedAction) -> Result<Outcome> {
        let me = self.account()?;
        let mail_link = format!("https://mail.google.com/mail/u/{me}/#all/{thread_id}");
        let done = |s: &str| Ok(Outcome::Done(s.to_string()));
        match a.kind.as_str() {
            "calendar" => {
                open_url(&calendar_url(a, &me, &mail_link)?)?;
                done("Opened the event in Google Calendar — check it and press Save")
            }
            "task" => {
                let to = self.cfg.tasks.email.trim();
                if to.is_empty() {
                    bail!("no task address yet — set [tasks] email in {} to your TickTick add-by-email address", crate::config::config_path().display());
                }
                let title = if a.title.is_empty() { &a.label } else { &a.title };
                // TickTick reads a date in the subject as the due date.
                let subject = if a.start.is_empty() { title.clone() } else { format!("{title} {}", a.start.replace('T', " ")) };
                let body = format!("{}\n\nEmail: {mail_link}", a.note);
                self.send(actor, &SendReq { to: to.to_string(), subject, body, ..Default::default() })?;
                done(&format!("Task added: {title}"))
            }
            "link" => {
                open_url(&a.url)?;
                done("Opened in your browser")
            }
            "unsubscribe" => {
                let how = self.unsubscribe(actor, thread_id)?;
                self.archive(actor, thread_id)?;
                done(&format!("{how}, and archived"))
            }
            "archive" => {
                self.archive(actor, thread_id)?;
                done("Archived")
            }
            "star" => {
                self.star(actor, thread_id, true)?;
                done("Starred")
            }
            "label" => {
                let name = if a.title.is_empty() { &a.label } else { &a.title };
                self.label(actor, thread_id, name, false)?;
                done(&format!("Labelled {name}"))
            }
            "forward" => {
                let t = self.thread(actor, thread_id)?;
                let body = format!("{}\n\n---------- Forwarded message ----------\n{}", a.note, t.transcript);
                Ok(Outcome::Compose(SendReq { to: a.to.clone(), subject: format!("Fwd: {}", t.subject), body, ..Default::default() }))
            }
            k => Err(anyhow!("Sunbox doesn't know how to do '{k}' yet")),
        }
    }

    /// Unsubscribe from the list a thread came from: RFC 8058 one-click when offered,
    /// else the unsubscribe email, else the web page.
    fn unsubscribe(&self, actor: &Actor, thread_id: &str) -> Result<String> {
        let hs = self.gmail()?.latest_headers(thread_id, &["List-Unsubscribe", "List-Unsubscribe-Post"])?;
        let list = hs.first().map(|h| h.as_str()).unwrap_or_default();
        let one_click = hs.get(1).is_some_and(|p| p.to_lowercase().contains("one-click"));
        let targets: Vec<&str> = list.split(',').map(|s| s.trim().trim_start_matches('<').trim_end_matches('>')).filter(|s| !s.is_empty()).collect();
        let https = targets.iter().find(|t| t.starts_with("https://"));
        let mailto = targets.iter().find(|t| t.starts_with("mailto:"));
        if let (Some(url), true) = (https, one_click) {
            ureq::post(url).send_form(&[("List-Unsubscribe", "One-Click")]).map_err(|e| anyhow!("unsubscribe failed: {e}"))?;
            return Ok("Unsubscribed".into());
        }
        if let Some(m) = mailto {
            let rest = &m["mailto:".len()..];
            let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
            let subject = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("subject="))
                .map(|s| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_default())
                .unwrap_or_else(|| "unsubscribe".into());
            let to = urlencoding::decode(to).map(|c| c.into_owned()).unwrap_or_default();
            self.send(actor, &SendReq { to, subject, body: "unsubscribe".into(), ..Default::default() })?;
            return Ok("Sent the unsubscribe email".into());
        }
        if let Some(url) = https {
            open_url(url)?;
            return Ok("Opened the unsubscribe page".into());
        }
        bail!("this sender doesn't offer an unsubscribe link")
    }
}

/// Google Calendar's prefilled "new event" page, so the user confirms before it's saved.
fn calendar_url(a: &SuggestedAction, account: &str, mail_link: &str) -> Result<String> {
    let all_day = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    let timed = |s: &str| {
        ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"]
            .iter()
            .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok())
    };
    let dates = if let Some(d) = all_day(&a.start) {
        let end = all_day(&a.end).filter(|e| *e > d).unwrap_or(d + chrono::Days::new(1));
        format!("{}/{}", d.format("%Y%m%d"), end.format("%Y%m%d"))
    } else if let Some(t) = timed(&a.start) {
        let end = timed(&a.end).filter(|e| *e > t).unwrap_or(t + chrono::Duration::hours(1));
        format!("{}/{}", t.format("%Y%m%dT%H%M%S"), end.format("%Y%m%dT%H%M%S"))
    } else {
        bail!("Claude didn't give this event a date it could read ('{}')", a.start);
    };
    let title = if a.title.is_empty() { &a.label } else { &a.title };
    let details = format!("{}\n\n{mail_link}", a.note).trim().to_string();
    let enc = |s: &str| urlencoding::encode(s).into_owned();
    Ok(format!(
        "https://calendar.google.com/calendar/render?action=TEMPLATE&text={}&dates={dates}&details={}&location={}&authuser={}",
        enc(title),
        enc(&details),
        enc(&a.location),
        enc(account)
    ))
}

/// Open a web page in the user's browser (http/https only).
pub fn open_url(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("not a web link: {url}");
    }
    Command::new("xdg-open").arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_links() {
        let a = SuggestedAction { kind: "calendar".into(), title: "Rehearsal".into(), start: "2026-11-08T15:00".into(), location: "St. Mark".into(), ..Default::default() };
        let u = calendar_url(&a, "me@x.com", "https://mail/x").unwrap();
        assert!(u.contains("dates=20261108T150000/20261108T160000"), "{u}");
        assert!(u.contains("text=Rehearsal") && u.contains("location=St.%20Mark") && u.contains("authuser=me%40x.com"));
        let day = SuggestedAction { start: "2026-11-08".into(), ..a.clone() };
        assert!(calendar_url(&day, "me@x.com", "").unwrap().contains("dates=20261108/20261109"));
        let bad = SuggestedAction { start: "next week".into(), ..a };
        assert!(calendar_url(&bad, "me@x.com", "").is_err());
    }
}
