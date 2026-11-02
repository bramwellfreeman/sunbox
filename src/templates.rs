//! `{{placeholder}}` rendering for templates.
//!
//! Built-ins: {{name}} {{first_name}} {{email}} {{my_email}} {{date}} {{subject}}
//! Contact profile: {{contact.company}} {{contact.role}} {{contact.<custom field>}} …
//! Plus any caller-supplied vars. Unknown placeholders are left intact and
//! reported so an agent can fill them in.

use crate::models::Contact;
use std::collections::BTreeMap;

pub struct Rendered {
    pub subject: String,
    pub body: String,
    pub missing: Vec<String>,
}

pub fn context(
    contact: Option<&Contact>,
    to_email: &str,
    to_name: &str,
    my_email: &str,
    subject: &str,
    vars: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();
    let name = contact.map(|c| c.name.as_str()).filter(|n| !n.is_empty()).unwrap_or(to_name);
    let name = if name.is_empty() || name.contains('@') { "" } else { name };
    ctx.insert("name".into(), name.to_string());
    ctx.insert("first_name".into(), name.split_whitespace().next().unwrap_or_default().to_string());
    ctx.insert("email".into(), to_email.to_string());
    ctx.insert("my_email".into(), my_email.to_string());
    ctx.insert("subject".into(), subject.to_string());
    ctx.insert("date".into(), chrono::Local::now().format("%B %-d, %Y").to_string());
    if let Some(c) = contact {
        for (k, v) in [
            ("name", &c.name),
            ("company", &c.company),
            ("role", &c.role),
            ("relationship", &c.relationship),
            ("tone", &c.tone),
            ("notes", &c.notes),
        ] {
            ctx.insert(format!("contact.{k}"), v.clone());
        }
        for (k, v) in &c.fields {
            ctx.insert(format!("contact.{k}"), v.clone());
        }
    }
    ctx.extend(vars.clone());
    ctx
}

pub fn render_str(text: &str, ctx: &BTreeMap<String, String>, missing: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        match ctx.get(key).filter(|v| !v.is_empty()) {
            Some(v) => out.push_str(v),
            None => {
                if !missing.iter().any(|m| m == key) {
                    missing.push(key.to_string());
                }
                out.push_str(&rest[start..start + 2 + end + 2]);
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn render(subject: &str, body: &str, ctx: &BTreeMap<String, String>) -> Rendered {
    let mut missing = Vec::new();
    let subject = render_str(subject, ctx, &mut missing);
    let body = render_str(body, ctx, &mut missing);
    Rendered { subject, body, missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_reports_missing() {
        let c = Contact {
            email: "j@x.com".into(),
            name: "Jane Doe".into(),
            company: "Hall".into(),
            ..Default::default()
        };
        let ctx = context(Some(&c), "j@x.com", "", "me@y.com", "Re: hi", &BTreeMap::new());
        let r = render("Hi {{first_name}}", "At {{contact.company}} on {{venue_date}}", &ctx);
        assert_eq!(r.subject, "Hi Jane");
        assert_eq!(r.body, "At Hall on {{venue_date}}");
        assert_eq!(r.missing, vec!["venue_date"]);
    }
}
