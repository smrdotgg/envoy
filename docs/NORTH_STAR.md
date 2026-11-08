# Envoy: North Star

> ## Email at the speed of thought.
> Open it, handle what matters, close it — in seconds.

Every decision in Envoy is judged against this. When two good ideas conflict, the laws below
decide, in order.

## Compass

**Less time in email per day.** This is a compass, not a metric: it is judged by feel, by using
Envoy every day. There is **no telemetry, no user studies, no tracking**. The only things we
measure are the app's own latency budgets, in CI.

## Laws (in priority order)

1. **Speed is the product.** Everything local is fully loaded and interactive in under 100 ms
   from a cold start — and 100 ms is generous. Nothing ever blocks input or drops a frame. Work
   that can't meet the budget streams in progressively, off the hot path, or it doesn't ship.
   Budgets are enforced in CI; a regression fails the build.
2. **Your mail stays yours.** Mail data travels only between your computer and your mail
   provider. No Envoy server is ever in the mail path. Remote content is blocked by default.
3. **Simple by default, deep on demand.** Out of the box Envoy is a fast, obvious mail app that
   anyone can use. Power users get keyboard-everything, density, and `settings.json`. A normal
   user never meets a power feature unless they go looking.
4. **Free, and costs ~nothing to run.** Open source (MIT), fully client-side, built for the love
   of software. A server is only acceptable if it is trivial (≈$5/month at a million users) and
   never touches mail data, headers, or tokens.

## Who it's for

Power users, high-volume emailers, and normal people who just want a fast mail app. When they
conflict: defaults serve normal people, depth serves power users (law 3).

Pains we solve, in order: **slowness**, then **overload**, then **too many accounts**.

## Latency budgets

| Interaction | Budget |
|---|---|
| Cold start → local data fully interactive | < 100 ms |
| Keypress → frame | < 16 ms |
| Open a cached message | < 50 ms |
| Local search results | < 50 ms |
| Archive / delete / move | Instant (optimistic, undoable) |
| Idle memory | < 150 MB |
| Binary size | < 30 MB |

Network-bound work (fetching new mail, sending) can take longer, but never blocks the UI, and
new mail arrives without layout jank.

## Key decisions

- **Platforms:** macOS, Windows, and Linux are equal first-class targets. Budgets hold on all
  three and are checked on all three.
- **Freshness:** show the local cache instantly, sync right after. An opt-in, near-zero-cost
  background agent (menubar/tray) may come later to keep the cache warm and power notifications.
  Never a Slack-style always-running app.
- **Rendering:** plain-text and simple HTML go through the native renderer (instant). Complex
  HTML goes through the system webview (WKWebView / WebView2 / WebKitGTK — never bundled
  Chromium), from a pool of 2–3 warm views created *after* the first frame, preloading the mails
  the user is most likely to open next. Opening one is a swap.
- **Webview lockdown (non-negotiable):** no JavaScript, no network (remote images per-sender
  opt-in), links open in the system browser, strict CSP, no cookies or storage. The webview is
  display-only and never steals keyboard shortcuts.
- **Triage:** inbox-zero flow (archive / snooze / done, keyboard-first) plus local auto-split
  (Important / Other / Newsletters / Notifications) from rules and heuristics. Snoozes live on the
  mail server (IMAP), not on any Envoy server.
- **Accounts:** unified inbox by default; per-account views one keystroke away.
- **AI:** opt-in, deliberately late, bring-your-own-key or local models, never on the hot path.
  Core triage must be great with zero AI.
- **Gmail sign-in:** app passwords (plus the 100-user OAuth test cap) for now. Once the product is
  worth showing, apply for Google verification as a *local client application*, which can exempt
  Envoy from the paid CASA assessment. Fallback: ~$600/year CASA, a fair donation goal. Law 2 is
  what makes the exemption case, so it must stay true.

## Not doing

Mobile · web version · calendar · tasks · chat · team / shared inboxes · CRM · bulk sending ·
theme marketplace. Contacts only as far as autocomplete needs. A scripting hook *maybe*, much
later.
