use envoy_core::accounts::{Account, AuthKind};
use envoy_core::imap::Credential;
#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    if std::env::var("HOST").is_err() { unsafe { std::env::set_var("ENVOY_INSECURE_TLS", "1"); } }
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(3465);
    let a = Account { id: "x".into(), email: "alice@localhost".into(), display_name: String::new(), auth: AuthKind::Password, username: "alice@localhost".into(), imap_host: "localhost".into(), imap_port: 3993, smtp_host: std::env::var("HOST").unwrap_or("localhost".into()), smtp_port: port, hue: 0.0, signature: String::new(), oauth_client_id: String::new(), oauth_client_secret: String::new() };
    let raw = b"From: alice@localhost\r\nTo: alice@localhost\r\nSubject: probe\r\n\r\nhi\r\n";
    let mut size = None;
    let r = tokio::time::timeout(std::time::Duration::from_secs(15), envoy_core::smtp::send(&a, &Credential::Password("pw".into()), "alice@localhost", &["alice@localhost".into()], raw, &mut size)).await;
    println!("{r:?}, the server takes {size:?}");
}
