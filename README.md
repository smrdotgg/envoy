# Envoy

A fast, native, configurable email client in Rust, built on [Rune](https://github.com/anomalyco/rune)
(GPU-rendered native UI). Gmail-like layout, offline-first, everything configurable from a
VS Code–style settings screen backed by a plain `settings.json`.

**Email at the speed of thought.** See [`docs/NORTH_STAR.md`](docs/NORTH_STAR.md) for the laws
every decision is judged against. MIT licensed.

## Try it

```sh
# Sample mail, no account, nothing sent anywhere:
cargo run --release -p envoy -- --demo

# The real thing:
cargo run --release -p envoy

# A double-clickable macOS app in dist/Envoy.app:
scripts/bundle-macos.sh && open dist/Envoy.app
```

First build takes a few minutes (Rune's renderer); after that it's incremental.

### Linux

Build-time packages (Ubuntu/Debian): `sudo apt-get install -y pkg-config libxkbcommon-dev libwayland-dev`.
Running needs a desktop session (X11 or Wayland). On a machine with no GPU, add
`mesa-vulkan-drivers` and run with `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`.

### Windows

`cargo build --release -p envoy` → `target\release\envoy.exe`. No extra setup.

## Signing in to Gmail

Two ways, chosen on the "Add account" screen:

1. **Gmail app password** (quickest). Needs 2-Step Verification on your Google account.
   Create a password named "Envoy" at <https://myaccount.google.com/apppasswords> and paste
   the 16 letters. That's it.
2. **Google sign-in (OAuth)**. Google requires every app to have its own OAuth client, so
   this needs a one-time setup in Google Cloud Console (create project → enable Gmail API →
   OAuth consent screen "External" with yourself as a test user → Clients → *Desktop app*).
   Paste the client ID and secret into the form, or set `ENVOY_GOOGLE_CLIENT_ID` /
   `ENVOY_GOOGLE_CLIENT_SECRET` (at build time to bake them in, or at run time).

Any other IMAP/SMTP provider works through "Other email (IMAP)"; common providers
(Outlook, iCloud, Yahoo, Fastmail…) fill in their servers automatically.

Passwords and tokens live in the macOS Keychain / Windows Credential Manager. On Linux
(or with `ENVOY_SECRETS=file`) they go to `secrets.json` in the data folder with 0600
permissions.

## Settings

Everything is in **Settings** (⌘, / Ctrl+,): search, filter to "Modified only", reset any
value. Each change is written to `settings.json`, which holds *only your overrides* as flat
VS Code–style keys:

```jsonc
{
  "appearance.theme": "dark",
  "appearance.accent": "violet",
  "inbox.readingPane": "bottom",
  "keys.archive": "e, y",
  "sync.intervalSeconds": 60
}
```

Edit it by hand and Envoy reloads it live (comments and trailing commas are fine).
"Edit settings.json" also writes `settings.defaults.jsonc` next to it: every setting with
its description and default, to copy from. Back the file up, sync it, copy it to another
machine — it contains no secrets.

| | macOS | Windows | Linux |
|---|---|---|---|
| Settings & accounts | `~/Library/Application Support/dev.envoy.Envoy/` | `%APPDATA%\envoy\Envoy\config\` | `~/.config/envoy/` |
| Mail cache | same folder, `mail.sqlite` | `%APPDATA%\envoy\Envoy\data\` | `~/.local/share/envoy/` |

`ENVOY_HOME=/some/folder` puts everything in one folder (portable installs).

## Keyboard

All rebindable under Settings → Keyboard (`mod` = ⌘ on macOS, Ctrl elsewhere).

| Key | Action | Key | Action |
|---|---|---|---|
| `j` / `k` | next / previous | `c` | compose |
| `e` | archive | `r` / `a` / `f` | reply / reply all / forward |
| `#` | delete | `mod+enter` | send |
| `z` | undo archive / delete / move / send | `/` | search |
| `s` | star | `esc` | close |
| `u` | toggle read | `mod+,` | settings |
| `mod+1/2/3` | inbox / starred / sent | `mod+shift+b` | toggle sidebar |

Search understands `from:`, `to:`, `subject:`, `is:unread`, `is:read`, `is:starred`,
`has:attachment`, plus free text over subject, people and body (SQLite FTS5, instant,
offline).

## How it's built

- `crates/envoy-core` — no UI. Settings schema, accounts, secrets, OAuth (loopback + PKCE),
  the SQLite store (WAL, FTS5), IMAP sync (async-imap over rustls), SMTP (lettre), and the
  engine: a background thread with one sync worker per account.
- `crates/envoy` — the Rune app: sidebar, virtualized message list, conversation reader
  (HTML mail rendered natively as Markdown — no web view, no remote images, no tracking
  pixels), floating composer, settings and account screens.
- `vendor/rune` — the Rune crates, vendored (Rune's repo is large). Refresh with
  `scripts/vendor-rune.py <path-to-rune-checkout>`.

**Reliability model.** The UI only ever reads the local cache, so it never waits on the
network. Every action (read, star, archive, delete, send) is applied locally first and
queued in the database; the worker replays the queue on the server and retries with backoff
until it succeeds, across restarts and offline periods. Sending waits `compose.undoSendSeconds`
in that queue so it can be undone; archive, delete, spam and other moves wait
`behavior.undoSeconds` (5 s) the same way, with the messages hidden rather than deleted
locally, so **Undo** in the toast (or `z`) puts them back exactly as they were and the server
never sees the move. Once the toast is gone the move goes through; if the app quits first, it
goes through on the next start. Deleting from Trash is permanent and has no undo. The worker
uses IMAP IDLE for push, reconnects with
backoff, and times out dead connections in 30 s.

**Drafts are never lost.** What you write is saved to the local database as you type (a moment
after you pause, every few seconds while you don't, and when the window closes), and a composer
that was open when Envoy quit or crashed comes back, minimized, on the next start. (Should the
database refuse a draft as Envoy closes, it's kept in `drafts-recovery.json` in the data folder
and brought back on the next start.) A few seconds
later the draft is copied to the account's Drafts folder through the same queue (offline, it
waits), each save replacing the previous copy, so other devices see one draft. Opening a message
in Drafts opens it in the composer, attachments and all; they go with it when it's saved and
sent. Envoy writes plain text, so editing a draft with formatting keeps its text and attachments
but not the formatting (the composer says so), and a draft it can't edit in place without losing
something (signed, encrypted, formatted only) opens as a copy, leaving the original as it is.
Sending removes the draft, and Undo send brings the composer back as it was; discarding a draft
has an Undo too. Opening another composer keeps the current one in Drafts.

## Testing

```sh
cargo test                                      # unit tests
cargo run -p envoy-core --example e2e           # engine against a local GreenMail server (see the file header)
ENVOY_DEMO=1 target/debug/envoy --snap out.png --scene open     # headless screenshot
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/flow.json --snap out.png --at 3.6
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/undo.json --snap out.png --at 1.0   # archive: toast with Undo
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/undo.json --snap out.png --at 2.0   # after z: message back, open
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/drafts.json --snap out.png --at 2.8 # draft opened, edited, discarded: toast with Undo
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/drafts.json --snap out.png --at 3.6 # after z: the composer back with the edit
```

Scenes: `open light dark compact bottom compose light-compose settings settings-keys accounts add welcome sent drafts draft draft-notice`.

**Latency budgets** ([`docs/NORTH_STAR.md`](docs/NORTH_STAR.md)) are checked in CI on all three OSes
against a generated 100k-message mailbox; each check prints measured vs budget and fails over budget.
Checks that miss today are listed per OS in `crates/envoy-bench/known-misses.toml`, each with a
ceiling it mustn't reach; one that starts passing raises a warning to remove its entry. To run
them locally:

```sh
cargo build --profile dist -p envoy -p envoy-bench
target/dist/envoy-bench all --home /tmp/envoy-mailbox --bin target/dist/envoy   # generates the mailbox once (~900 MB)
target/dist/envoy-bench search --home /tmp/envoy-mailbox                         # or just: open, store, app, size
```

`app` opens a real window five times; on a headless Linux box run it under `xvfb-run -a` with
`libxkbcommon-x11-0` installed and the `VK_ICD_FILENAMES` above. Each run is paired with runs of
two launch floors built beside `envoy-bench` (`floor-window`, a bare Hello World window, and
`floor-rune`, the smallest Rune app): cold start is gated on Envoy's time above the bare window's.
`crates/envoy-bench/src/main.rs` says what each number measures.

`RUST_LOG=envoy_core=debug` shows what the sync workers are doing.
