//! Accounts, stored in `accounts.json` (no secrets: passwords and tokens live in the OS
//! keychain, see [`crate::secrets`]).

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    /// A password or app password, sent with IMAP LOGIN / SMTP AUTH.
    Password,
    /// Google OAuth 2.0 (XOAUTH2). The refresh token is the secret.
    GoogleOAuth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// Stable id, used for the keychain entry and the local cache.
    pub id: String,
    /// The address mail is sent from.
    pub email: String,
    /// The name recipients see.
    #[serde(default)]
    pub display_name: String,
    pub auth: AuthKind,
    /// The login name (usually the email).
    #[serde(default)]
    pub username: String,
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// A color hue (0–360) for the account's dot in the sidebar.
    #[serde(default)]
    pub hue: f32,
    /// Per-account signature; falls back to `compose.signature` when empty.
    #[serde(default)]
    pub signature: String,
    /// Google OAuth client (a "Desktop app" client from Google Cloud Console). Google treats a
    /// desktop client's secret as non-confidential, so it is kept here, not in the keychain.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub oauth_client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub oauth_client_secret: String,
}

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    465
}

impl Account {
    pub fn is_gmail(&self) -> bool {
        self.imap_host.ends_with("gmail.com") || self.imap_host.ends_with("googlemail.com")
    }

    pub fn login(&self) -> &str {
        if self.username.is_empty() { &self.email } else { &self.username }
    }

    pub fn from_name(&self) -> String {
        if self.display_name.is_empty() { self.email.clone() } else { self.display_name.clone() }
    }
}

/// Server presets by address domain, so adding a common account only needs the address.
pub fn preset_for(email: &str) -> Option<(&'static str, u16, &'static str, u16)> {
    let domain = email.rsplit('@').next()?.to_ascii_lowercase();
    Some(match domain.as_str() {
        "gmail.com" | "googlemail.com" => ("imap.gmail.com", 993, "smtp.gmail.com", 465),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => ("outlook.office365.com", 993, "smtp-mail.outlook.com", 587),
        "icloud.com" | "me.com" | "mac.com" => ("imap.mail.me.com", 993, "smtp.mail.me.com", 587),
        "yahoo.com" | "ymail.com" => ("imap.mail.yahoo.com", 993, "smtp.mail.yahoo.com", 465),
        "fastmail.com" | "fastmail.fm" => ("imap.fastmail.com", 993, "smtp.fastmail.com", 465),
        "proton.me" | "protonmail.com" => ("127.0.0.1", 1143, "127.0.0.1", 1025),
        "aol.com" => ("imap.aol.com", 993, "smtp.aol.com", 465),
        "zoho.com" => ("imap.zoho.com", 993, "smtp.zoho.com", 465),
        _ => return None,
    })
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    accounts: Vec<Account>,
}

pub fn load() -> Vec<Account> {
    std::fs::read_to_string(paths::accounts_file())
        .ok()
        .and_then(|t| serde_json::from_str::<File>(&t).ok())
        .map(|f| f.accounts)
        .unwrap_or_default()
}

pub fn save(accounts: &[Account]) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(&File { accounts: accounts.to_vec() }).unwrap_or_default();
    paths::write_atomic(&paths::accounts_file(), format!("{text}\n").as_bytes())
}

pub fn new_id(email: &str) -> String {
    let slug: String = email.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let n: u32 = rand::random::<u32>() % 10000;
    format!("{slug}-{n:04}")
}
