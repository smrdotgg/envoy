//! Google OAuth 2.0 for installed apps: loopback redirect + PKCE.
//!
//! <https://developers.google.com/identity/protocols/oauth2/native-app>

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPES: &str = "https://mail.google.com/ openid email profile";

/// A client id baked in at build time (`ENVOY_GOOGLE_CLIENT_ID=... cargo build`), else read
/// from the environment at run time.
pub fn default_client() -> (String, String) {
    let id = option_env!("ENVOY_GOOGLE_CLIENT_ID").map(str::to_owned).or_else(|| std::env::var("ENVOY_GOOGLE_CLIENT_ID").ok());
    let secret =
        option_env!("ENVOY_GOOGLE_CLIENT_SECRET").map(str::to_owned).or_else(|| std::env::var("ENVOY_GOOGLE_CLIENT_SECRET").ok());
    (id.unwrap_or_default(), secret.unwrap_or_default())
}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Instant,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn random_string(n: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    (0..n).map(|_| CHARS[rand::random_range(0..CHARS.len())] as char).collect()
}

fn token_request(form: &[(&str, &str)]) -> anyhow::Result<Tokens> {
    let mut resp = agent().post(TOKEN_URL).send_form(form.iter().copied()).context("token request failed")?;
    let body: TokenResponse = resp.body_mut().read_json().context("unreadable token response")?;
    if let Some(e) = body.error {
        bail!("Google refused: {e}{}", body.error_description.map(|d| format!(" ({d})")).unwrap_or_default());
    }
    let access_token = body.access_token.ok_or_else(|| anyhow!("no access token in response"))?;
    let (email, name) = body.id_token.as_deref().map(id_token_claims).unwrap_or_default();
    Ok(Tokens {
        access_token,
        refresh_token: body.refresh_token,
        expires_at: Instant::now() + Duration::from_secs(body.expires_in.unwrap_or(3600).saturating_sub(60)),
        email,
        name,
    })
}

fn id_token_claims(jwt: &str) -> (Option<String>, Option<String>) {
    let Some(payload) = jwt.split('.').nth(1) else { return (None, None) };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else { return (None, None) };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return (None, None) };
    (v["email"].as_str().map(str::to_owned), v["name"].as_str().map(str::to_owned))
}

/// Runs the whole browser sign-in. Blocks until the browser redirects back (or 5 minutes).
/// `open_browser` is given the URL to open.
pub fn authorize(client_id: &str, client_secret: &str, login_hint: Option<&str>, open_browser: impl FnOnce(&str)) -> anyhow::Result<Tokens> {
    if client_id.trim().is_empty() {
        bail!("No Google OAuth client id. Create a Desktop app client in Google Cloud Console, or sign in with an app password.");
    }
    let listener = TcpListener::bind("127.0.0.1:0").context("couldn't open a local port for the sign-in redirect")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(24);
    let mut url = url::Url::parse(AUTH_URL)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", &state);
    if let Some(hint) = login_hint.filter(|h| !h.is_empty()) {
        url.query_pairs_mut().append_pair("login_hint", hint);
    }
    open_browser(url.as_str());

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let code = loop {
        if Instant::now() > deadline {
            bail!("Timed out waiting for the browser sign-in");
        }
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        let parsed = url::Url::parse(&format!("http://localhost{path}"))?;
        let get = |k: &str| parsed.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (title, ok) = match (get("code"), get("error")) {
            (Some(_), _) if get("state").as_deref() == Some(state.as_str()) => ("You're signed in. You can close this tab and return to Envoy.", true),
            (_, Some(e)) => (if e == "access_denied" { "Sign-in was cancelled." } else { "Sign-in failed." }, false),
            _ => ("Waiting for sign-in…", false),
        };
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>Envoy</title><body style=\"font:16px -apple-system,system-ui,sans-serif;display:grid;place-items:center;height:90vh;color:#222\"><div><h2>Envoy</h2><p>{title}</p></div>"
        );
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
        if ok {
            break get("code").unwrap_or_default();
        }
        if let Some(e) = get("error") {
            bail!("Google sign-in: {e}");
        }
    };
    token_request(&[
        ("code", code.as_str()),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect.as_str()),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier.as_str()),
    ])
}

pub fn refresh(client_id: &str, client_secret: &str, refresh_token: &str) -> anyhow::Result<Tokens> {
    token_request(&[
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ])
}

/// The SASL XOAUTH2 initial response.
pub fn xoauth2(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}
