//! A message's HTML made safe to open in a browser.
//!
//! "Open in browser" hands the system browser a file. The browser runs whatever is in it and
//! loads whatever it points at, regardless of Envoy's settings, so the file is written anew
//! from an allowlist, not cleaned of what's known to be bad:
//!
//! - **Elements.** Only HTML elements for reading mail are written ([`KEEP`]): document
//!   structure, text and its formatting, headings, paragraphs, lists, quotes, code, tables,
//!   pictures, links, `<center>`, `<font>` and `<style>`. Elements that run, embed, load or
//!   take input, the legacy raw-text elements (`<xmp>`, `<noembed>`, `<noframes>`,
//!   `<plaintext>`) and everything outside the HTML namespace (`<svg>`, `<math>`) are removed
//!   with their content ([`DROP`]). Any other element (`<form>`, `<o:p>`, `<details>`, custom
//!   tags) is unwrapped: its content stays, checked in turn.
//! - **Attributes.** The presentational ones email uses ([`ATTRS`], [`element_attr`]), plus
//!   `href` on `<a>`, `src` on `<img>` and `background` on `<table>`, `<td>`, `<th>` and
//!   `<body>`, each checked as a URL. Every value is escaped in full (`& " ' < >`).
//! - **URLs** are read as browsers read them: surrounding controls and spaces trimmed, tabs and
//!   newlines removed. Links keep `http(s)`, `mailto`, `tel` and `#fragment`s, and nothing
//!   without a scheme besides, since in a `file://` page a relative, protocol-relative or
//!   backslash path points at local files or network shares. Pictures keep the message's own
//!   (`cid:`, inlined as `data:`), `data:` pictures, and remote ones only when remote images are
//!   allowed for the message *and* Envoy would fetch them itself ([`crate::parse`]'s `images`):
//!   tracking pixels, hidden pictures and the backgrounds of tiny or empty boxes stay out, as
//!   in the reader.
//! - **CSS** (`<style>` and `style=""`): its `url()`s follow the same rule, functions that load
//!   from bare strings (`image-set()`) are broken up, and `<` is written as the CSS escape `\3C `
//!   so nothing in a sheet can end its `<style>` early ([`css`]).
//! - **Encoding.** A UTF-8 byte-order mark first; no comments; no C0 controls but tab, LF, FF
//!   and CR.
//! - **Policy.** First in `<head>`: `<meta charset>`, a strict Content-Security-Policy
//!   (nothing loads or runs but inline styles and `data:` pictures, plus `http(s)` pictures
//!   when allowed), `no-referrer` and no DNS prefetching.
//!
//! The file is then read back as a browser would read it (html5ever again) and checked against
//! the same allowlist ([`check`]). Some trees the first parse builds don't serialize to markup
//! that parses back to the same tree; writing plain HTML elements only avoids the known cases,
//! and the check catches the rest: if anything in the reparsed tree isn't allowed, the file
//! holds the message's text instead, in a `<pre>`. Every walk uses a stack, so deep documents
//! can't overflow it.

use std::borrow::Cow;
use std::collections::HashSet;

use html5ever::ns;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Elements written, with their content.
const KEEP: &[&str] = &[
    "html", "head", "body", "title", "style", //
    "div", "span", "p", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre", "code", "center", "font", "address", //
    "section", "article", "header", "footer", "main", "nav", "aside", "figure", "figcaption", //
    "b", "strong", "i", "em", "u", "s", "strike", "del", "ins", "small", "big", "sub", "sup", "mark", "kbd", "samp", "var", "tt", "q", "cite", "abbr", "dfn", "bdi", "bdo", "wbr", "nobr", //
    "ul", "ol", "li", "dl", "dt", "dd", //
    "table", "caption", "colgroup", "col", "thead", "tbody", "tfoot", "tr", "td", "th", //
    "a", "img",
];

/// Written without content or end tag.
const VOID: &[&str] = &["br", "hr", "img", "col", "wbr"];

/// Elements removed with everything in them: they run, embed, load or take input, they're
/// legacy raw-text elements, or they open foreign content (any element outside the HTML
/// namespace goes the same way, whatever its name).
const DROP: &[&str] = &[
    "script", "noscript", "template", "iframe", "frame", "frameset", "object", "embed", "applet", "param", "portal", "fencedframe", //
    "audio", "video", "source", "track", "canvas", "map", "area", //
    "base", "link", "meta", //
    "input", "button", "select", "option", "optgroup", "datalist", "textarea", "keygen", "output", "progress", "meter", "dialog", //
    "xmp", "noembed", "noframes", "plaintext", //
    "svg", "math",
];

/// Attributes allowed on every written element: the presentational ones email uses.
const ATTRS: &[&str] =
    &["align", "valign", "width", "height", "bgcolor", "color", "border", "cellpadding", "cellspacing", "colspan", "rowspan", "dir", "lang", "title", "alt", "class", "id", "style", "hidden"];

/// Attributes allowed on some elements only, URLs aside: legacy presentation and in-page
/// anchors.
fn element_attr(tag: &str, name: &str) -> bool {
    matches!(
        (tag, name),
        ("a", "name")
            | ("img", "hspace" | "vspace")
            | ("font", "face" | "size")
            | ("td" | "th", "nowrap")
            | ("ol", "start" | "type")
            | ("ul", "type")
            | ("li", "value" | "type")
            | ("col" | "colgroup", "span")
            | ("body", "text" | "link" | "vlink" | "alink")
            | ("style", "media" | "type")
    )
}

/// Links get this `rel`, always.
const REL: &str = "noopener noreferrer";

/// Functions that load pictures from bare strings: broken up wherever they are.
const LOADERS: &[&str] = &["image-set", "-webkit-image-set", "image", "src"];

/// `cid:` pictures inlined from CSS per message; later ones become `none`.
const CSS_INLINE: usize = 64;

#[derive(Clone, Copy, PartialEq)]
enum Url {
    /// Followed when clicked: `<a href>`.
    Link,
    /// Loaded at once: `<img src>`, `background`.
    Picture,
}

fn url_attr(tag: &str, name: &str) -> Option<Url> {
    match (tag, name) {
        ("a", "href") => Some(Url::Link),
        ("img", "src") | ("table" | "td" | "th" | "body", "background") => Some(Url::Picture),
        _ => None,
    }
}

/// Which URLs the copy may hold, written and checked alike.
struct Rules {
    remote: bool,
    /// Remote pictures Envoy itself would fetch for this message (as [`browser_url`] reads
    /// them).
    fetched: HashSet<String>,
}

impl Rules {
    fn link(&self, raw: &str) -> Option<String> {
        let v = browser_url(raw);
        match scheme(&v).as_deref() {
            Some("http" | "https" | "mailto" | "tel") => Some(v),
            None if v.starts_with('#') => Some(v),
            _ => None,
        }
    }

    /// A picture URL that may stand as it is (`cid:` is resolved before this).
    fn picture_ok(&self, v: &str) -> bool {
        match scheme(v).as_deref() {
            Some("http" | "https") => self.remote && self.fetched.contains(v),
            Some("data") => data_picture(v),
            _ => false,
        }
    }
}

/// What the writer needs besides the rules: the caller's `cid:` resolver and its CSS budget.
struct Writer<'a> {
    rules: Rules,
    inline: &'a mut dyn FnMut(&str) -> Option<String>,
    css_inlined: usize,
}

impl Writer<'_> {
    fn picture(&mut self, raw: &str) -> Option<String> {
        let v = browser_url(raw);
        if scheme(&v).as_deref() == Some("cid") {
            return (self.inline)(&v).filter(|d| data_picture(d));
        }
        self.rules.picture_ok(&v).then_some(v)
    }

    /// A `url()` argument as it'll be written in `url('…')`, or None for `none`.
    fn css_picture(&mut self, raw: &str) -> Option<String> {
        let v = browser_url(raw);
        if v.is_empty() || v.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '\'' | '"' | '\\' | '(' | ')' | '<')) {
            return None;
        }
        if scheme(&v).as_deref() == Some("cid") {
            if self.css_inlined >= CSS_INLINE {
                return None;
            }
            self.css_inlined += 1;
            return (self.inline)(&v).filter(|d| data_picture(d) && !d.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '\'' | '\\' | '<')));
        }
        self.rules.picture_ok(&v).then_some(v)
    }
}

/// The sanitized document. `remote_images` allows the `http(s)` pictures Envoy would fetch
/// (the sender is allowed, or the user chose to load them); `inline` turns a `cid:` URL into a
/// `data:` URI, or None.
pub fn export(html: &str, remote_images: bool, inline: &mut dyn FnMut(&str) -> Option<String>) -> String {
    // `inline` is the caller's; nothing of it is left half-changed by a panic in here.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (out, rules) = written(html, remote_images, inline);
        finish(out, &rules, || crate::plain(&crate::parse_unguarded(html)))
    }))
    .unwrap_or_else(|_| {
        log::warn!("html: sanitizing a message for the browser panicked");
        String::new()
    })
}

/// The copy as written, before its check, and the rules it was written by.
fn written(html: &str, remote: bool, inline: &mut dyn FnMut(&str) -> Option<String>) -> (String, Rules) {
    let fetched = if remote {
        crate::parse_unguarded(html).images.iter().filter(|u| matches!(scheme(u).as_deref(), Some("http" | "https"))).map(|u| browser_url(u)).collect()
    } else {
        HashSet::new()
    };
    let mut w = Writer { rules: Rules { remote, fetched }, inline, css_inlined: 0 };
    let filtered = crate::prefilter(html);
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(&*filtered);
    let out = serialize(&dom, &mut w);
    (out, w.rules)
}

/// `out` if it passes [`check`]; otherwise the message's text in a `<pre>`.
fn finish(out: String, rules: &Rules, text: impl FnOnce() -> String) -> String {
    match check(&out, rules) {
        Ok(()) => out,
        Err(why) => {
            log::warn!("html: the browser copy failed its check ({why}); writing the message's text instead");
            plain_copy(&text())
        }
    }
}

fn plain_copy(text: &str) -> String {
    let mut out = String::from("\u{FEFF}<!DOCTYPE html>\n<html><head>");
    push_metas(&mut out, false);
    out.push_str("</head><body><pre style=\"white-space: pre-wrap\">");
    push_text(&mut out, text);
    out.push_str("</pre></body></html>\n");
    out
}

/// The `<meta>`s that open `<head>`, as attribute lists.
fn metas(remote: bool) -> [Vec<(&'static str, String)>; 4] {
    let img = if remote { "data: https: http:" } else { "data:" };
    [
        vec![("charset", "utf-8".to_owned())],
        vec![("http-equiv", "Content-Security-Policy".to_owned()), ("content", format!("default-src 'none'; img-src {img}; style-src 'unsafe-inline'; font-src 'none'; form-action 'none'; base-uri 'none'"))],
        vec![("name", "referrer".to_owned()), ("content", "no-referrer".to_owned())],
        vec![("http-equiv", "x-dns-prefetch-control".to_owned()), ("content", "off".to_owned())],
    ]
}

fn push_metas(out: &mut String, remote: bool) {
    for m in metas(remote) {
        out.push_str("<meta");
        for (k, v) in m {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            push_attr(out, &v);
            out.push('"');
        }
        out.push('>');
    }
}

enum Step {
    /// A node to write; true inside `<style>`.
    Node(Handle, bool),
    End(&'static str),
}

fn element(h: &Handle) -> Option<&str> {
    match &h.data {
        NodeData::Element { name, .. } if name.ns == ns!(html) => Some(name.local.as_ref()),
        _ => None,
    }
}

fn serialize(dom: &RcDom, w: &mut Writer) -> String {
    let mut out = String::from("\u{FEFF}<!DOCTYPE html>\n");
    let root = dom.document.children.borrow().iter().find(|c| element(c) == Some("html")).cloned();
    let Some(root) = root else {
        out.push_str("<html><head>");
        push_metas(&mut out, w.rules.remote);
        out.push_str("</head><body></body></html>\n");
        return out;
    };
    let mut stack = vec![Step::Node(root, false)];
    while let Some(step) = stack.pop() {
        let (node, in_style) = match step {
            Step::End(tag) => {
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
                continue;
            }
            Step::Node(n, s) => (n, s),
        };
        let push_children = |stack: &mut Vec<Step>, in_style: bool| {
            stack.extend(node.children.borrow().iter().rev().map(|c| Step::Node(c.clone(), in_style)));
        };
        match &node.data {
            NodeData::Text { contents } if in_style => out.push_str(&css(&contents.borrow(), w)),
            NodeData::Text { contents } => push_text(&mut out, &contents.borrow()),
            NodeData::Element { name, attrs, .. } => {
                // Foreign content (`<svg>`, `<math>` and all in them) goes whole.
                if name.ns != ns!(html) {
                    continue;
                }
                let local = name.local.as_ref();
                let Some(&tag) = KEEP.iter().find(|t| **t == local) else {
                    if !DROP.contains(&local) {
                        push_children(&mut stack, in_style);
                    }
                    continue;
                };
                out.push('<');
                out.push_str(tag);
                push_attrs(&mut out, tag, &attrs.borrow(), w);
                out.push('>');
                if tag == "head" {
                    push_metas(&mut out, w.rules.remote);
                } else if tag == "html" && !node.children.borrow().iter().any(|c| element(c) == Some("head")) {
                    out.push_str("<head>");
                    push_metas(&mut out, w.rules.remote);
                    out.push_str("</head>");
                }
                if VOID.contains(&tag) {
                    continue;
                }
                // The parser drops a newline right after `<pre>`: keep a leading one.
                if tag == "pre"
                    && let Some(NodeData::Text { contents }) = node.children.borrow().first().map(|c| &c.data)
                    && contents.borrow().starts_with('\n')
                {
                    out.push('\n');
                }
                stack.push(Step::End(tag));
                push_children(&mut stack, tag == "style");
            }
            // Comments, doctypes (one is written above), processing instructions.
            _ => {}
        }
    }
    out.push('\n');
    out
}

fn push_attrs(out: &mut String, tag: &str, attrs: &[html5ever::Attribute], w: &mut Writer) {
    let mut link = false;
    for a in attrs {
        // Namespaced attributes (`xlink:href`, `xml:lang`) belong to foreign content.
        if !a.name.ns.is_empty() {
            continue;
        }
        let n = a.name.local.as_ref();
        let value: Cow<str> = match url_attr(tag, n) {
            Some(Url::Link) => match w.rules.link(&a.value) {
                Some(v) => {
                    link = true;
                    v.into()
                }
                None => continue,
            },
            Some(Url::Picture) => match w.picture(&a.value) {
                Some(v) => v.into(),
                None => continue,
            },
            None if n == "style" => css(&a.value, w).into(),
            None if ATTRS.contains(&n) || element_attr(tag, n) => Cow::Borrowed(&a.value),
            None => continue,
        };
        out.push(' ');
        out.push_str(n);
        out.push_str("=\"");
        push_attr(out, &value);
        out.push('"');
    }
    if link {
        out.push_str(" rel=\"");
        out.push_str(REL);
        out.push('"');
    }
}

/// C0 controls other than tab, LF, FF and CR.
fn bad_control(c: char) -> bool {
    c < ' ' && !matches!(c, '\t' | '\n' | '\x0C' | '\r')
}

fn push_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if bad_control(c) => {}
            c => out.push(c),
        }
    }
}

fn push_attr(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if bad_control(c) => {}
            c => out.push(c),
        }
    }
}

/// A URL attribute as browsers read it: leading and trailing C0 controls and spaces trimmed,
/// tabs and newlines removed (and, here, every other C0 control too).
fn browser_url(v: &str) -> String {
    v.trim_matches(|c: char| c <= ' ').chars().filter(|c| *c >= ' ').collect()
}

/// The scheme of a URL read by [`browser_url`], lowercased, if it has one: a letter, then
/// letters, digits, `+`, `-` or `.`, up to a colon (`a/b:c` and `#x:y` are relative).
fn scheme(url: &str) -> Option<String> {
    let s = &url[..url.find(':')?];
    let mut chars = s.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

/// A `data:` URI of a picture format browsers draw.
fn data_picture(v: &str) -> bool {
    ["data:image/png", "data:image/jpeg", "data:image/jpg", "data:image/gif", "data:image/webp", "data:image/bmp"]
        .iter()
        .any(|p| v.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)) && matches!(v.as_bytes().get(p.len()), Some(b';' | b',')))
}

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
}

fn css_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0C)
}

/// The length of the escape at `s` (which starts with a backslash followed by something other
/// than a newline), and whether it's written in hex.
fn escape_len(s: &str) -> (usize, bool) {
    let b = s.as_bytes();
    let hex = b[1..].iter().take(6).take_while(|c| c.is_ascii_hexdigit()).count();
    if hex > 0 {
        let mut n = 1 + hex;
        if b.get(n) == Some(&b'\r') && b.get(n + 1) == Some(&b'\n') {
            n += 2;
        } else if b.get(n).is_some_and(|c| css_space(*c)) {
            n += 1;
        }
        return (n, true);
    }
    (1 + s[1..].chars().next().map_or(0, char::len_utf8), false)
}

/// Writes the escape at `s` to `out`, as written unless it escapes a character other than a
/// letter or digit: those are rewritten in hex (`\(` → `\28 `), so a raw `(` or `<` never
/// appears in an escape. Returns its length in `s`.
fn push_escape(out: &mut String, s: &str) -> usize {
    let (n, hex) = escape_len(s);
    match s[1..].chars().next() {
        Some(c) if !hex && !c.is_alphanumeric() => {
            out.push_str(&format!("\\{:X} ", c as u32));
        }
        _ => out.push_str(&s[..n]),
    }
    n
}

/// CSS for the copy, in one pass:
/// - `url()`s keep only pictures that may load ([`Writer::css_picture`]), written as
///   `url('…')`; the rest become `none`.
/// - Functions that load from bare strings ([`LOADERS`]) and function names written with
///   escapes (`\75rl(`) get a space before their parenthesis, so they're no longer functions.
/// - Comments are emptied (`/**/` still separates what it separated), and `(` and `<` inside
///   strings are written as escapes (the same string), so `url(` appears only as a real,
///   checked function.
/// - `<` becomes the escape `\3C `, so a sheet can't end its `<style>` (`</style>`) early;
///   a `<!--` (which CSS ignores) becomes a space.
/// - C0 controls (but tab, LF, FF and CR) are removed first, so removing them can't join
///   anything up.
fn css(src: &str, w: &mut Writer) -> String {
    let src: Cow<str> = if src.contains(bad_control) { src.chars().filter(|c| !bad_control(*c)).collect::<String>().into() } else { src.into() };
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 16);
    // Where the identifier being read starts in `out`, and whether it holds an escape.
    let mut ident: Option<(usize, bool)> = None;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = crate::find(b, i + 2, b"*/").map_or(b.len(), |e| e + 2);
                out.push_str("/**/");
                ident = None;
            }
            b'"' | b'\'' => {
                i = css_string(&src, i, &mut out);
                ident = None;
            }
            b'\\' if b.get(i + 1).is_some_and(|c| !matches!(c, b'\n' | b'\r' | 0x0C)) => {
                let start = ident.map_or(out.len(), |(s, _)| s);
                i += push_escape(&mut out, &src[i..]);
                ident = Some((start, true));
            }
            // `<!--` around a sheet (common in mail) is ignored by CSS; as `\3C !--` it would
            // be an identifier and take the first rule with it.
            b'<' if b[i..].starts_with(b"<!--") => {
                out.push(' ');
                ident = None;
                i += 4;
            }
            b'<' => {
                // An escape, so it starts (or continues) an identifier, as in CSS.
                let start = ident.map_or(out.len(), |(s, _)| s);
                out.push_str("\\3C ");
                ident = Some((start, true));
                i += 1;
            }
            b'(' => {
                let name = ident.map(|(s, escaped)| (s, escaped, out[s..].to_ascii_lowercase()));
                ident = None;
                match name {
                    Some((_, true, _)) => out.push_str(" ("),
                    Some((s, false, n)) if n == "url" => {
                        out.truncate(s);
                        let (end, arg) = url_arg(&src, i + 1);
                        match arg.and_then(|a| w.css_picture(a)) {
                            Some(u) => {
                                out.push_str("url('");
                                out.push_str(&u);
                                out.push_str("')");
                            }
                            None => out.push_str("none"),
                        }
                        i = end;
                        continue;
                    }
                    Some((_, false, n)) if LOADERS.contains(&n.as_str()) => out.push_str(" ("),
                    _ => out.push('('),
                }
                i += 1;
            }
            c if ident_byte(c) => {
                if ident.is_none() {
                    ident = Some((out.len(), false));
                }
                let n = src[i..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&src[i..i + n]);
                i += n;
            }
            c => {
                out.push(c as char);
                ident = None;
                i += 1;
            }
        }
    }
    out
}

/// Copies the quoted string at `i` to `out`, with `(` and `<` written as escapes; returns
/// where it ends. A newline ends it unclosed, as in CSS.
fn css_string(src: &str, i: usize, out: &mut String) -> usize {
    let b = src.as_bytes();
    let q = b[i];
    out.push(q as char);
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            c if c == q => {
                out.push(q as char);
                return j + 1;
            }
            b'\n' | b'\r' | 0x0C => return j,
            b'\\' => match b.get(j + 1) {
                None => {
                    out.push('\\');
                    j += 1;
                }
                // A line continuation.
                Some(b'\n' | b'\r' | 0x0C) => {
                    let n = if b[j + 1] == b'\r' && b.get(j + 2) == Some(&b'\n') { 3 } else { 2 };
                    out.push_str(&src[j..j + n]);
                    j += n;
                }
                Some(_) => j += push_escape(out, &src[j..]),
            },
            b'(' => {
                out.push_str("\\28 ");
                j += 1;
            }
            b'<' => {
                out.push_str("\\3C ");
                j += 1;
            }
            _ => {
                let n = src[j..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&src[j..j + n]);
                j += n;
            }
        }
    }
    j
}

/// A `url(` argument starting at `from`: where the function ends (after its `)`), and the URL
/// if it's well-formed and free of escapes.
fn url_arg(src: &str, from: usize) -> (usize, Option<&str>) {
    let b = src.as_bytes();
    let mut i = from;
    while i < b.len() && css_space(b[i]) {
        i += 1;
    }
    // Byte offsets of ASCII characters, so always on character boundaries.
    let paren = |from: usize| crate::find(b, from, b")");
    match b.get(i) {
        Some(&q) if q == b'"' || q == b'\'' => {
            let Some(len) = src[i + 1..].find(q as char) else { return (b.len(), None) };
            let url = &src[i + 1..i + 1 + len];
            let after = i + 1 + len + 1;
            let Some(p) = paren(after) else { return (b.len(), None) };
            let clean = !url.contains(['\\', '\n', '\r', '\x0C']) && src[after..p].bytes().all(css_space);
            (p + 1, clean.then_some(url))
        }
        _ => {
            let Some(p) = paren(i) else { return (b.len(), None) };
            let url = src[i..p].trim_end_matches(|c: char| c.is_ascii_whitespace());
            let clean = !url.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | '(' | '\\'));
            (p + 1, clean.then_some(url))
        }
    }
}

/// Reads `out` back as a browser would and checks that everything in the tree is allowed:
/// HTML elements in [`KEEP`] only (and the policy `<meta>`s, first in `<head>`), allowed
/// attributes only, URL attributes that pass the rules as they stand, CSS whose `url()`s do too
/// and with no `<`, no comments, no C0 controls.
fn check(out: &str, rules: &Rules) -> Result<(), String> {
    let markup = out.strip_prefix('\u{FEFF}').ok_or("no byte-order mark")?;
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(markup);
    let metas = metas(rules.remote);
    let is_meta = |attrs: &[html5ever::Attribute], m: &[(&str, String)]| attrs.len() == m.len() && m.iter().all(|(k, v)| attrs.iter().any(|a| a.name.ns.is_empty() && &*a.name.local == *k && &*a.value == v.as_str()));
    let mut seen_metas = 0;
    let mut heads = 0;
    let mut stack: Vec<(Handle, bool)> = vec![(dom.document.clone(), false)];
    while let Some((n, in_style)) = stack.pop() {
        let mut style = false;
        match &n.data {
            NodeData::Document | NodeData::Doctype { .. } => {}
            NodeData::Comment { .. } => return Err("a comment".into()),
            NodeData::ProcessingInstruction { .. } => return Err("a processing instruction".into()),
            NodeData::Text { contents } => {
                let t = contents.borrow();
                if t.contains(bad_control) {
                    return Err("a control character in text".into());
                }
                if in_style {
                    css_ok(&t, rules).map_err(|e| format!("<style>: {e}"))?;
                }
            }
            NodeData::Element { name, attrs, .. } => {
                if name.ns != ns!(html) {
                    return Err(format!("<{}> outside HTML", name.local));
                }
                let tag = name.local.as_ref();
                let attrs = attrs.borrow();
                if tag == "meta" {
                    if !metas.iter().any(|m| is_meta(&attrs, m)) {
                        return Err("a <meta> of its own".into());
                    }
                    seen_metas += 1;
                    continue;
                }
                if !KEEP.contains(&tag) {
                    return Err(format!("<{tag}>"));
                }
                if tag == "head" {
                    heads += 1;
                    let kids = n.children.borrow();
                    let mut els = kids.iter().filter_map(|c| if let NodeData::Element { attrs, .. } = &c.data { Some((element(c), attrs)) } else { None });
                    if !metas.iter().all(|m| els.next().is_some_and(|(t, a)| t == Some("meta") && is_meta(&a.borrow(), m))) {
                        return Err("the policy isn't first in <head>".into());
                    }
                }
                let mut href = false;
                let mut rel = false;
                for a in attrs.iter() {
                    let k = a.name.local.as_ref();
                    let v = &*a.value;
                    if !a.name.ns.is_empty() {
                        return Err(format!("<{tag} {k}> in a namespace"));
                    }
                    if v.contains(bad_control) {
                        return Err(format!("a control character in <{tag} {k}>"));
                    }
                    let ok = match url_attr(tag, k) {
                        Some(Url::Link) => {
                            href = true;
                            rules.link(v).as_deref() == Some(v)
                        }
                        Some(Url::Picture) => rules.picture_ok(v),
                        None if tag == "a" && k == "rel" => {
                            rel = true;
                            v == REL
                        }
                        None if k == "style" => {
                            css_ok(v, rules).map_err(|e| format!("<{tag} style>: {e}"))?;
                            true
                        }
                        None => ATTRS.contains(&k) || element_attr(tag, k),
                    };
                    if !ok {
                        return Err(format!("<{tag} {k}=\"{v}\">"));
                    }
                }
                if href && !rel {
                    return Err("a link without rel".into());
                }
                style = tag == "style";
            }
        }
        stack.extend(n.children.borrow().iter().map(|c| (c.clone(), style)));
    }
    if heads != 1 || seen_metas != metas.len() {
        return Err("no policy".into());
    }
    Ok(())
}

/// CSS as it should come out of [`css`]: no `<`; every `url(` written `url('…')` around a
/// picture that may load; no [`LOADERS`]; no function name with an escape in it.
fn css_ok(css: &str, rules: &Rules) -> Result<(), String> {
    if css.contains('<') {
        return Err("a `<`".into());
    }
    let b = css.as_bytes();
    for (p, _) in css.match_indices('(') {
        let mut k = p;
        while k > 0 && ident_byte(b[k - 1]) {
            k -= 1;
        }
        let name = css[k..p].to_ascii_lowercase();
        // `\75rl(`, or `\75 rl(` (a hex escape ends at one space).
        let escaped = (k > 0 && b[k - 1] == b'\\') || {
            let mut m = k;
            if m > 0 && css_space(b[m - 1]) {
                m -= 1;
            }
            let digits_end = m;
            while m > 0 && digits_end - m < 6 && b[m - 1].is_ascii_hexdigit() {
                m -= 1;
            }
            m < digits_end && m > 0 && b[m - 1] == b'\\' && !name.is_empty()
        };
        if escaped {
            return Err(format!("an escaped function name before `(` at {p}"));
        }
        if LOADERS.contains(&name.as_str()) {
            return Err(format!("{name}()"));
        }
        if name == "url" {
            let rest = &css[p + 1..];
            let url = rest.strip_prefix('\'').and_then(|r| r.find("')").map(|e| &r[..e])).ok_or("an unquoted url()")?;
            if url.contains('\'') || !rules.picture_ok(url) {
                return Err(format!("url('{url}')"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(cid: &str) -> Option<String> {
        (cid == "cid:logo@x").then(|| "data:image/png;base64,iVBORw0KGgo=".to_owned())
    }

    fn run(html: &str, remote: bool) -> String {
        export(html, remote, &mut png)
    }

    /// The copy before its check, and the check's verdict.
    fn checked(html: &str, remote: bool) -> (String, Result<(), String>) {
        let (out, rules) = written(html, remote, &mut png);
        let verdict = check(&out, &rules);
        (out, verdict)
    }

    #[test]
    fn active_content_is_removed() {
        let html = r##"<html><head><meta http-equiv="refresh" content="0;url=https://evil.example"><base href="https://evil.example/"><link rel=stylesheet href="https://evil.example/a.css"><script>alert(1)</script><title>T</title></head>
            <body onload="steal()"><p onclick="x()" style="color:red">Hi <a href="javascript:alert(1)">bad</a> <a href=" java	script:alert(1)">tabbed</a> <a href="https://ok.example/" ping="https://t.example">good</a> <a href="#top">top</a> <a href="mailto:a@b.c">mail</a></p>
            <iframe src="https://evil.example"></iframe><object data="x.swf"></object><embed src="x"><form action="https://evil.example/post"><input name=q><button>Go</button><p>In a form</p></form>
            <svg><script>alert(2)</script><a xlink:href="javascript:alert(3)"><text>svg</text></a></svg><noscript><img src="https://t.example/ns.gif"></noscript><video src="https://e.x/v.mp4"></video></body></html>"##;
        let out = run(html, true);
        for bad in ["<script", "alert(", "javascript", "onload", "onclick", "refresh", "<base", "<link", "evil.example", "<iframe", "<object", "<embed", "<input", "<button", "<form", "ping=", "<noscript", "ns.gif", "<video", "steal", "<svg", ">svg<"] {
            assert!(!out.to_ascii_lowercase().contains(bad), "{bad} survived:\n{out}");
        }
        for good in ["Hi ", "https://ok.example/", "#top", "mailto:a@b.c", "In a form", "color:red", "<title>T</title>", "noopener noreferrer"] {
            assert!(out.contains(good), "{good} lost:\n{out}");
        }
    }

    #[test]
    fn a_strict_policy_comes_first() {
        let out = run("<p>x</p>", false);
        assert!(out.starts_with("\u{FEFF}<!DOCTYPE html>\n<html><head>"), "a byte-order mark, then the doctype: {out:?}");
        let head = out.find("<head>").expect("a head") + "<head>".len();
        assert!(
            out[head..].starts_with(r#"<meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src &#39;none&#39;; img-src data:; style-src &#39;unsafe-inline&#39;; font-src &#39;none&#39;; form-action &#39;none&#39;; base-uri &#39;none&#39;"><meta name="referrer" content="no-referrer"><meta http-equiv="x-dns-prefetch-control" content="off">"#),
            "{out}"
        );
        assert!(run("<p>x</p>", true).contains("img-src data: https: http:;"));
        // Read back as a browser reads it, the policy is intact.
        let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(&out[3..]);
        let html = dom.document.children.borrow().iter().find(|c| element(c) == Some("html")).cloned().unwrap();
        let head = html.children.borrow()[0].clone();
        let NodeData::Element { attrs, .. } = &head.children.borrow()[1].data else { panic!() };
        assert_eq!(&*attrs.borrow()[1].value, "default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src 'none'; form-action 'none'; base-uri 'none'");
    }

    #[test]
    fn only_allowed_elements_and_attributes_are_written() {
        let html = r##"<html xmlns:o="urn:schemas-microsoft-com:office:office" lang="en"><body bgcolor="#eee" text="#333" onload="x()">
            <table width="600" cellpadding="4" align="center" data-x="1" role="presentation"><tr><td valign="top" nowrap style="color:red" aria-label="a">Cell<o:p>&nbsp;</o:p></td></tr></table>
            <font face="Georgia" size="3" color="navy">Font</font><x-card class="c"><b>Custom</b></x-card><details><summary>Sum</summary>Body</details>
            <p><img src="cid:logo@x" width="80" height="20" alt="Logo" loading="lazy" decoding="async" usemap="#m"><map name="m"><area href="https://e.x/"></map>
            <a href="https://e.x/" target="_blank" rel="opener" name="top" download>Link</a><canvas>Canvas fallback</canvas><select><option>Opt</option></select><textarea>Area</textarea></p></body></html>"##;
        let out = run(html, false);
        for good in [
            r#"<html lang="en">"#,
            r##"<body bgcolor="#eee" text="#333">"##,
            r#"<table width="600" cellpadding="4" align="center"><tbody><tr><td valign="top" nowrap="" style="color:red">Cell"#,
            "<font face=\"Georgia\" size=\"3\" color=\"navy\">Font</font>",
            "<b>Custom</b>",
            "SumBody",
            r#"<img src="data:image/png;base64,iVBORw0KGgo=" width="80" height="20" alt="Logo">"#,
            r#"<a href="https://e.x/" name="top" rel="noopener noreferrer">Link</a>"#,
        ] {
            assert!(out.contains(good), "{good} lost:\n{out}");
        }
        for bad in ["xmlns", "onload", "data-x", "role=", "aria-", "<o:p", "<x-card", "<details", "<summary", "loading", "decoding", "usemap", "<map", "<area", "target", "opener\"", "download", "Canvas", "<canvas", "Opt", "Area"] {
            assert!(!out.contains(bad), "{bad} survived:\n{out}");
        }
    }

    #[test]
    fn foreign_content_and_legacy_raw_text_go_whole() {
        // Trees that reserialize to markup that parses differently: `<style>` and friends in
        // SVG or MathML read their content as markup, the legacy raw-text elements don't.
        let cases = [
            r#"<svg><style><img src=x onerror=alert(1)></style></svg>"#,
            r#"<math><mtext><table><mglyph><style><img src=x onerror=alert(1)>"#,
            r#"<math><annotation-xml encoding="text/html"><style><img src=x onerror=alert(1)></style></annotation-xml></math>"#,
            r#"<svg><foreignObject><p>in svg</p><style>a{}</style></foreignObject><desc><b>desc</b></desc></svg>"#,
            r#"<noembed><img title="</noembed><img src=x onerror=alert(1)>"></noembed>"#,
            r#"<noframes><p title="</noframes><img src=x onerror=alert(1)>"></noframes>"#,
            r#"<xmp><img src=x onerror=alert(1)></xmp>"#,
            r#"<p>before</p><plaintext><img src=x onerror=alert(1)>"#,
            r#"<svg></p><style><a id="</style><img src=1 onerror=alert(1)>">"#,
            r#"<math><mi><svg><title><style><img src=x onerror=alert(1)></style></title></svg></mi></math>"#,
        ];
        for html in cases {
            let (out, verdict) = checked(html, true);
            assert_eq!(verdict, Ok(()), "{html}\n{out}");
            for bad in ["onerror", "<svg", "<math", "<mtext", "<mglyph", "<annotation", "<foreignobject", "<xmp", "<noembed", "<noframes", "<plaintext", "in svg", "desc"] {
                assert!(!out.to_ascii_lowercase().contains(bad), "{bad} survived {html}:\n{out}");
            }
        }
    }

    #[test]
    fn style_text_cant_end_its_element() {
        let out = run(r#"<style>.q::before { content: "</stylex><b>(" } p { color: red }</style><p>x</p>"#, false);
        assert!(out.contains(r#"content: "\3C /stylex>\3C b>\28 " }"#), "{out}");
        // A sheet wrapped in `<!-- -->`, as much mail has it, keeps its first rule.
        let out = run("<style><!--\np.first { color: red }\n--></style><p class=first>x</p>", false);
        assert!(out.contains("<style> \np.first { color: red }\n--></style>"), "{out}");
        // A tree whose sheet says `</style>`, as built by hand (no parse makes one): still one
        // sheet when read back.
        let dom = html5ever::parse_document(RcDom::default(), Default::default()).one("<style>x</style><p>after</p>");
        let mut stack = vec![dom.document.clone()];
        while let Some(n) = stack.pop() {
            if let NodeData::Text { contents } = &n.data
                && &**contents.borrow() == "x"
            {
                *contents.borrow_mut() = "a{}</style><img src=x onerror=alert(1)><style>".into();
            }
            stack.extend(n.children.borrow().iter().cloned());
        }
        let mut w = Writer { rules: Rules { remote: false, fetched: HashSet::new() }, inline: &mut png, css_inlined: 0 };
        let out = serialize(&dom, &mut w);
        assert_eq!(check(&out, &w.rules), Ok(()), "{out}");
        assert!(out.contains(r"<style>a{}\3C /style>\3C img src=x onerror=alert(1)>\3C style></style></head>"), "{out}");
        assert!(out.contains("<body><p>after</p></body>"), "{out}");
    }

    #[test]
    fn css_loads_only_what_envoy_would() {
        let html = r#"<style>
              .a { background: url(cid:logo@x) } .b { background: URL( "https://img.example/hero.jpg" ) } .c { background: url(file:///etc/passwd) }
              .d { background-image: image-set("https://t.example/x.png" 1x) } .e { background: \75rl(https://t.example/esc) } .f { background: u\72l(https://t.example/esc2) }
              /* url(https://t.example/comment) */ .g::after { content: "url(https://t.example/str)" } @import url(https://t.example/a.css);
              .h { background: url('data:image/gif;base64,R0lGOD==') } .i { background: url(data:text/html,<b>) }
            </style><div class="b"><p>Hero</p></div><div style="background:url(&quot;cid:logo@x&quot;) no-repeat"><p>Inline</p></div>
            <div style="background-image:url(https://t.example/empty-box)"></div>"#;
        for remote in [false, true] {
            let (out, verdict) = checked(html, remote);
            assert_eq!(verdict, Ok(()), "{out}");
            assert!(out.contains(".a { background: url('data:image/png;base64,iVBORw0KGgo=') }"), "{out}");
            assert!(out.contains(r#"style="background:url(&#39;data:image/png;base64,iVBORw0KGgo=&#39;) no-repeat""#), "{out}");
            assert!(out.contains("url('data:image/gif;base64,R0lGOD==')"), "{out}");
            for bad in ["url('https://t.example", "url(https://t.example", "url(&#39;https://t.example", "file:", "image-set(", "text/html", "cid:", "comment"] {
                assert!(!out.contains(bad), "{bad} survived (remote {remote}):\n{out}");
            }
            assert!(out.contains(r#"image-set ("https://t.example/x.png" 1x)"#), "a string can't load once it isn't an argument: {out}");
            assert!(out.contains(r"\75rl (") && out.contains(r"u\72l ("), "escaped names stop being functions: {out}");
            assert!(out.contains(r#"content: "url\28 https://t.example/str)""#) && out.contains("@import none;"), "{out}");
            assert!(out.contains("background: none }") && out.contains("background-image:none"), "{out}");
            // The hero is in a box with content, so Envoy fetches it when allowed.
            assert_eq!(out.contains("url('https://img.example/hero.jpg')"), remote, "{out}");
        }
    }

    #[test]
    fn pictures_follow_the_image_setting_and_embedded_ones_inline() {
        let html = r#"<img src="https://img.example/a.png" alt="A"><img src="cid:logo@x"><img src="cid:missing@x" alt="M"><table><tr><td background="https://img.example/b.png">c</td></tr></table>
            <img src="data:image/png;base64,AAAA"><img src="data:text/html;base64,PHNjcmlwdD4="><img src="data:image/svg+xml,<svg/>"><img src="file:///etc/passwd"><img src="relative.png"><img srcset="https://img.example/2x.png 2x">"#;
        let blocked = run(html, false);
        assert!(!blocked.contains("https://img.example/a.png") && !blocked.contains("https://img.example/b.png"), "{blocked}");
        assert!(blocked.contains(r#"alt="A""#), "alt text stays");
        assert_eq!(blocked.matches("data:image/png;base64,iVBORw0KGgo=").count(), 1, "{blocked}");
        assert!(blocked.contains("data:image/png;base64,AAAA"));
        for bad in ["data:text/html", "svg", "file:", "relative.png", "srcset", "cid:"] {
            assert!(!blocked.contains(bad), "{bad} survived:\n{blocked}");
        }
        let allowed = run(html, true);
        assert!(allowed.contains(r#"src="https://img.example/a.png""#) && allowed.contains(r#"background="https://img.example/b.png""#), "{allowed}");
        // A resolver handing back something that isn't a picture is ignored.
        let out = export(r#"<img src="cid:a@x">"#, false, &mut |_| Some("data:text/html,<script>alert(1)</script>".into()));
        assert!(!out.contains("text/html") && out.contains("<img>"), "{out}");
    }

    #[test]
    fn tracking_pixels_stay_out_when_images_are_allowed() {
        let html = r#"<style>.px { background: url(https://t.example/sheet.gif) }</style>
            <img src="https://t.example/open.gif" width="1" height="1"><img src="https://t.example/max.gif" style="max-width:1px;max-height:1px">
            <img src="https://t.example/zero.gif" width="0" height="0"><div style="display:none"><img src="https://t.example/hidden.gif"></div>
            <table><tr><td width="1" height="1" background="https://t.example/cell.gif">x</td><td background="https://img.example/cell.jpg">Cell</td></tr></table>
            <div style="width:1px;height:1px;background:url(https://t.example/box.gif)">x</div><div class="px" style="height:1px">y</div>
            <img src="https://img.example/hero.jpg" width="600" height="300" alt="Hero">"#;
        let (out, verdict) = checked(html, true);
        assert_eq!(verdict, Ok(()), "{out}");
        assert!(!out.contains("t.example"), "trackers survived:\n{out}");
        assert!(out.contains(r#"src="https://img.example/hero.jpg""#) && out.contains(r#"background="https://img.example/cell.jpg""#), "{out}");
    }

    #[test]
    fn links_keep_a_scheme_or_stay_in_the_page() {
        let html = r##"<a href="#top">a</a><a href=" #x ">b</a><a href="//evil.example/x">c</a><a href="\\server\share\x">d</a><a href="/etc/passwd">e</a><a href="../x.html">f</a>
            <a href="x.html">g</a><a href="?q=1">h</a><a href="">i</a><a href="http s://e.x/">j</a><a href="1http://e.x/">k</a><a href="file:///etc/passwd">l</a><a href="smb://server/x">m</a>
            <a href="HTTPS://ok.example/">n</a><a href="mailto:a@b.c">o</a><a href="tel:+1555">p</a><a href="ht&#9;tp://tab.example/">q</a>"##;
        let out = run(html, false);
        for good in [r##"href="#top""##, r##"href="#x""##, r#"href="HTTPS://ok.example/""#, r#"href="mailto:a@b.c""#, r#"href="tel:+1555""#, r#"href="http://tab.example/""#] {
            assert!(out.contains(good), "{good} lost:\n{out}");
        }
        assert_eq!(out.matches("href=").count(), 6, "{out}");
        assert_eq!(out.matches("rel=").count(), 6, "{out}");
    }

    #[test]
    fn text_and_attributes_are_escaped_and_cleaned() {
        let html = "<!-- a secret --><p title=\"a&quot;b'c&lt;d&gt;e&amp;f\u{1}\u{1b}g\">x &lt;y&gt; &amp; z\u{1}\u{8}\u{b}\u{1f}\t\u{c}end<!-- another --></p><?php echo 1 ?>";
        let (out, verdict) = checked(html, false);
        assert_eq!(verdict, Ok(()), "{out}");
        assert!(out.contains(r#"<p title="a&quot;b&#39;c&lt;d&gt;e&amp;fg">x &lt;y&gt; &amp; z"#), "{out:?}");
        assert!(out.contains("z\t\u{c}end</p>"), "tab and form feed stay: {out:?}");
        for bad in ["secret", "another", "<!--", "php", "\u{1}", "\u{8}", "\u{b}", "\u{1b}", "\u{1f}"] {
            assert!(!out.contains(bad), "{bad:?} survived:\n{out:?}");
        }
        // A leading newline in `<pre>` survives the parser dropping one.
        let out = run("<pre>\n\nindented</pre>", false);
        assert!(out.contains("<pre>\n\nindented</pre>"), "{out:?}");
    }

    #[test]
    fn a_failed_check_writes_the_text_instead() {
        let rules = Rules { remote: false, fetched: HashSet::new() };
        for bad in ["\u{FEFF}<!DOCTYPE html><svg><p>x</p></svg>", "<!DOCTYPE html><p>no mark</p>", "\u{FEFF}<p><!-- c --></p>", "\u{FEFF}<html><head></head><body><img src=https://t.example/p.gif></body></html>"] {
            assert!(check(bad, &rules).is_err(), "{bad}");
            let out = finish(bad.to_owned(), &rules, || "Hello <b>&".into());
            assert!(out.contains("<pre style=\"white-space: pre-wrap\">Hello &lt;b&gt;&amp;</pre>"), "{out}");
            assert_eq!(check(&out, &rules), Ok(()), "the text copy passes the check too");
        }
    }

    #[test]
    fn every_fixture_passes_the_check() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../envoy-core/tests/fixtures/html");
        let mut n = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_none_or(|x| x != "html") {
                continue;
            }
            let html = std::fs::read_to_string(&p).unwrap();
            for remote in [false, true] {
                let (out, verdict) = checked(&html, remote);
                assert_eq!(verdict, Ok(()), "{} (remote {remote})", p.display());
                assert!(out.len() > html.len() / 10, "{} lost most of itself", p.display());
            }
            n += 1;
        }
        assert!(n >= 20, "{n} fixtures");
    }

    /// xorshift64*: deterministic, so a failure reproduces.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    #[test]
    fn random_documents_pass_the_check() {
        // Pieces that confuse parsers: foreign content and its integration points, raw-text
        // and RCDATA elements, tables, selects, templates, comments, CDATA, stray end tags,
        // controls, CSS escapes.
        let tags = [
            "svg", "math", "mtext", "mi", "mglyph", "malignmark", "annotation-xml encoding=\"text/html\"", "annotation-xml", "foreignObject", "desc", "title", "style", "xmp", "noembed", "noframes",
            "noscript", "iframe", "textarea", "select", "option", "template", "table", "tbody", "tr", "td", "th", "caption", "colgroup", "col", "form", "a href=\"https://e.x/\"",
            "a href=\"javascript:alert(1)\"", "p", "div", "b", "i", "font color=red", "font face=x", "center", "img src=\"https://e.x/i.png\"", "img src=\"cid:logo@x\"", "br", "image", "listing", "pre",
            "body", "html", "head", "keygen", "embed", "object", "isindex", "button", "li", "ul", "dl", "dd", "h1", "o:p", "x-foo", "details", "picture", "source", "map", "area",
            "base href=//evil.example/", "meta http-equiv=refresh content=0", "link rel=stylesheet href=x", "frameset", "plaintext",
        ];
        let attrs = [
            "style=\"background:url(https://t.example/a.png)\"", "title=\"</style><b>\"", "class=x", "href=\"//evil\"", "background=\"https://t.example/b.png\"", "onerror=alert(1)",
            "xlink:href=javascript:alert(1)", "width=1 height=1", "data-x=1", "style=\"a:\\75rl(https://t.example/c)\"", "src=\"java\tscript:x\"", "dir=rtl",
        ];
        let raw = [
            "text", "</style>", "</noembed>", "</xmp>", "</title>", "</textarea>", "</script>", "<!--", "-->", "--!>", "<![CDATA[", "]]>", "<?xml x?>", "<!doctype html>", "</", "<", ">", "&lt;",
            "&amp;lt;", "&#0;", "&#x1;", "\u{1}", "\u{0}", "\r\n", "\"", "'", "<img src=x onerror=alert(1)>", "url(https://t.example/p.gif)", "a{b:c}", "\\3C", "\\75rl(https://t.example/x)",
            "image-set(\"https://t.example/x\" 1x)", "</p>", "</table>", "</svg>", "</math>", "</select>", "</template>", "</noscript>", "</iframe>", " ", "\n",
        ];
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for round in 0..400 {
            let mut html = String::new();
            for _ in 0..(rng.next() % 80) {
                match rng.next() % 5 {
                    0 | 1 => html.push_str(rng.pick(&raw)),
                    2 => {
                        html.push_str("</");
                        html.push_str(rng.pick(&tags).split(' ').next().unwrap());
                        html.push('>');
                    }
                    _ => {
                        html.push('<');
                        html.push_str(rng.pick(&tags));
                        for _ in 0..(rng.next() % 3) {
                            html.push(' ');
                            html.push_str(rng.pick(&attrs));
                        }
                        html.push('>');
                    }
                }
            }
            for remote in [false, true] {
                let (out, verdict) = checked(&html, remote);
                assert_eq!(verdict, Ok(()), "round {round} (remote {remote}):\n{html:?}\n→\n{out}");
            }
        }
    }

    #[test]
    fn deep_documents_dont_overflow() {
        let html = format!("{}x{}", "<div><form>".repeat(50_000), "</form></div>".repeat(50_000));
        let t = std::thread::Builder::new().stack_size(2 << 20).spawn(move || export(&html, false, &mut |_| None)).unwrap();
        let out = t.join().unwrap();
        assert!(out.contains(">x<") && !out.contains("<pre"), "{}", &out[..out.len().min(300)]);
    }
}
