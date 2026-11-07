//! Checks the Gmail IMAP sign-in path with deliberately wrong credentials.
use envoy_core::accounts::{Account, AuthKind};
use envoy_core::imap::{self, Credential};
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let a = Account { id: "probe".into(), email: "envoy.probe.nonexistent@gmail.com".into(), display_name: String::new(), auth: AuthKind::Password, username: String::new(), imap_host: "imap.gmail.com".into(), imap_port: 993, smtp_host: "smtp.gmail.com".into(), smtp_port: 465, hue: 0.0, signature: String::new(), oauth_client_id: String::new(), oauth_client_secret: String::new() };
    let t = std::time::Instant::now();
    match imap::connect(&a, &Credential::Password("wrong password".into())).await {
        Ok(_) => println!("unexpectedly logged in"),
        Err(e) => println!("{:?}: auth error = {}: {e:#}", t.elapsed(), imap::is_auth_error(&e)),
    }
}
