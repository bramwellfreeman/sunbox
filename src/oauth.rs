//! Google OAuth for installed apps: loopback redirect + PKCE + refresh.

use crate::config::{Config, SCOPES};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    pub refresh_token: Option<String>,
}

fn random(n: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(n).map(char::from).collect()
}

/// Runs the browser consent flow and returns the token response.
/// `quiet` keeps it off stderr, for callers that own the terminal (the TUI).
pub fn sign_in(cfg: &Config, open_browser: bool, quiet: bool) -> Result<TokenResponse> {
    if cfg.google.client_secret.is_empty() {
        bail!(
            "no Google client secret configured. Run `crayon auth setup` (or set CRAYON_GOOGLE_CLIENT_SECRET)"
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random(24);

    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={challenge}\
         &code_challenge_method=S256&access_type=offline&prompt=select_account%20consent&state={state}",
        urlencoding::encode(&cfg.google.client_id),
        urlencoding::encode(&redirect),
        urlencoding::encode(SCOPES),
    );

    if !quiet {
        eprintln!("Open this URL to sign in with Google:\n\n  {url}\n");
    }
    if open_browser {
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    if !quiet {
        eprintln!("Waiting for Google to redirect back…");
    }

    let code = loop {
        let (mut stream, _) = listener.accept()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        // "GET /?code=...&state=... HTTP/1.1"
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let param = |k: &str| {
            query.split('&').find_map(|kv| {
                let (key, v) = kv.split_once('=')?;
                (key == k).then(|| urlencoding::decode(v).map(|s| s.into_owned()).unwrap_or_default())
            })
        };
        if param("state").is_none() && param("code").is_none() && param("error").is_none() {
            // favicon or similar
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        }
        let (status, msg) = if let Some(err) = param("error") {
            ("400 Bad Request", format!("Sign-in failed: {err}"))
        } else if param("state").as_deref() != Some(&state) {
            ("400 Bad Request", "State mismatch — please retry.".into())
        } else {
            ("200 OK", "Crayon Box is signed in. You can close this tab.".into())
        };
        let body = format!(
            "<html><body style=\"font-family:sans-serif;padding:3em\"><h2>✏️ {msg}</h2></body></html>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        if let Some(err) = param("error") {
            bail!("Google returned error: {err}");
        }
        if param("state").as_deref() != Some(&state) {
            bail!("OAuth state mismatch");
        }
        break param("code").ok_or_else(|| anyhow!("no code in redirect"))?;
    };

    let resp = ureq::post(TOKEN_URL).send_form(&[
        ("code", code.as_str()),
        ("client_id", &cfg.google.client_id),
        ("client_secret", &cfg.google.client_secret),
        ("redirect_uri", &redirect),
        ("grant_type", "authorization_code"),
        ("code_verifier", &verifier),
    ]);
    parse_token(resp).context("exchanging authorization code")
}

pub fn refresh(cfg: &Config, refresh_token: &str) -> Result<TokenResponse> {
    let resp = ureq::post(TOKEN_URL).send_form(&[
        ("client_id", cfg.google.client_id.as_str()),
        ("client_secret", &cfg.google.client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ]);
    parse_token(resp).context("refreshing access token (try `crayon auth login` again)")
}

fn parse_token(resp: Result<ureq::Response, ureq::Error>) -> Result<TokenResponse> {
    match resp {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(code, r)) => {
            bail!("token endpoint HTTP {code}: {}", r.into_string().unwrap_or_default())
        }
        Err(e) => Err(e.into()),
    }
}
