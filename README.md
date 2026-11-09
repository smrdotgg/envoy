# Envoy

A fast, native, configurable email client in Rust, built on [Rune](https://github.com/anomalyco/rune)
(GPU-rendered native UI). Gmail-like layout, offline-first, everything configurable from a
VS Code–style settings screen backed by a plain `settings.json`.

**Email at the speed of thought.** See [`docs/NORTH_STAR.md`](docs/NORTH_STAR.md) for the laws
every decision is judged against. MIT licensed.

## Try it

```sh
# Sample mail, no account, nothing sent anywhere (the real app, offline):
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
| `!` | report spam (from Spam: not spam) | `/` | search |
| `v` | move to… | `l` | label… (Gmail) |
| `b` | snooze… (in Snoozed: a new time) | `mod+shift+a` | attach files (in the composer) |
| `z` | undo archive / delete / move / label / snooze / send | `esc` | close, or clear the selection |
| `s` | star | `mod+,` | settings |
| `u` | toggle read | `mod+shift+b` | toggle sidebar |
| `x` | select / unselect | `shift+j` / `shift+k` | select the next / previous too |
| `mod+a` | select all | `mod+1/2/3` | inbox / starred / sent |
| `1` `2` `3` `4` | inbox tab: Important / Other / Newsletters / Notifications | `alt+1` … `alt+4` | move to that tab, with its sender |

With messages selected (also shift-click for a range, mod-click to add one), every key above
acts on all of them, `j` and `k` move without opening, and one `z` undoes the whole batch.
Move to… and Label… search as you type; with messages from several accounts selected they
offer only what every one of those accounts has.

Search understands `from:`, `to:`, `subject:`, `is:unread`, `is:read`, `is:starred`,
`has:attachment`, `category:` (`category:newsletters`, or just `category:n`), plus free text
over subject, people and body (SQLite FTS5, instant, offline). Search looks in every inbox tab
unless you add `category:` (no tab is chosen while a search shows; an opened message says which
it's in).

## Split inbox

The inbox comes in four tabs, each with its unread count, so nothing is out of sight without a
number on it:

- **Important**: mail from people you've written to, mail sent to you directly, Gmail's
  Important label, and every conversation you're part of.
- **Other**: mail from people where you're only copied, or that went to a group.
- **Newsletters**: anything with an unsubscribe link (`List-Unsubscribe`), mailing lists
  (`List-Id`, `Precedence: bulk`), newsletter platforms. A list post from someone you've written
  to stays in Other.
- **Notifications**: automated mail: `Auto-Submitted`, GitHub/GitLab/Jira's own headers,
  notifications@, alerts@, receipts@ and no-reply senders, receipts, bookings and alerts.

A conversation goes where its strongest message does (a person beats a machine, and you beat
anyone: reply to a newsletter and the conversation is Important). It's all worked out on your
computer when mail arrives, from a few headers, and kept with the message, so a tab opens as
fast as a folder; nothing about it ever reaches your mail server.

Wrong tab? `alt+1` … `alt+4` (or Move to… `v`) moves the conversation, and future mail from
its sender, with Undo. That writes a rule, and rules beat Envoy's own judgment. Rules live in
`settings.json` (Settings → Inbox shows them one per line, as `from:*@github.com
notifications`); the first that matches wins:

```jsonc
{
  "inbox.rules": [
    { "from": "*@github.com", "category": "notifications" },      // a pattern
    { "from": "news@shop.example", "category": "newsletters" },   // one sender
    { "from": "northwind.dev", "category": "important" },         // a domain and its subdomains
    { "list": "rust-lang.org", "category": "newsletters" }        // a mailing list's List-Id
  ]
}
```

Changing rules re-sorts the cache in the background (a rule for one sender, at once). Turn the
split off with `"inbox.split": false` (Settings → Inbox → Split inbox).

## Recipients

Typing in To, Cc or Bcc suggests people from your own mail: ↑ and ↓ to choose, Return or Tab to
take one (it goes in as `Name <address>`, with a comma for the next), Esc to hide them; a click
works too. They come from the mail Envoy keeps on your computer, never from a server: the people
you write to first (writing to someone counts five times what hearing from them does), and the
ones you've been in touch with lately (a month ago counts half). Newsletters, lists and other
bulk senders you've never written to come last, no-reply addresses not at all, and your own
addresses never.

## Snooze

Press `b` (or the clock in the reader's toolbar, or over a selection) to put a message away until
later: **Later today**, **Tonight**, **Tomorrow**, **This weekend**, **Next week**, or type a time
into the picker's field, as you'd say it: `fri 9am`, `tomorrow 5pm`, `in 2 days`, `in 3 hours`,
`dec 3`, `2026-12-03 09:00` (read on this computer, in your time zone). A conversation snoozes
whole. It leaves the inbox at once, with **Undo** for the undo window, like an archive; undone,
the server never hears of it.

At its time it's back **at the top of your inbox** (of its own tab, in the split inbox), unread and
marked "Snoozed", until you deal with it (read it and move on, or archive it); a search lists it
by date like any other. **Snoozed** in the sidebar lists everything snoozed, soonest first, with
when each comes back; `b` there picks a new time, and the inbox icon on a row brings it back now.

**Snoozes live on your mail server, not on any Envoy server.** A snoozed message is in a
folder named for when it wakes, under `Snoozed`: `Snoozed/20261110T0800Z` (UTC, to the minute);
on Gmail, a label. Other mail apps show them as folders or labels, and every Envoy you run sees
the same snoozes. Any Envoy that's running at the time brings a message back (and one that
wasn't catches up when it starts); nothing happens while none is. On Gmail an empty label goes a
week after its time; other servers keep the empty folders (deleting a folder there deletes what's
in it, and another Envoy could be moving a message in), which Envoy then leaves alone. If your
server already keeps a `Snoozed` folder of its own (or won't let Envoy make one), Envoy's go
under `Envoy Snoozed` instead, and the server's own snoozes are shown in Snoozed but left to it.
Snoozes made in Gmail's web app aren't visible over IMAP, so Envoy doesn't show them.

## Attachments

Attach files with the paperclip next to Send (or `mod+shift+a`), which opens your system's file
picker (pick several at once), or drop files from anywhere onto the composer. Each file shows as
a chip with its name and size, and an × to take it off; while a file is being read it says
"Attaching…" (reading never holds up typing). Forwarding a message takes its attachments along,
pictures shown inside it too (a forward is plain text), each removable like any other.

A message can be as big as your mail server says it takes (Envoy asks it once, and again each
time it sends; Gmail's limit comes to about 25 MB of attachments, as files grow by a third when
encoded for sending). Until Envoy has asked, 25 MB of attachments is the limit, which most
providers share. Over it, the composer says so and won't send until some come off, rather than
have the server bounce the message later; a single file too big for any message isn't attached
at all. If the server takes less by the time the message goes (or refuses it as too big), it's
not sent, nor tried again: it comes back to the composer and Drafts, saying why. Files are kept with
the draft on your computer from the moment you pick them: one still being read when you start
another message, quit or crash is still there when you open the draft again (read again, or,
if the file is gone, marked as not attached, never silently dropped). They go to the Drafts
folder with the draft and are sent with it; Envoy never sends a message without a file it
shows, or with one it doesn't (if the draft's files changed elsewhere, Send says what changed,
shows the files as they are, and waits for you to send again). A draft carrying big files is copied to the Drafts folder less often while you type
(every couple of minutes at 25 MB), since each copy sends them all again.

## How it's built

- `crates/envoy-core` — no UI. Settings schema, accounts, secrets, OAuth (loopback + PKCE),
  the SQLite store (WAL, FTS5), IMAP sync (async-imap over rustls), SMTP (lettre), and the
  engine: a background thread with one sync worker per account.
- `crates/envoy-html` — the native HTML mail renderer: email HTML and its CSS (`<style>`
  sheets through a bounded cascade, `@media` for a desktop pane) to a layout tree of text,
  boxes, table rows on a shared column grid and fluid columns, with a local score of what it
  couldn't draw ([decision record](docs/decisions/0001-html-rendering.md)). Built for size;
  envoy-core re-exports it as `envoy_core::html`.
- `crates/envoy` — the Rune app: sidebar, virtualized message list, conversation reader
  (HTML mail drawn natively from that tree — no web view, tracking pixels dropped, pictures
  per `reading.loadImages`), floating composer, settings and account screens.
- `vendor/rune` — the Rune crates, vendored (Rune's repo is large). Refresh with
  `scripts/vendor-rune.py <path-to-rune-checkout>`.

**Reliability model.** The UI only ever reads the local cache, so it never waits on the
network. Every action (read, star, archive, delete, send) is applied locally first and
queued in the database; the worker replays the queue on the server and retries with backoff
until it succeeds, across restarts and offline periods. Sending waits `compose.undoSendSeconds`
in that queue so it can be undone; archive, delete, spam, other moves and labels (on Gmail a
label is a copy in the label's mailbox) wait `behavior.undoSeconds` (5 s) the same way, with
the messages hidden rather than deleted locally, so **Undo** in the toast (or `z`) puts them
back exactly as they were and the server never sees the move. Once the toast is gone the move goes through; if the app quits first, it
goes through on the next start. Deleting from Trash is permanent and has no undo. The worker
uses IMAP IDLE for push, reconnects with
backoff, and times out dead connections in 30 s.

**Drafts are never lost.** What you write is saved to the local database as you type (a moment
after you pause, every few seconds while you don't, and when the window closes), and a composer
that was open when Envoy quit or crashed comes back, minimized, on the next start. (Should the
database refuse a draft as Envoy closes, it's kept in the `drafts-recovery` folder in the data
folder and brought back on the next start.) A few seconds
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
cargo run -p envoy-core --example e2e           # engine against a local GreenMail server (below)
ENVOY_DEMO=1 target/debug/envoy --snap out.png --scene open     # headless screenshot
ENVOY_DEMO=1 ENVOY_DEMO_HTML=mail.html target/debug/envoy --snap out.png --scene html --size 1300x3000   # a whole HTML email
cargo run -p envoy-core --example html_dump -- mail.html            # its layout tree (TREE=1) and fidelity score
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/flow.json --snap out.png --at 4.8
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/suggest.json --snap out.png --at 0.7 # "l" in To: suggestions, bulk senders last
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/suggest.json --snap out.png --at 1.6 # one taken with Return, "s", and ↓
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/suggest.json --snap out.png --at 3.2 # Esc, "a", Tab takes Sam; Tab again to Subject
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/undo.json --snap out.png --at 1.0   # archive: toast with Undo
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/undo.json --snap out.png --at 2.0   # after z: message back, open
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/drafts.json --snap out.png --at 2.8 # draft opened, edited, discarded: toast with Undo
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/drafts.json --snap out.png --at 3.6 # after z: the composer back with the edit
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/attach.json --snap out.png --at 1.0 # files picked: "Attaching…"
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/attach.json --snap out.png --at 2.0 # attached, and a folder that can't be
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/attach.json --snap out.png --at 2.5 # a file dragged over the composer
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/attach.json --snap out.png --at 4.2 # dropped, and the folder taken off
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/select.json --snap out.png --at 1.2 # x, shift+j twice: three selected
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/select.json --snap out.png --at 1.8 # e: all three archived, one toast
ENVOY_DEMO=1 target/debug/envoy --script scripts/ui/select.json --snap out.png --at 2.6 # z: all three back
```

Run from the repository root (the attach script picks files in it), with a debug build: release
builds leave headless runs out.

**Demo mode** (`--demo`, `ENVOY_DEMO=1`) is the whole app, engine and store included, on sample
mail kept by an IMAP server inside the process (`crates/envoy-core/src/demo.rs`): the cache is
filled from it as a first sync would, and moves, labels, flags, drafts, sends and snoozes go to it
through the op queue and come back by sync, as with a real account. Sends are filed in Sent (and
mail to the demo's own address arrives in its inbox); nothing ever opens a connection (every way
out is refused in demo mode, and a test checks no socket is opened), and its files (cache,
settings) go in a folder of its own, deleted when it closes. The scripts tap list rows by message
id (`m9` is the first conversation), as the demo's cache numbers them. Scenes: `open light dark compact bottom compose forward compose-suggest light-compose settings settings-keys accounts add welcome sent drafts draft draft-notice html html-dark select select-off picker picker-select label snooze snooze-typed snoozed woken toast toast-compose counts split split-newsletters split-moved split-search split-narrow split-empty split-off settings-rules`.

HTML rendering is tested against the mail in `crates/envoy-core/tests/fixtures/html` (sources and
licenses in its `LICENSES.md`).

**End to end.** CI's `e2e` job runs the engine against a real IMAP/SMTP server,
[GreenMail](https://greenmail-mail-test.github.io/greenmail/): initial sync, new mail by IDLE,
flags both ways, archive, spam, trash and moves with the undo window (off the server during it,
untouched by an undo, there after it), labels (a copy in another mailbox, the message staying
put) and a two-message move under one undo key, delete forever, sending with undo send, drafts (saved,
edited, sent, undone, discarded), moves and a send held across a kill and a restart, and snoozes
(undone inside the window with the server untouched; let go into its `Snoozed/…` folder, made and
subscribed; one whose time came first woken in place; a folder left past its time woken on the
next check, unread and pinned; old empty ones kept (only Gmail's are deleted); unsnoozed from Snoozed). It runs
twice: against GreenMail as it is (UIDPLUS, MOVE, IDLE), then through a proxy that takes UIDPLUS
and MOVE away, so the fallbacks run too, and fails if Envoy sends a command that server lacks,
or one that would expunge mail it didn't delete. To run it locally (the second pass needs
`openssl`):

```sh
docker run -d --rm --name envoy-greenmail \
  -e GREENMAIL_OPTS='-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:pw@localhost,erin:pw@localhost -Dgreenmail.users.login=email' \
  -p 3025:3025 -p 3143:3143 -p 3465:3465 -p 3993:3993 greenmail/standalone:2.1.3
cargo run -p envoy-core --example e2e           # both passes, ~3 minutes; `-- full` or `-- plain` runs one
docker stop envoy-greenmail
```

CI runs it on a debug build and again on a dist build linked like the shipped Linux binary, with
identical functions folded: `cargo rustc --profile dist -p envoy-core --example e2e -- -Clink-arg=-Wl,--icf=safe`,
then `target/dist/examples/e2e`.

Each pass empties its GreenMail user's mailboxes first, so it can run again against the same server.

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
