//! Local SQLite store: tokens, contacts, templates, routines, audit log.
//! Rows are JSON blobs keyed by a natural id so the schema can grow freely.

use crate::config;
use crate::models::*;
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

pub struct Tokens {
    pub account: String,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_at: i64,
}

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

impl Store {
    pub fn open() -> Result<Self> {
        let dir = config::data_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("crayon.db");
        let conn = Connection::open(&path)?;
        config::restrict(&path);
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS tokens (account TEXT PRIMARY KEY, refresh_token TEXT NOT NULL,
                 access_token TEXT NOT NULL DEFAULT '', expires_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS contacts (email TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS templates (name TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS routines (name TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS routine_seen (routine TEXT NOT NULL, thread_id TEXT NOT NULL,
                 last_date INTEGER NOT NULL, PRIMARY KEY (routine, thread_id));
             CREATE TABLE IF NOT EXISTS audit (id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT NOT NULL,
                 actor TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL, ok INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    // ---- tokens ----

    /// Tokens for `account`, or for the default account when `None`.
    pub fn tokens(&self, account: Option<&str>) -> Result<Option<Tokens>> {
        let account = match account {
            Some(a) => a.to_string(),
            None => match self.default_account()? {
                Some(a) => a,
                None => return Ok(None),
            },
        };
        Ok(self
            .db()
            .query_row(
                "SELECT account, refresh_token, access_token, expires_at FROM tokens WHERE account = ?1 COLLATE NOCASE",
                [&account],
                |r| {
                    Ok(Tokens {
                        account: r.get(0)?,
                        refresh_token: r.get(1)?,
                        access_token: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Signed-in accounts, oldest first.
    pub fn accounts(&self) -> Result<Vec<String>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT account FROM tokens ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The chosen default, falling back to the first account signed in.
    pub fn default_account(&self) -> Result<Option<String>> {
        let accounts = self.accounts()?;
        let chosen = self.setting("default_account")?;
        Ok(chosen
            .and_then(|c| accounts.iter().find(|a| a.eq_ignore_ascii_case(&c)).cloned())
            .or_else(|| accounts.into_iter().next()))
    }

    pub fn set_default_account(&self, account: &str) -> Result<()> {
        self.set_setting("default_account", account)
    }

    /// Adds the account, or replaces its tokens if it's already signed in.
    pub fn save_tokens(&self, t: &Tokens) -> Result<()> {
        self.db().execute(
            "INSERT INTO tokens VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account) DO UPDATE SET
                 refresh_token = excluded.refresh_token, access_token = excluded.access_token,
                 expires_at = excluded.expires_at",
            params![t.account, t.refresh_token, t.access_token, t.expires_at],
        )?;
        Ok(())
    }

    pub fn update_access(&self, account: &str, access: &str, expires_at: i64) -> Result<()> {
        self.db().execute(
            "UPDATE tokens SET access_token = ?2, expires_at = ?3 WHERE account = ?1",
            params![account, access, expires_at],
        )?;
        Ok(())
    }

    /// Returns false when that account wasn't signed in.
    pub fn remove_tokens(&self, account: &str) -> Result<bool> {
        Ok(self.db().execute("DELETE FROM tokens WHERE account = ?1 COLLATE NOCASE", [account])? > 0)
    }

    // ---- settings ----

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO settings VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- generic JSON tables ----

    fn put<T: Serialize>(&self, table: &str, key_col: &str, key: &str, v: &T) -> Result<()> {
        self.db().execute(
            &format!("INSERT INTO {table} ({key_col}, data) VALUES (?1, ?2)
                      ON CONFLICT({key_col}) DO UPDATE SET data = excluded.data"),
            params![key, serde_json::to_string(v)?],
        )?;
        Ok(())
    }

    fn get<T: DeserializeOwned>(&self, table: &str, key_col: &str, key: &str) -> Result<Option<T>> {
        let raw: Option<String> = self
            .db()
            .query_row(&format!("SELECT data FROM {table} WHERE {key_col} = ?1"), [key], |r| r.get(0))
            .optional()?;
        raw.map(|s| serde_json::from_str(&s).map_err(Into::into)).transpose()
    }

    fn all<T: DeserializeOwned>(&self, table: &str, key_col: &str) -> Result<Vec<T>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!("SELECT data FROM {table} ORDER BY {key_col}"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }

    fn remove(&self, table: &str, key_col: &str, key: &str) -> Result<()> {
        let n = self.db().execute(&format!("DELETE FROM {table} WHERE {key_col} = ?1"), [key])?;
        if n == 0 {
            return Err(anyhow!("no {} named '{key}'", table.trim_end_matches('s')));
        }
        Ok(())
    }

    // ---- contacts ----

    pub fn contacts(&self) -> Result<Vec<Contact>> {
        self.all("contacts", "email")
    }
    pub fn contact(&self, email: &str) -> Result<Option<Contact>> {
        self.get("contacts", "email", &email.to_lowercase())
    }
    pub fn save_contact(&self, c: &Contact) -> Result<()> {
        self.put("contacts", "email", &c.email.to_lowercase(), c)
    }
    pub fn delete_contact(&self, email: &str) -> Result<()> {
        self.remove("contacts", "email", &email.to_lowercase())
    }

    // ---- templates ----

    pub fn templates(&self) -> Result<Vec<Template>> {
        self.all("templates", "name")
    }
    pub fn template(&self, name: &str) -> Result<Option<Template>> {
        self.get("templates", "name", name)
    }
    pub fn save_template(&self, t: &Template) -> Result<()> {
        self.put("templates", "name", &t.name, t)
    }
    pub fn delete_template(&self, name: &str) -> Result<()> {
        self.remove("templates", "name", name)
    }

    // ---- routines ----

    pub fn routines(&self) -> Result<Vec<Routine>> {
        self.all("routines", "name")
    }
    pub fn routine(&self, name: &str) -> Result<Option<Routine>> {
        self.get("routines", "name", name)
    }
    pub fn save_routine(&self, r: &Routine) -> Result<()> {
        self.put("routines", "name", &r.name, r)
    }
    pub fn delete_routine(&self, name: &str) -> Result<()> {
        self.db().execute("DELETE FROM routine_seen WHERE routine = ?1", [name])?;
        self.remove("routines", "name", name)
    }

    /// Latest-message date we already processed for this thread, if any.
    pub fn routine_seen(&self, routine: &str, thread_id: &str) -> Result<Option<i64>> {
        Ok(self
            .db()
            .query_row(
                "SELECT last_date FROM routine_seen WHERE routine = ?1 AND thread_id = ?2",
                [routine, thread_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn mark_routine_seen(&self, routine: &str, thread_id: &str, date: i64) -> Result<()> {
        self.db().execute(
            "INSERT INTO routine_seen VALUES (?1, ?2, ?3)
             ON CONFLICT(routine, thread_id) DO UPDATE SET last_date = excluded.last_date",
            params![routine, thread_id, date],
        )?;
        Ok(())
    }

    // ---- audit ----

    pub fn audit(&self, actor: &str, action: &str, detail: &str, ok: bool) {
        let _ = self.db().execute(
            "INSERT INTO audit (at, actor, action, detail, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), actor, action, detail, ok],
        );
    }

    pub fn audit_log(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT at, actor, action, detail, ok FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(AuditEntry {
                at: r.get(0)?,
                actor: r.get(1)?,
                action: r.get(2)?,
                detail: r.get(3)?,
                ok: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
