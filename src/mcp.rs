//! MCP server over stdio (newline-delimited JSON-RPC 2.0).
//! `crayon mcp` — register with e.g. `claude mcp add crayon -- crayon mcp`.

use crate::config::PERMISSIONS;
use crate::engine::{Actor, Engine, SendReq};
use crate::models::{Contact, Routine, Template};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};

const INSTRUCTIONS: &str = "Crayon Box gives you the user's Gmail plus a local store of contact profiles, \
reply templates and routines. Start with `inbox_triage` for an overview, read threads with `get_thread` \
(the `transcript` field is the compact LLM-friendly view), and check `get_contact` before writing to someone — \
profiles carry tone and notes. Prefer `create_draft` unless the user asked you to send. Templates use \
{{placeholders}}: name, first_name, email, my_email, date, subject, contact.<field>, plus any `vars` you pass. \
Routines pair a Gmail search query with actions (label, archive, star, mark_read, draft, send, notify, exec) \
and run on a schedule. Everything you do is permission-checked and written to an audit log the user can see.";

fn send_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "to": {"type": "string", "description": "Comma-separated recipients. Optional for replies (defaults to the sender)."},
            "cc": {"type": "string"},
            "bcc": {"type": "string"},
            "subject": {"type": "string", "description": "Optional for replies and templated mail."},
            "body": {"type": "string", "description": "Plain text. Optional when `template` is set (otherwise prepended to it)."},
            "reply_to_thread": {"type": "string", "description": "Thread id to reply within."},
            "template": {"type": "string", "description": "Template name to render."},
            "vars": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Template placeholder values."}
        }
    })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn tools() -> Value {
    let thread = json!({"thread_id": {"type": "string"}});
    let contact_props = json!({
        "email": {"type": "string"},
        "name": {"type": "string"},
        "company": {"type": "string"},
        "role": {"type": "string"},
        "relationship": {"type": "string", "description": "client, family, venue, vendor, colleague…"},
        "tone": {"type": "string", "description": "How to write to them, e.g. 'warm, brief, no emoji'."},
        "tags": {"type": "array", "items": {"type": "string"}},
        "notes": {"type": "string"},
        "fields": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Custom fields (\"\" deletes one); usable as {{contact.<key>}}."}
    });
    let action = json!({
        "type": "object",
        "description": "One of: {type:label,name} {type:archive} {type:star} {type:mark_read} {type:draft,template,vars?} {type:send,template,vars?} {type:notify,message?} {type:exec,command}",
        "properties": {
            "type": {"type": "string", "enum": ["label", "archive", "star", "mark_read", "draft", "send", "notify", "exec"]},
            "name": {"type": "string"}, "template": {"type": "string"},
            "vars": {"type": "object", "additionalProperties": {"type": "string"}},
            "message": {"type": "string"}, "command": {"type": "string"}
        },
        "required": ["type"]
    });
    let t = |name: &str, desc: &str, mut schema: Value| {
        // Every tool can act as any signed-in account.
        schema["properties"]["account"] = json!({"type": "string", "description": "Signed-in account (email) to act as; the default account when omitted. `whoami` lists them."});
        json!({"name": name, "description": desc, "inputSchema": schema})
    };
    json!([
        t("whoami", "Signed-in accounts (and the default), plus the permissions agents currently hold.", obj(json!({}), &[])),
        t("inbox_triage", "Inbox overview: threads needing a reply, threads waiting on others, and bulk/newsletters.",
          obj(json!({"max": {"type": "integer", "default": 40}}), &[])),
        t("search_threads", "Search mail with Gmail query syntax (e.g. 'from:x is:unread newer_than:7d'). Defaults to the inbox.",
          obj(json!({"query": {"type": "string", "default": "in:inbox"}, "max": {"type": "integer", "default": 25}}), &[])),
        t("get_thread", "Read a thread. format=transcript (default, compact) or full (every message with headers, body, attachments).",
          obj(json!({"thread_id": {"type": "string"}, "format": {"type": "string", "enum": ["transcript", "full"]}}), &["thread_id"])),
        t("preview_email", "Resolve recipients/subject/template exactly as send would, without sending. Reports missing template vars.", send_schema()),
        t("create_draft", "Save a Gmail draft (new mail or a reply) for the user to review.", send_schema()),
        t("send_email", "Send mail now (new or reply). Requires the 'send' permission.", send_schema()),
        t("archive_thread", "Remove a thread from the inbox.", obj(thread.clone(), &["thread_id"])),
        t("star_thread", "Star (to-do) or unstar a thread.",
          obj(json!({"thread_id": {"type": "string"}, "starred": {"type": "boolean", "default": true}}), &["thread_id"])),
        t("mark_read", "Mark a thread read or unread.",
          obj(json!({"thread_id": {"type": "string"}, "read": {"type": "boolean", "default": true}}), &["thread_id"])),
        t("label_thread", "Add (or remove) a user label by name; created if missing.",
          obj(json!({"thread_id": {"type": "string"}, "label": {"type": "string"}, "remove": {"type": "boolean"}}), &["thread_id", "label"])),
        t("list_labels", "User-created Gmail labels.", obj(json!({}), &[])),
        t("list_contacts", "Contact profiles, optionally filtered by a text query.", obj(json!({"query": {"type": "string"}}), &[])),
        t("get_contact", "One contact profile by email.", obj(json!({"email": {"type": "string"}}), &["email"])),
        t("upsert_contact", "Create or update a contact profile. Only provided fields change; tags and fields merge.", obj(contact_props, &["email"])),
        t("delete_contact", "Delete a contact profile.", obj(json!({"email": {"type": "string"}}), &["email"])),
        t("suggest_contacts", "Frequent real correspondents that have no profile yet — good candidates to create.",
          obj(json!({"scan": {"type": "integer", "default": 100}}), &[])),
        t("list_templates", "All reply templates.", obj(json!({}), &[])),
        t("get_template", "One template by name.", obj(json!({"name": {"type": "string"}}), &["name"])),
        t("save_template", "Create or replace a template. Body/subject may use {{placeholders}}.",
          obj(json!({"name": {"type": "string"}, "description": {"type": "string"}, "subject": {"type": "string"}, "body": {"type": "string"}}), &["name", "body"])),
        t("delete_template", "Delete a template.", obj(json!({"name": {"type": "string"}}), &["name"])),
        t("render_template", "Render a template for a recipient without sending; shows unfilled placeholders.",
          obj(json!({"name": {"type": "string"}, "to": {"type": "string"}, "vars": {"type": "object", "additionalProperties": {"type": "string"}}}), &["name", "to"])),
        t("list_routines", "All routines with schedule and last run.", obj(json!({}), &[])),
        t("save_routine", "Create or replace a routine: a Gmail query + ordered actions, run every N minutes on new matches.",
          obj(json!({
              "name": {"type": "string"}, "description": {"type": "string"},
              "query": {"type": "string", "description": "Gmail search selecting threads, e.g. 'in:inbox from:@venue.org'"},
              "every_minutes": {"type": "integer", "default": 15},
              "enabled": {"type": "boolean", "default": true},
              "actions": {"type": "array", "items": action}
          }), &["name", "query", "actions"])),
        t("delete_routine", "Delete a routine.", obj(json!({"name": {"type": "string"}}), &["name"])),
        t("run_routines", "Run routines now (ignores schedule). dry_run lists what would match.",
          obj(json!({"name": {"type": "string"}, "dry_run": {"type": "boolean", "default": true}}), &[])),
        t("get_audit_log", "Recent actions taken by agents, routines and the user.", obj(json!({"limit": {"type": "integer", "default": 30}}), &[])),
    ])
}

fn s<'a>(a: &'a Value, k: &str) -> &'a str {
    a[k].as_str().unwrap_or_default()
}

fn req_str<'a>(a: &'a Value, k: &str) -> Result<&'a str> {
    a[k].as_str().filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("missing required argument `{k}`"))
}

fn call(e: &Engine, actor: &Actor, name: &str, a: &Value) -> Result<Value> {
    let max = |d: u64| a["max"].as_u64().unwrap_or(d) as usize;
    Ok(match name {
        "whoami" => json!({
            "account": e.account()?,
            "accounts": e.accounts()?,
            "default_account": e.default_account()?,
            "agent_permissions": e.cfg.agent.allow,
            "all_permissions": PERMISSIONS.iter().map(|(p, d)| json!({"name": p, "meaning": d})).collect::<Vec<_>>(),
        }),
        "inbox_triage" => {
            let threads = e.search(actor, "in:inbox", max(40))?;
            let pick = |f: &dyn Fn(&&crate::models::ThreadSummary) -> bool| {
                threads.iter().filter(f).map(|t| json!({
                    "thread_id": t.id, "from": format!("{} <{}>", t.from_name, t.from_email),
                    "subject": t.subject, "snippet": t.snippet, "unread": t.unread, "starred": t.starred,
                    "date": chrono::DateTime::from_timestamp_millis(t.date).map(|d| d.to_rfc3339()),
                })).collect::<Vec<_>>()
            };
            json!({
                "needs_reply": pick(&|t| t.needs_reply),
                "waiting_on_others": pick(&|t| !t.needs_reply && !t.is_bulk),
                "bulk": pick(&|t| t.is_bulk),
            })
        }
        "search_threads" => {
            let q = a["query"].as_str().unwrap_or("in:inbox");
            json!(e.search(actor, q, max(25))?)
        }
        "get_thread" => {
            let t = e.thread(actor, req_str(a, "thread_id")?)?;
            if s(a, "format") == "full" {
                json!(t)
            } else {
                json!({"thread_id": t.id, "subject": t.subject, "message_count": t.messages.len(), "transcript": t.transcript})
            }
        }
        "preview_email" => json!(e.compose(&serde_json::from_value::<SendReq>(a.clone())?)?),
        "create_draft" => {
            let (id, c) = e.draft(actor, &serde_json::from_value(a.clone())?)?;
            json!({"draft_id": id, "message": c})
        }
        "send_email" => json!({"message_id": e.send(actor, &serde_json::from_value(a.clone())?)?}),
        "archive_thread" => {
            e.archive(actor, req_str(a, "thread_id")?)?;
            json!({"ok": true})
        }
        "star_thread" => {
            e.star(actor, req_str(a, "thread_id")?, a["starred"].as_bool().unwrap_or(true))?;
            json!({"ok": true})
        }
        "mark_read" => {
            e.mark_read(actor, req_str(a, "thread_id")?, a["read"].as_bool().unwrap_or(true))?;
            json!({"ok": true})
        }
        "label_thread" => {
            e.label(actor, req_str(a, "thread_id")?, req_str(a, "label")?, a["remove"].as_bool().unwrap_or(false))?;
            json!({"ok": true})
        }
        "list_labels" => json!(e.labels(actor)?),
        "list_contacts" => json!(e.contacts(actor, a["query"].as_str())?),
        "get_contact" => json!(e.contact(actor, req_str(a, "email")?)?),
        "upsert_contact" => json!(e.upsert_contact(actor, serde_json::from_value::<Contact>(a.clone())?)?),
        "delete_contact" => {
            e.delete_contact(actor, req_str(a, "email")?)?;
            json!({"ok": true})
        }
        "suggest_contacts" => json!(e.suggest_contacts(actor, a["scan"].as_u64().unwrap_or(100) as usize)?),
        "list_templates" => json!(e.templates(actor)?),
        "get_template" => json!(e.template(actor, req_str(a, "name")?)?),
        "save_template" => json!(e.save_template(actor, serde_json::from_value::<Template>(a.clone())?)?),
        "delete_template" => {
            e.delete_template(actor, req_str(a, "name")?)?;
            json!({"ok": true})
        }
        "render_template" => {
            let req = SendReq {
                to: req_str(a, "to")?.into(),
                template: Some(req_str(a, "name")?.into()),
                vars: serde_json::from_value(a["vars"].clone()).unwrap_or_default(),
                ..Default::default()
            };
            json!(e.compose(&req)?)
        }
        "list_routines" => json!(e.routines(actor)?),
        "save_routine" => json!(e.save_routine(actor, serde_json::from_value::<Routine>(a.clone())?)?),
        "delete_routine" => {
            e.delete_routine(actor, req_str(a, "name")?)?;
            json!({"ok": true})
        }
        "run_routines" => {
            let dry = a["dry_run"].as_bool().unwrap_or(true);
            if !dry {
                e.cfg.require("manage")?;
            }
            json!(e.run_routines(a["name"].as_str(), true, dry)?)
        }
        "get_audit_log" => json!(e.store.audit_log(a["limit"].as_u64().unwrap_or(30) as usize)?),
        other => return Err(anyhow!("unknown tool '{other}'")),
    })
}

/// Handle one JSON-RPC message. Returns `None` for notifications.
pub fn handle(engine: &Engine, actor: &mut Actor, msg: &Value, rename: bool) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or_default();
    let params = &msg["params"];
    let result: std::result::Result<Value, (i64, String)> = match method {
        "initialize" => {
            if let (true, Some(n)) = (rename, params["clientInfo"]["name"].as_str()) {
                *actor = Actor::Agent(n.to_string());
            }
            Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "crayon-box", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = if params["arguments"].is_object() { params["arguments"].clone() } else { json!({}) };
            let other = match args["account"].as_str().filter(|a| !a.is_empty()) {
                Some(a) => Some(engine.for_account(a)),
                None => None,
            };
            let res = match other {
                Some(Ok(e)) => call(&e, actor, name, &args),
                Some(Err(e)) => Err(e),
                None => call(engine, actor, name, &args),
            };
            Ok(match res {
                Ok(v) => json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]}),
                Err(e) => json!({"content": [{"type": "text", "text": format!("Error: {e:#}")}], "isError": true}),
            })
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}}),
    })
}

pub fn serve(engine: &Engine) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut actor = Actor::Agent("mcp".into());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(engine, &mut actor, &msg, true),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(resp) = resp {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Streamable-HTTP MCP for hosted agents (POST JSON-RPC, JSON responses, no SSE).
/// Auth: `Authorization: Bearer <token>`, or the token as the last path segment
/// (`/mcp/<token>`) for clients that only take a URL.
pub fn serve_http(engine: &Engine, bind: &str, token: &str, agent: &str) -> Result<()> {
    use tiny_http::{Header, Response, Server};
    let server = Server::http(bind).map_err(|e| anyhow!("binding {bind}: {e}"))?;
    eprintln!("Crayon MCP (HTTP) listening on http://{bind}/mcp as agent '{agent}'");
    let json_ct: Header = "Content-Type: application/json".parse().unwrap();

    for mut req in server.incoming_requests() {
        let url = req.url().split('?').next().unwrap_or("/").trim_end_matches('/').to_string();
        if url == "/health" {
            let _ = req.respond(Response::from_string("ok"));
            continue;
        }
        let path_token = url.strip_prefix("/mcp/").unwrap_or("");
        if url != "/mcp" && path_token.is_empty() {
            let _ = req.respond(Response::from_string("not found").with_status_code(404));
            continue;
        }
        let bearer = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string))
            .unwrap_or_default();
        if !(same(&bearer, token) || same(path_token, token)) {
            engine.store.audit(&format!("agent:{agent}"), "http_auth", "rejected request with bad token", false);
            let _ = req.respond(Response::from_string("unauthorized").with_status_code(401));
            continue;
        }
        if req.method() != &tiny_http::Method::Post {
            // No server-initiated stream; DELETE/GET are allowed to 405 per the spec.
            let _ = req.respond(Response::from_string("").with_status_code(405));
            continue;
        }
        let mut body = String::new();
        if req.as_reader().take(4 << 20).read_to_string(&mut body).is_err() {
            let _ = req.respond(Response::from_string("bad body").with_status_code(400));
            continue;
        }
        let mut actor = Actor::Agent(agent.to_string());
        let reply = match serde_json::from_str::<Value>(&body) {
            Ok(Value::Array(batch)) => {
                let out: Vec<Value> = batch.iter().filter_map(|m| handle(engine, &mut actor, m, false)).collect();
                (!out.is_empty()).then(|| Value::Array(out))
            }
            Ok(msg) => handle(engine, &mut actor, &msg, false),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        let _ = match reply {
            Some(v) => {
                let mut resp = Response::from_string(v.to_string()).with_header(json_ct.clone());
                if body.contains("\"initialize\"") {
                    let sid = format!("Mcp-Session-Id: {}", rand_hex(16));
                    resp = resp.with_header(sid.parse::<Header>().unwrap());
                }
                req.respond(resp)
            }
            None => req.respond(Response::empty(202)),
        };
    }
    Ok(())
}

pub fn rand_hex(n: usize) -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    (0..n).map(|_| format!("{:02x}", r.gen::<u8>())).collect()
}
