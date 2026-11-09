# 0001 — HTML mail renders natively; no live web view

**Status:** accepted, November 2026. **Laws:** 1 (speed), 2 (privacy), 3 (simple by default).

## Context

[`NORTH_STAR.md`](../NORTH_STAR.md) ("Rendering") plans for plain and simple HTML through the
native renderer, and complex HTML through the system web view (WKWebView, WebView2,
WebKitGTK) "from a pool of 2–3 warm views created after the first frame … Opening one is a
swap", under the "Webview lockdown" rules: no JavaScript, no network, links in the system
browser, strict CSP, no storage, display-only, never stealing keyboard shortcuts.

A feasibility study of live child web views concluded against them:

- **Wayland can't host them.** Envoy draws its whole window into one GPU surface (Rune on
  wgpu). A WebKitGTK view is a GTK widget with its own surface tree; there is no way to embed it
  as a child of a surface another toolkit owns on Wayland (XEmbed is X11-only), so on Linux the
  plan has no way to work at all.
- **They sit on top of everything.** Where embedding works (an `NSView` on macOS, a child `HWND`
  on Windows), the native view is composited above the GPU surface. Everything Rune draws over
  the reading pane — toasts, the composer sheet, menus, dialogs, the command palette — would be
  hidden behind it. Z-order can't be interleaved.
- **They take focus.** A click in a child web view moves keyboard focus into another process's
  view; Envoy's single-key shortcuts (`j`, `k`, `e`, `#`) stop reaching the app until focus is
  pulled back, against "never steals keyboard shortcuts".
- A warm pool of multi-process web views also costs memory the 150 MB idle budget doesn't have,
  and process spawns in the user's first seconds. (Not measured for this record; this was the
  study's estimate.)

## Decision

1. **(b) Now: make the native renderer good enough for most real mail**, and let it say when
   it isn't. `envoy_core::html` reads the CSS email actually uses — `<style>` sheets with a
   bounded cascade, `@media` evaluated for a desktop pane, a shared column model for tables
   (`colspan`, `rowspan` down an edge), inline-block fluid columns that wrap, line height,
   weights, families, letter spacing, text transform, per-side borders, margins, background
   pictures — and records what it dropped or approximated in a local
   [`Fidelity`](../../crates/envoy-html/src/fidelity.rs) score. A 22-mail fixture corpus
   (`crates/envoy-core/tests/fixtures/html`) keeps it honest.
2. **The reader offers the original only when it matters.** When the score says the layout
   likely can't be reproduced (`Verdict::Poor`: positioned layers, CSS grid or flex layouts,
   drawings, embedded media, forms…), a single quiet line above the message offers "Open in
   browser"; the user can dismiss it. Mail that merely differs in details (`Approximate`) gets no
   note. Nothing loads as a side effect: the browser opens only when the user asks, and it gets
   a sanitized copy (below). The score never leaves the machine; it is logged at debug level
   only.
3. **(c) Later, maybe: an offscreen snapshot helper** for mail the classifier flags, drawn
   into the reading pane as pictures (below). Not built now.

### Choices inside the renderer

- **Media queries are evaluated for a 760-point desktop pane** (the default reading width), not
  the live pane. The layout tree is built once per message off the UI thread and cached; making
  it depend on the pane width would mean building it again, parsing included, as the divider
  moves, or building one tree per breakpoint on the open path. So `max-width: 600px` mobile
  rules don't apply, and `min-width: 480px` desktop rules (MJML's column widths) do.
- **Narrow panes are fitted when the tree is drawn** (`html_view`), at the width the reader
  has, read from its layout in 8-point steps: dragging the divider rebuilds the message's
  elements when a step is crossed, never its tree. Nothing reaches past the reader. Fixed widths,
  minimums, pictures and padding are capped at the room each box gets, as `max-width: 100%`
  does in mail clients. A word longer than its line (a link) breaks. Table columns keep their
  longest word while there's room for every column's. Fluid columns stack below 480 points (two
  240-point columns, where MJML and Cerberus templates switch to one column), or when one would
  be under 120. One thing is read from the phone rules: which table cells they make full-width
  blocks (Cerberus's `.stack-column`, Foundation's `.columns`). Rows of those stack below the
  same 480 points. `envoy`'s `html_view` tests lay every fixture out at 320, 520 and 900 points
  and fail if any box or line of text reaches past the width or out of its box.
- **`prefers-color-scheme: dark` rules are not applied.** "Original" (`reading.htmlColors`)
  draws mail as designed on a light page, where the light rules are the consistent ones. "Adapt"
  recolours mail to the theme by its own rules, the same way for every message; mixing in an
  author's partial dark rules (often only backgrounds and some text) on top of that gives
  results neither the author nor the adaptation intended. Applying them properly would also make
  the cached tree depend on the theme.
- **No CSS crates.** The cascade is hand-written (`crates/envoy-html/src/css.rs`, about 1,250 lines with its tests): the
  subset email uses, with no new dependencies, for the binary budget. Selector matching is
  indexed by id, class and tag, pre-filtered by a Bloom filter of each element's ancestors'
  classes and ids, and capped per message (`MATCH_BUDGET`); past the cap the sheets stop
  applying and the classifier notes it. The renderer is its own crate (`envoy-html`), built at
  opt-level "s" like html5ever under it: it runs once per message, off the UI thread. It adds
  about 50 KB to the macOS binary.
- **Hostile input is bounded.** Mail is attacker-controlled and parsing runs on 2 MiB engine
  threads while the store is locked. `parse` catches panics at its boundary (an empty tree, so
  the reader falls back to text, rather than a poisoned lock); the builder stops laying out at
  200 levels or 1 MiB of stack and keeps the rest as text; a linear pre-pass caps what reaches
  html5ever's quadratic corners (512 attributes per tag, 256 open elements); selectors,
  declarations and candidate rules have caps. `tests/hostile.rs` in `envoy-html` keeps it so.
- **Remote images are a per-sender opt-in.** `reading.loadImages` defaults to "ask":
  remote pictures (and background pictures) stay hidden behind a quiet "Images hidden" bar
  that loads them once or always from that sender (`reading.imagesFrom`: exactly `user@domain`,
  `domain` or `@domain`; anything else is ignored and flagged in Settings). Embedded `cid:`
  pictures always show. Tracking pixels never load, `<img>` or background: a picture or box at
  most 3 px either way, by its effective size (`max-width`/`max-height` included).
- **"Open in browser" opens a sanitized copy** (`envoy_html::export`), written from an
  allowlist, not cleaned of what's known to be bad. Only plain HTML elements for reading mail
  are written; `<svg>`, `<math>`, the legacy raw-text elements and anything that runs, embeds,
  loads or takes input go with their content; other elements are unwrapped. Only presentational
  attributes, plus checked URLs: links keep `http(s)`, `mailto`, `tel` and `#fragment`s (no
  scheme-less paths, which a `file://` page resolves to local files); remote pictures stay only
  when they'd load in Envoy, tracking pixels excluded. CSS `url()`s follow the same rule, and
  `<` in sheets is escaped. A strict CSP, no referrer and no DNS prefetching come first in
  `<head>`; a UTF-8 byte-order mark and `<!DOCTYPE html>` first in the file. The result is
  parsed back and checked against the same allowlist; if it fails, the file holds the message's
  text instead.
- **"Open original" opens the message as received**, as an `.eml` file for the system's mail
  viewer, not as an `.html` file in a browser that would ignore the image setting.
- **Exported files are private.** Both go to a folder only the user can open (Envoy's cache
  folder on Linux, not the shared `/tmp`; the per-user temporary folder on macOS and Windows),
  created 0700, each file 0600, created exclusively under a random name, and removed after a
  day. When the default app for the file's type on Linux is a Snap, which can't read hidden
  folders in home, the folder is `Downloads/Envoy` instead, with the same protections and a
  `.trackerignore` so desktop search doesn't index mail.
- **Families map to generic faces.** Sans → the app's sans, mono → its mono, serif → a system
  serif by name (Georgia; DejaVu Serif on Linux). Rune only registers its bundled faces and a
  few system families (symbols, CJK, mono), so the serif request falls back to the sans face
  today; loading a system serif lazily, off the UI thread, is a Rune follow-up.

## What the snapshot route (c) would require

A helper that renders a flagged message in a hidden system web view and hands back pictures
the reading pane draws natively, so nothing sits over the window and nothing takes focus:

- **Sanitize before loading.** Strip `script`, `iframe`, `object`, `embed`, `form`, `base`,
  `meta http-equiv=refresh`, event-handler attributes and `javascript:` URLs; drop remote
  stylesheets and fonts.
- **CSP and a custom scheme for images.** Load the document from an app scheme with a policy
  like `default-src 'none'; img-src envoy-img: cid:; style-src 'unsafe-inline'`, and serve
  images through `envoy-img:` from the same disk cache and the same `reading.loadImages` policy
  as the native renderer, so remote content stays blocked by default (law 2). Block all other
  network requests at the engine (`WKContentRuleList` / `WKURLSchemeHandler`, WebView2's
  `WebResourceRequested` filter, WebKitGTK URI scheme handlers with no network proxy).
- **Host-only scripts, page scripts off.** Measuring the page (height, link and text
  rectangles) needs script; the page's own must never run. The engines allow both:
  `WKWebpagePreferences.allowsContentJavaScript = false` with evaluation in a client content
  world; WebView2 `IsScriptEnabled = false` (scripts injected with `ExecuteScriptAsync` still
  run); WebKitGTK `enable-javascript-markup = false` with `webkit_web_view_evaluate_javascript`.
- **WebView2 specifics.** A hidden WebView2 stops rendering (`IsVisible = false` suspends it),
  so capture needs an off-screen but "visible" host window or `CapturePreviewAsync`. The Edge
  runtime sends diagnostic data under the system's settings and SmartScreen reputation checks
  send URLs to Microsoft unless disabled (`IsReputationCheckingRequired = false`); both need
  turning off or documenting against law 2, and the runtime's own update traffic is outside our
  control.
- **Tall mail.** A 10,000-point newsletter at 2× is 20,000 device pixels tall — past maximum
  texture sizes (8,192–16,384) and about 120 MB as one RGBA bitmap at 1,520 pixels wide. It has
  to be captured in tiles (say 2,048 pixels), lazily as the user scrolls, with a cap on cached
  tiles, and re-captured when the width, theme or zoom changes.
- **Selection and accessibility.** A picture has no text. The helper would extract text runs
  with their rectangles (host script over the DOM) to drive selection, copy, find, link hit
  testing and AccessKit nodes over the tiles.
- **Off the open path.** A snapshot takes hundreds of milliseconds and a process: it would run
  on arrival or prefetch and be cached, with the native rendering (and its note) shown until
  it's ready — never on the 50 ms open path.

## Consequences

- The North Star's "Rendering" bullet ("pool of 2–3 warm views … opening one is a swap") no
  longer describes the plan. Revising it is for the user; this record doesn't edit it.
- Mail that needs a browser gets one only on request, in the user's own browser, sanitized:
  nothing in it runs, and it loads only the remote pictures Envoy itself would load.
- What the renderer still doesn't do (and how the score treats it): absolute/fixed positioning,
  flex and grid layout (their items stand one under another), floats that aren't columns, inline SVG, forms, embedded media, transforms
  and filters, multi-column text, `::before`/`::after` content, `rowspan` away from the table's
  edges (each noted, weighted and capped per kind); gradients (first colour), `display: block`
  on some but not all of a row's cells, `dir="rtl"` beyond column order, `@supports`, web fonts
  (not noted: minor or invisible).
