//! Whether Envoy may use the network at all. It always may, except in demo mode (`--demo`),
//! whose mail lives on an in-process server ([`crate::demo`]) and which promises that nothing
//! leaves the computer. Every place Envoy would open a connection (IMAP, SMTP, Google's
//! sign-in, a remote picture) asks [`allowed`] first, so even a path the demo's own routing
//! missed can't reach the network; and a refusal is counted, for tests to check that none was
//! even tried.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static OFFLINE: AtomicBool = AtomicBool::new(false);
static REFUSED: AtomicUsize = AtomicUsize::new(0);

/// No connection is opened from here on, in this process.
pub fn go_offline() {
    OFFLINE.store(true, Ordering::SeqCst);
}

/// Ok unless the process is offline ([`go_offline`]): then an error naming `what`, counted.
pub fn allowed(what: &str) -> anyhow::Result<()> {
    if !OFFLINE.load(Ordering::SeqCst) {
        return Ok(());
    }
    REFUSED.fetch_add(1, Ordering::SeqCst);
    log::error!("refused to open a connection in demo mode: {what}");
    anyhow::bail!("Demo mode never connects to anything ({what})")
}

/// How many connections [`allowed`] refused.
pub fn refused() -> usize {
    REFUSED.load(Ordering::SeqCst)
}
