mod ai;
mod assistant;
mod cleaning;
mod config;
mod engine;
mod gmail;
mod mcp;
mod models;
mod oauth;
mod store;
mod templates;
mod tui;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::{Actor, Engine, SendReq};
use models::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};

/// Sunbox — Gmail in your terminal, built for humans and AI agents.
#[derive(Parser)]
#[command(name = "sunbox", version, about)]
struct Cli {
    /// Machine-readable JSON output (for scripts and agents).
    #[arg(long, global = true)]
    json: bool,
    /// Act as this signed-in account (default: the default account, or $SUNBOX_ACCOUNT).
    #[arg(long, global = true, value_name = "EMAIL")]
    account: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive inbox (default when no command is given).
    Tui,
    /// Sign in / out, configure the OAuth client.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// List the inbox. Filters: all, todo (needs reply), primary, bulk, starred.
    Inbox {
        #[arg(long, default_value = "all")]
        filter: String,
        #[arg(long, default_value_t = 30)]
        max: usize,
    },
    /// Search with Gmail query syntax.
    Search {
        query: String,
        #[arg(long, default_value_t = 25)]
        max: usize,
    },
    /// Read a thread (cleaned transcript; --full for every message).
    Read {
        thread_id: String,
        #[arg(long)]
        full: bool,
    },
    /// Send (or --draft) mail. With no --body and no --template, opens $EDITOR.
    Send(SendArgs),
    /// Remove a thread from the inbox.
    Archive { thread_id: String },
    /// Star a thread (Sunbox's "To do"); --off to unstar.
    Star {
        thread_id: String,
        #[arg(long)]
        off: bool,
    },
    /// Mark a thread read (--unread to reverse).
    MarkRead {
        thread_id: String,
        #[arg(long)]
        unread: bool,
    },
    /// Add a user label (created if missing); --remove to take it off.
    Label {
        thread_id: String,
        name: String,
        #[arg(long)]
        remove: bool,
    },
    /// List user labels.
    Labels,
    /// Contact profiles.
    #[command(subcommand)]
    Contacts(ContactCmd),
    /// Reply templates with {{placeholders}}.
    #[command(subcommand)]
    Templates(TemplateCmd),
    /// Automations: Gmail query + actions on a schedule.
    #[command(subcommand)]
    Routines(RoutineCmd),
    /// The built-in Claude assistant: briefings, drafted replies, client profiles, templates.
    #[command(subcommand)]
    Claude(ClaudeCmd),
    /// What AI agents may do, and what they did.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Run the MCP server: stdio (Claude Code, Codex, opencode…) or --http for hosted agents.
    Mcp {
        /// Serve streamable HTTP instead of stdio.
        #[arg(long)]
        http: bool,
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
        /// Agent name used in the audit log.
        #[arg(long, default_value = "remote")]
        name: String,
        /// Print the access token (creating one if needed) and exit.
        #[arg(long)]
        show_token: bool,
        /// Replace the access token (old one stops working) and exit.
        #[arg(long)]
        rotate_token: bool,
    },
}

#[derive(Subcommand)]
enum ClaudeCmd {
    /// Which backend and model the assistant uses.
    Status,
    /// Rank what matters in the inbox (--all: every signed-in inbox).
    Brief {
        #[arg(long)]
        all: bool,
    },
    /// Draft a reply for you to check (nothing is sent).
    Draft {
        thread_id: String,
        /// Steer the reply, e.g. "decline politely".
        #[arg(long)]
        note: Option<String>,
    },
    /// Replies Claude has drafted.
    Drafts,
    /// Read recent mail with someone and update their profile.
    Learn { email: String },
    /// Write a template from an instruction (--from NAME revises one). Saved only with --save.
    Template {
        instruction: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        save: bool,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Save the Google OAuth client id/secret (prompts if not given).
    Setup {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// Sign in with Google in the browser. Run again to add another account.
    Login {
        /// Just print the URL (e.g. over SSH).
        #[arg(long)]
        no_browser: bool,
        /// Make the new account the default.
        #[arg(long)]
        default: bool,
    },
    /// Sign an account out (the --account one, else the default).
    Logout {
        account: Option<String>,
    },
    /// Show the signed-in accounts and the default.
    #[command(alias = "list")]
    Status,
    /// Choose which account commands, the TUI and agents use by default.
    Default { account: String },
}

#[derive(clap::Args)]
struct SendArgs {
    #[arg(long, default_value = "")]
    to: String,
    #[arg(long, default_value = "")]
    cc: String,
    #[arg(long, default_value = "")]
    bcc: String,
    #[arg(long, short, default_value = "")]
    subject: String,
    /// Body text, or "-" to read stdin.
    #[arg(long, short)]
    body: Option<String>,
    /// Reply within this thread id.
    #[arg(long)]
    reply: Option<String>,
    #[arg(long, short)]
    template: Option<String>,
    /// Template variable, repeatable: --var venue="St. Mark's"
    #[arg(long = "var", value_parser = parse_kv)]
    vars: Vec<(String, String)>,
    /// Save as a Gmail draft instead of sending.
    #[arg(long)]
    draft: bool,
    /// Show the resolved message and stop.
    #[arg(long)]
    dry_run: bool,
    /// Don't ask for confirmation.
    #[arg(long, short)]
    yes: bool,
}

#[derive(Subcommand)]
enum ContactCmd {
    List { query: Option<String> },
    /// Search everyone you can write to (saved profiles + people from your mail).
    Find {
        query: Vec<String>,
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// Rebuild the address book from recent mail (runs by itself on first use).
    Sync,
    Get { email: String },
    /// Create/update a profile (only given fields change). Or pipe JSON with --from-json -.
    Set {
        email: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        company: Option<String>,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        relationship: Option<String>,
        #[arg(long)]
        tone: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Custom field, repeatable: --field instrument=cello
        #[arg(long = "field", value_parser = parse_kv)]
        fields: Vec<(String, String)>,
        /// Contact JSON file, or "-" for stdin.
        #[arg(long)]
        from_json: Option<String>,
    },
    Delete { email: String },
    /// Frequent correspondents without a profile.
    Suggest {
        #[arg(long, default_value_t = 100)]
        scan: usize,
    },
}

#[derive(Subcommand)]
enum TemplateCmd {
    List,
    Show { name: String },
    /// Create/replace. Body from --body, --file, or stdin; $EDITOR if none.
    Set {
        name: String,
        #[arg(long, short, default_value = "")]
        subject: String,
        #[arg(long, short)]
        body: Option<String>,
        #[arg(long, short)]
        file: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
    Delete { name: String },
    /// Render for a recipient without sending.
    Render {
        name: String,
        #[arg(long)]
        to: String,
        #[arg(long = "var", value_parser = parse_kv)]
        vars: Vec<(String, String)>,
    },
}

#[derive(Subcommand)]
enum RoutineCmd {
    List,
    Show { name: String },
    /// Create/replace from JSON (file or "-" for stdin). See AGENTS.md for the shape.
    Set { file: String },
    Delete { name: String },
    Enable { name: String },
    Disable { name: String },
    /// Run routines now regardless of schedule.
    Run {
        name: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Run only the routines that are due (what the systemd timer calls).
    Tick,
    /// Install a systemd user timer that ticks routines.
    InstallTimer {
        #[arg(long, default_value_t = 5)]
        every_minutes: u32,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Show agent permissions.
    Permissions,
    Allow { permission: String },
    Deny { permission: String },
    /// Recent agent/routine/user actions.
    Audit {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Register the MCP server with an agent CLI: claude (default), codex.
    Install {
        #[arg(default_value = "claude")]
        client: String,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got '{s}'"))
}

fn main() {
    let cli = Cli::parse();
    config::migrate_from_crayon();
    if let Err(e) = run(cli) {
        eprintln!("sunbox: {e:#}");
        std::process::exit(1);
    }
}

fn print<T: Serialize>(json: bool, v: &T, human: impl FnOnce(&T)) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        human(v);
    }
    Ok(())
}

fn claude_cmd(e: &Engine, json: bool, c: ClaudeCmd) -> Result<()> {
    match c {
        ClaudeCmd::Status => {
            let v = serde_json::json!({"backend": e.claude_backend(), "about": e.cfg.claude.about});
            print(json, &v, |v| {
                println!("{}", v["backend"].as_str().unwrap_or_default());
                if e.cfg.claude.about.is_empty() {
                    println!("Tip: add a few lines about your work as `about` under [claude] in {}", config::config_path().display());
                }
            })?;
        }
        ClaudeCmd::Brief { all } => {
            let accounts = if all { e.accounts()? } else { vec![e.account()?] };
            let mut threads = vec![];
            for a in &accounts {
                threads.extend(e.for_account(a)?.search(&Actor::User, "in:inbox", 60)?);
            }
            eprintln!("Asking Claude about {} threads…", threads.len());
            let b = e.brief(&threads, &accounts)?;
            print(json, &b, |b| {
                println!("{}
", b.summary);
                for i in &b.items {
                    println!("{:>7}  {:<9} {} — {}
         {}", i.priority, i.action, i.from, i.subject, i.why);
                }
                for t in &b.template_ideas {
                    println!("
Template idea “{}”: {}", t.name, t.why);
                }
            })?;
        }
        ClaudeCmd::Draft { thread_id, note } => {
            let d = e.ai_draft(&thread_id, note.as_deref())?;
            print(json, &d, |d| {
                println!("To: {}
Subject: {}

{}", d.to, d.subject, d.body);
                for c in &d.check {
                    println!("  ⚠ check: {c}");
                }
            })?;
        }
        ClaudeCmd::Drafts => {
            let ds = e.ai_drafts()?;
            print(json, &ds, |ds| {
                for d in ds {
                    println!("{:<10} {}  {} — {}", d.status, d.thread_id, d.to, d.subject);
                }
            })?;
        }
        ClaudeCmd::Learn { email } => {
            let c = e.learn_contact(&email)?;
            print(json, &c, |c| println!("{} <{}> · {}

{}", c.name, c.email, c.relationship, c.learned))?;
        }
        ClaudeCmd::Template { instruction, from, save } => {
            let base = match &from {
                Some(n) => Some(e.template(&Actor::User, n)?.ok_or_else(|| anyhow!("no template named {n}"))?),
                None => None,
            };
            let mut t = e.ai_template(&instruction, base.as_ref())?;
            if save {
                t = e.save_template(&Actor::User, t)?;
            }
            print(json, &t, |t| {
                println!("name: {}
description: {}
subject: {}

{}", t.name, t.description, t.subject, t.body);
                if !save {
                    println!("
(not saved — add --save)");
                }
            })?;
        }
    }
    Ok(())
}

fn ok(json: bool, what: &str) {
    if json {
        println!("{}", serde_json::json!({"ok": true}));
    } else {
        println!("✓ {what}");
    }
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    let cmd = cli.cmd.unwrap_or(Cmd::Tui);

    // Commands that must work before sign-in / without an engine.
    if let Cmd::Auth(AuthCmd::Setup { client_id, client_secret }) = &cmd {
        return auth_setup(client_id.clone(), client_secret.clone());
    }

    let e = Engine::open(cli.account)?;
    let me = Actor::User;

    match cmd {
        Cmd::Tui => tui::run(e)?,
        Cmd::Mcp { http: false, show_token: false, rotate_token: false, .. } => mcp::serve(&e)?,
        Cmd::Mcp { bind, name, show_token, rotate_token, .. } => {
            let mut cfg = e.cfg.clone();
            if cfg.mcp.http_token.is_empty() || rotate_token {
                cfg.mcp.http_token = format!("sunbox_{}", mcp::rand_hex(24));
                cfg.save()?;
                e.store.audit("user", "mcp_token", if rotate_token { "rotated" } else { "created" }, true);
            }
            if show_token || rotate_token {
                println!("{}", cfg.mcp.http_token);
            } else {
                mcp::serve_http(&e, &bind, &cfg.mcp.http_token, &name)?;
            }
        }
        Cmd::Auth(a) => match a {
            AuthCmd::Setup { .. } => unreachable!(),
            AuthCmd::Login { no_browser, default } => {
                let acct = e.login(!no_browser, false)?;
                if default || e.accounts()?.len() == 1 {
                    e.set_default_account(&acct)?;
                }
                let is_default = e.default_account()?.as_deref() == Some(acct.as_str());
                ok(json, &format!("signed in as {acct}{}", if is_default { " (default)" } else { "" }));
            }
            AuthCmd::Logout { account } => {
                let gone = e.logout(account.as_deref())?;
                ok(json, &format!("signed out {gone} (local tokens removed)"));
            }
            AuthCmd::Default { account } => {
                let acct = e.set_default_account(&account)?;
                ok(json, &format!("{acct} is now the default account"));
            }
            AuthCmd::Status => {
                let v = serde_json::json!({
                    "signed_in": !e.accounts()?.is_empty(), "accounts": e.accounts()?,
                    "default_account": e.default_account()?, "account": e.account().ok(),
                    "client_secret_configured": !e.cfg.google.client_secret.is_empty(),
                    "config": config::config_path(), "data": config::data_dir(),
                });
                print(json, &v, |v| {
                    let def = v["default_account"].as_str().unwrap_or_default();
                    match v["accounts"].as_array().filter(|a| !a.is_empty()) {
                        Some(accts) => {
                            println!("Signed in:");
                            for a in accts.iter().filter_map(|a| a.as_str()) {
                                println!("  {} {a}", if a == def { "★" } else { " " });
                            }
                            println!("  (★ = default; add one with `sunbox auth login`)");
                        }
                        None => println!("Not signed in"),
                    }
                    println!("Client secret: {}", if v["client_secret_configured"] == true { "configured" } else { "MISSING — run `sunbox auth setup`" });
                    println!("Config: {}\nData:   {}", v["config"].as_str().unwrap_or(""), v["data"].as_str().unwrap_or(""));
                })?;
            }
        },
        Cmd::Inbox { filter, max } => {
            let mut threads = e.search(&me, "in:inbox", max)?;
            threads.retain(|t| match filter.as_str() {
                "todo" => t.needs_reply,
                "primary" => !t.is_bulk,
                "bulk" | "newsletters" => t.is_bulk,
                "starred" => t.starred,
                _ => true,
            });
            print(json, &threads, |ts| print_threads(ts))?;
        }
        Cmd::Search { query, max } => {
            let threads = e.search(&me, &query, max)?;
            print(json, &threads, |ts| print_threads(ts))?;
        }
        Cmd::Read { thread_id, full } => {
            let t = e.thread(&me, &thread_id)?;
            print(json, &t, |t| {
                println!("\x1b[1m{}\x1b[0m  ({} messages)\n", t.subject, t.messages.len());
                if full {
                    for m in &t.messages {
                        println!("\x1b[1mFrom:\x1b[0m {}\n\x1b[1mTo:\x1b[0m {}", m.from, m.to);
                        if !m.cc.is_empty() {
                            println!("\x1b[1mCc:\x1b[0m {}", m.cc);
                        }
                        println!("\x1b[1mDate:\x1b[0m {}\n\n{}\n", fmt_date(m.date, true), m.body.trim());
                        for a in &m.attachments {
                            println!("📎 {} ({}, {} KB)", a.filename, a.mime_type, a.size / 1024);
                        }
                        println!("{}", "─".repeat(60));
                    }
                } else {
                    println!("{}", t.transcript);
                }
            })?;
            let _ = e.mark_read(&me, &thread_id, true);
        }
        Cmd::Send(args) => send_cmd(&e, json, args)?,
        Cmd::Archive { thread_id } => {
            e.archive(&me, &thread_id)?;
            ok(json, "archived");
        }
        Cmd::Star { thread_id, off } => {
            e.star(&me, &thread_id, !off)?;
            ok(json, if off { "unstarred" } else { "starred" });
        }
        Cmd::MarkRead { thread_id, unread } => {
            e.mark_read(&me, &thread_id, !unread)?;
            ok(json, if unread { "marked unread" } else { "marked read" });
        }
        Cmd::Label { thread_id, name, remove } => {
            e.label(&me, &thread_id, &name, remove)?;
            ok(json, &format!("{} label {name}", if remove { "removed" } else { "added" }));
        }
        Cmd::Labels => {
            let labels = e.labels(&me)?;
            print(json, &labels, |ls| ls.iter().for_each(|l| println!("{}", l.name)))?;
        }
        Cmd::Contacts(c) => contacts_cmd(&e, json, c)?,
        Cmd::Claude(c) => claude_cmd(&e, json, c)?,
        Cmd::Templates(t) => templates_cmd(&e, json, t)?,
        Cmd::Routines(r) => routines_cmd(&e, json, r)?,
        Cmd::Agent(a) => agent_cmd(e, json, a)?,
    }
    Ok(())
}

fn auth_setup(id: Option<String>, secret: Option<String>) -> Result<()> {
    let mut cfg = config::Config::load()?;
    if let Some(id) = id {
        cfg.google.client_id = id;
    }
    let secret = match secret {
        Some(s) => s,
        None => {
            eprintln!("Client id: {}", cfg.google.client_id);
            eprint!("Paste the OAuth client secret (GOCSPX-…): ");
            std::io::stderr().flush()?;
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim().to_string()
        }
    };
    if secret.is_empty() {
        bail!("no secret given");
    }
    cfg.google.client_secret = secret;
    cfg.save()?;
    println!("✓ saved to {} — now run `sunbox auth login`", config::config_path().display());
    Ok(())
}

fn read_input(arg: &str) -> Result<String> {
    if arg == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(arg).with_context(|| format!("reading {arg}"))
    }
}

/// Open $EDITOR on `initial` and return what was saved.
pub fn edit_text(initial: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("sunbox-{}.txt", std::process::id()));
    std::fs::write(&path, initial)?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "nvim".into());
    let status = std::process::Command::new("sh")
        .args(["-c", &format!("{editor} \"$1\"", ), "sh"])
        .arg(&path)
        .status()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    if !status.success() {
        bail!("editor exited with {status}");
    }
    Ok(text)
}

fn send_cmd(e: &Engine, json: bool, a: SendArgs) -> Result<()> {
    let mut body = match a.body.as_deref() {
        Some("-") => read_input("-")?,
        Some(b) => b.to_string(),
        None => String::new(),
    };
    if body.is_empty() && a.template.is_none() {
        if !std::io::stdin().is_terminal() {
            bail!("no --body/--template given and no terminal for $EDITOR");
        }
        body = edit_text("")?;
    }
    let req = SendReq {
        to: a.to,
        cc: a.cc,
        bcc: a.bcc,
        subject: a.subject,
        body,
        reply_to_thread: a.reply,
        template: a.template,
        vars: a.vars.into_iter().collect::<BTreeMap<_, _>>(),
    };
    let c = e.compose(&req)?;
    let interactive = std::io::stdin().is_terminal() && !json;
    if a.dry_run || (interactive && !a.yes) {
        print(json, &c, |c| {
            println!("To:      {}", c.to);
            if !c.cc.is_empty() {
                println!("Cc:      {}", c.cc);
            }
            if !c.bcc.is_empty() {
                println!("Bcc:     {}", c.bcc);
            }
            println!("Subject: {}\n\n{}\n", c.subject, c.body.trim_end());
            if !c.missing_vars.is_empty() {
                println!("⚠ unfilled placeholders: {}", c.missing_vars.join(", "));
            }
        })?;
        if a.dry_run {
            return Ok(());
        }
        eprint!("{} this? [y/N] ", if a.draft { "Save draft of" } else { "Send" });
        std::io::stderr().flush()?;
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans)?;
        if !ans.trim().eq_ignore_ascii_case("y") {
            bail!("cancelled");
        }
    }
    if a.draft {
        let (id, _) = e.draft(&Actor::User, &req)?;
        print(json, &serde_json::json!({"draft_id": id}), |_| println!("✓ draft saved ({id})"))
    } else {
        let id = e.send(&Actor::User, &req)?;
        print(json, &serde_json::json!({"message_id": id}), |_| println!("✓ sent ({id})"))
    }
}

fn contacts_cmd(e: &Engine, json: bool, c: ContactCmd) -> Result<()> {
    let me = Actor::User;
    match c {
        ContactCmd::List { query } => {
            let cs = e.contacts(&me, query.as_deref())?;
            print(json, &cs, |cs| {
                if cs.is_empty() {
                    println!("No contact profiles yet. Try `sunbox contacts suggest`.");
                }
                for c in cs {
                    let extra = [c.company.as_str(), c.relationship.as_str()]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .copied()
                        .collect::<Vec<_>>()
                        .join(" · ");
                    println!("{:<34} {:<24} {}", c.email, c.name, extra);
                }
            })?;
        }
        ContactCmd::Find { query, limit } => {
            e.ensure_people(&me)?;
            let ps = e.find_people(&me, &query.join(" "), limit)?;
            print(json, &ps, |ps| {
                if ps.is_empty() {
                    println!("Nobody matches. `sunbox contacts sync` rescans your mail.");
                }
                for p in ps {
                    let mark = if p.profile { "★" } else { " " };
                    println!("{mark} {:<26} {:<36} {}", p.name, p.email, p.company);
                }
            })?;
        }
        ContactCmd::Sync => {
            let n = e.sync_people(&me)?;
            ok(json, &format!("address book: {n} people from recent mail"));
        }
        ContactCmd::Get { email } => {
            let c = e.contact(&me, &email)?.ok_or_else(|| anyhow!("no profile for {email}"))?;
            print(json, &c, |c| println!("{}", toml::to_string_pretty(c).unwrap_or_default()))?;
        }
        ContactCmd::Set { email, name, company, role, relationship, tone, notes, tags, fields, from_json } => {
            let mut patch: Contact = match from_json {
                Some(src) => serde_json::from_str(&read_input(&src)?)?,
                None => Contact::default(),
            };
            if let Some(em) = email {
                patch.email = em;
            }
            let set = |dst: &mut String, v: Option<String>| {
                if let Some(v) = v {
                    *dst = v;
                }
            };
            set(&mut patch.name, name);
            set(&mut patch.company, company);
            set(&mut patch.role, role);
            set(&mut patch.relationship, relationship);
            set(&mut patch.tone, tone);
            set(&mut patch.notes, notes);
            patch.tags.extend(tags);
            patch.fields.extend(fields);
            let saved = e.upsert_contact(&me, patch)?;
            print(json, &saved, |c| println!("✓ saved profile for {}", c.email))?;
        }
        ContactCmd::Delete { email } => {
            e.delete_contact(&me, &email)?;
            ok(json, "deleted");
        }
        ContactCmd::Suggest { scan } => {
            let s = e.suggest_contacts(&me, scan)?;
            print(json, &s, |s| {
                for v in s {
                    println!("{:>3}  {:<34} {}", v["threads"], v["email"].as_str().unwrap_or(""), v["name"].as_str().unwrap_or(""));
                }
            })?;
        }
    }
    Ok(())
}

fn templates_cmd(e: &Engine, json: bool, t: TemplateCmd) -> Result<()> {
    let me = Actor::User;
    match t {
        TemplateCmd::List => {
            let ts = e.templates(&me)?;
            print(json, &ts, |ts| {
                if ts.is_empty() {
                    println!("No templates yet. `sunbox templates set <name>` opens your editor.");
                }
                for t in ts {
                    println!("{:<24} {}", t.name, if t.description.is_empty() { &t.subject } else { &t.description });
                }
            })?;
        }
        TemplateCmd::Show { name } => {
            let t = e.template(&me, &name)?.ok_or_else(|| anyhow!("no template named '{name}'"))?;
            print(json, &t, |t| {
                if !t.subject.is_empty() {
                    println!("Subject: {}\n", t.subject);
                }
                println!("{}", t.body);
            })?;
        }
        TemplateCmd::Set { name, subject, body, file, description } => {
            let body = match (body, file) {
                (Some(b), _) if b == "-" => read_input("-")?,
                (Some(b), _) => b,
                (None, Some(f)) => read_input(&f)?,
                (None, None) if std::io::stdin().is_terminal() => {
                    let existing = e.template(&me, &name)?.map(|t| t.body).unwrap_or_else(|| {
                        "Hi {{first_name}},\n\n\n\nBest,\nBram".to_string()
                    });
                    edit_text(&existing)?
                }
                (None, None) => read_input("-")?,
            };
            let saved = e.save_template(&me, Template { name, subject, body, description, updated_at: String::new() })?;
            print(json, &saved, |t| println!("✓ saved template '{}'", t.name))?;
        }
        TemplateCmd::Delete { name } => {
            e.delete_template(&me, &name)?;
            ok(json, "deleted");
        }
        TemplateCmd::Render { name, to, vars } => {
            let c = e.compose(&SendReq { to, template: Some(name), vars: vars.into_iter().collect(), ..Default::default() })?;
            print(json, &c, |c| {
                println!("Subject: {}\n\n{}", c.subject, c.body);
                if !c.missing_vars.is_empty() {
                    println!("\n⚠ unfilled: {}", c.missing_vars.join(", "));
                }
            })?;
        }
    }
    Ok(())
}

fn routines_cmd(e: &Engine, json: bool, r: RoutineCmd) -> Result<()> {
    let me = Actor::User;
    let toggle = |name: &str, on: bool| -> Result<()> {
        let mut r = e.store.routine(name)?.ok_or_else(|| anyhow!("no routine named '{name}'"))?;
        r.enabled = on;
        e.save_routine(&me, r)?;
        ok(json, if on { "enabled" } else { "disabled" });
        Ok(())
    };
    match r {
        RoutineCmd::List => {
            let rs = e.routines(&me)?;
            print(json, &rs, |rs| {
                if rs.is_empty() {
                    println!("No routines yet. See AGENTS.md, or ask your agent to create one.");
                }
                for r in rs {
                    println!(
                        "{} {:<22} every {:>3}m  {:<40} last: {}",
                        if r.enabled { "●" } else { "○" },
                        r.name,
                        r.every_minutes,
                        r.query,
                        r.last_run.as_deref().unwrap_or("never")
                    );
                }
            })?;
        }
        RoutineCmd::Show { name } => {
            let r = e.store.routine(&name)?.ok_or_else(|| anyhow!("no routine named '{name}'"))?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        RoutineCmd::Set { file } => {
            let r: Routine = serde_json::from_str(&read_input(&file)?).context("routine JSON")?;
            let saved = e.save_routine(&me, r)?;
            print(json, &saved, |r| println!("✓ saved routine '{}'", r.name))?;
        }
        RoutineCmd::Delete { name } => {
            e.delete_routine(&me, &name)?;
            ok(json, "deleted");
        }
        RoutineCmd::Enable { name } => toggle(&name, true)?,
        RoutineCmd::Disable { name } => toggle(&name, false)?,
        RoutineCmd::Run { name, dry_run } => {
            let reps = e.run_routines(name.as_deref(), true, dry_run)?;
            print(json, &reps, print_reports)?;
        }
        RoutineCmd::Tick => {
            let reps = e.run_routines(None, false, false)?;
            print(json, &reps, print_reports)?;
        }
        RoutineCmd::InstallTimer { every_minutes } => install_timer(every_minutes)?,
    }
    Ok(())
}

fn print_reports(reps: &Vec<engine::RoutineReport>) {
    for r in reps {
        match &r.skipped {
            Some(why) => println!("{}: skipped ({why})", r.routine),
            None => {
                println!("{}: {} new match(es)", r.routine, r.matched);
                for line in &r.results {
                    println!("   {line}");
                }
            }
        }
    }
}

fn install_timer(every: u32) -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?.join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("sunbox-routines.service"),
        format!(
            "[Unit]\nDescription=Sunbox routines\n\n[Service]\nType=oneshot\nExecStart={} routines tick\n",
            exe.display()
        ),
    )?;
    std::fs::write(
        dir.join("sunbox-routines.timer"),
        format!(
            "[Unit]\nDescription=Tick Sunbox routines every {every} min\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec={every}min\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
        ),
    )?;
    let st = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .and_then(|_| std::process::Command::new("systemctl").args(["--user", "enable", "--now", "sunbox-routines.timer"]).status())?;
    if !st.success() {
        bail!("systemctl failed ({st})");
    }
    println!("✓ sunbox-routines.timer active (every {every} min). Logs: journalctl --user -u sunbox-routines");
    Ok(())
}

fn agent_cmd(mut e: Engine, json: bool, a: AgentCmd) -> Result<()> {
    match a {
        AgentCmd::Permissions => {
            let v: Vec<_> = config::PERMISSIONS
                .iter()
                .map(|(p, d)| serde_json::json!({"permission": p, "allowed": e.cfg.allows(p), "meaning": d}))
                .collect();
            print(json, &v, |v| {
                for p in v {
                    println!(
                        "{} {:<8} {}",
                        if p["allowed"] == true { "✓" } else { "✗" },
                        p["permission"].as_str().unwrap_or(""),
                        p["meaning"].as_str().unwrap_or("")
                    );
                }
            })?;
        }
        AgentCmd::Allow { permission } | AgentCmd::Deny { permission } if !config::PERMISSIONS.iter().any(|(p, _)| *p == permission) => {
            bail!("unknown permission '{permission}' (one of: {})", config::PERMISSIONS.iter().map(|p| p.0).collect::<Vec<_>>().join(", "));
        }
        AgentCmd::Allow { permission } => {
            if !e.cfg.allows(&permission) {
                e.cfg.agent.allow.push(permission.clone());
            }
            e.cfg.save()?;
            e.store.audit("user", "allow", &permission, true);
            ok(json, &format!("agents may now: {permission}"));
        }
        AgentCmd::Deny { permission } => {
            e.cfg.agent.allow.retain(|p| p != &permission);
            e.cfg.save()?;
            e.store.audit("user", "deny", &permission, true);
            ok(json, &format!("agents may no longer: {permission}"));
        }
        AgentCmd::Audit { limit } => {
            let log = e.store.audit_log(limit)?;
            print(json, &log, |log| {
                for a in log.iter().rev() {
                    println!("{} {} {:<22} {:<16} {}", &a.at[..19.min(a.at.len())], if a.ok { "✓" } else { "✗" }, a.actor, a.action, a.detail);
                }
            })?;
        }
        AgentCmd::Install { client } => {
            let exe = std::env::current_exe()?.display().to_string();
            let args: Vec<&str> = match client.as_str() {
                "claude" => vec!["mcp", "add", "-s", "user", "sunbox", "--", &exe, "mcp"],
                "codex" => vec!["mcp", "add", "sunbox", "--", &exe, "mcp"],
                other => bail!("don't know how to register with '{other}'. Point it at: {exe} mcp"),
            };
            let st = std::process::Command::new(&client).args(&args).status().with_context(|| format!("running {client}"))?;
            if !st.success() {
                bail!("{client} mcp add failed ({st})");
            }
            ok(json, &format!("registered Sunbox MCP server with {client}"));
        }
    }
    Ok(())
}

pub fn fmt_date(ms: i64, long: bool) -> String {
    let Some(d) = chrono::DateTime::from_timestamp_millis(ms) else { return String::new() };
    let d = d.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    if long {
        d.format("%a %b %-d %Y, %-I:%M %p").to_string()
    } else if d.date_naive() == now.date_naive() {
        d.format("%-I:%M %p").to_string()
    } else if now.signed_duration_since(d).num_days() < 7 {
        d.format("%a").to_string()
    } else {
        d.format("%b %-d").to_string()
    }
}

fn print_threads(ts: &Vec<ThreadSummary>) {
    for t in ts {
        let marker = if t.unread { "\x1b[34m●\x1b[0m" } else { " " };
        let flag = if t.needs_reply { "\x1b[33m↩\x1b[0m" } else if t.starred { "★" } else { " " };
        let from: String = t.from_name.chars().take(22).collect();
        let subj: String = t.subject.chars().take(60).collect();
        println!("{marker}{flag} {:<16} {:<22} {:<60} {:>7}", t.id, from, subj, fmt_date(t.date, false));
    }
}
