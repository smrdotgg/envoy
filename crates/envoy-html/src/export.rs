//! A message's HTML made safe to open in a browser.
//!
//! "Open in browser" hands the system browser a file. The browser runs whatever is in it and
//! loads whatever it points at, regardless of Envoy's settings, so the file is rebuilt first:
//!
//! - Active content goes: `<script>`, `<noscript>`, frames, `<object>`/`<embed>`/`<applet>`,
//!   `<base>`, `<link>` (stylesheets, preloads), every original `<meta>` (refresh, CSP of its
//!   own), `<template>`, media, form controls; `<form>`s are unwrapped. Event-handler
//!   attributes (`on…`) and fetch-ish ones (`ping`, `srcset`, `action`…) go too.
//! - URLs are checked after removing the whitespace and control characters browsers ignore:
//!   links keep `http(s)`, `mailto`, `tel` and in-page targets; pictures keep `http(s)` only
//!   when remote images are allowed for this message, `cid:` becomes the embedded picture as a
//!   `data:` URI, and other `data:` URIs only as images. Everything else (`javascript:`,
//!   `vbscript:`, `file:`…) is removed.
//! - A strict Content-Security-Policy `<meta>` comes first in `<head>`: nothing loads or runs
//!   but inline styles and `data:` pictures (plus `http(s)` pictures when allowed), so even
//!   what the rewrite missed — `url()` in CSS, `@import`, fonts — stays blocked. A
//!   `no-referrer` policy keeps link clicks from telling sites where they came from.
//!
//! The tree is walked with a stack, and html5ever's serializer is iterative too, so deep
//! documents can't overflow the stack.

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, Namespace, QualName, ns};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom, SerializableHandle};

/// Elements removed with everything inside them.
const DROP: &[&str] = &[
    "script", "noscript", "iframe", "frame", "frameset", "object", "embed", "applet", "base", "link", "meta", "template", "portal", "audio", "video", "source", "track", "param",
    "input", "button", "select", "textarea", "option", "optgroup", "datalist", "keygen", "output", "dialog",
];

/// Elements replaced by their content.
const UNWRAP: &[&str] = &["form"];

/// Attributes that fetch, submit or embed: removed wherever they are.
const DROP_ATTRS: &[&str] = &["srcset", "lowsrc", "dynsrc", "ping", "action", "formaction", "srcdoc", "longdesc", "data", "codebase", "archive", "manifest", "profile", "http-equiv", "integrity", "imagesrcset"];

/// What a URL attribute holds.
enum Kind {
    /// Followed when clicked: `<a href>`, `<area href>`.
    Link,
    /// Loaded at once: `src`, `background`, `poster`, other elements' `href`.
    Picture,
}

/// The sanitized document. `remote_images` allows `http(s)` pictures (the sender is allowed,
/// or the user chose to load them); `inline` turns a `cid:` URL into a `data:` URI, or None.
pub fn export(html: &str, remote_images: bool, inline: &mut dyn FnMut(&str) -> Option<String>) -> String {
    // `inline` is the caller's; nothing of it is left half-changed by a panic in here.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| export_unguarded(html, remote_images, inline))).unwrap_or_else(|_| {
        log::warn!("html: sanitizing a message for the browser panicked");
        String::new()
    })
}

fn export_unguarded(html: &str, remote_images: bool, inline: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let html = crate::prefilter(html);
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).from_utf8().read_from(&mut html.as_bytes());
    let Ok(dom) = dom else { return String::new() };
    let mut head: Option<Handle> = None;
    let mut stack = vec![dom.document.clone()];
    while let Some(node) = stack.pop() {
        if head.is_none() && name(&node).as_deref() == Some("head") {
            head = Some(node.clone());
        }
        // Rebuild the child list: dropped elements out, unwrapped ones replaced by their
        // children (checked in turn).
        let old = std::mem::take(&mut *node.children.borrow_mut());
        let mut queue: std::collections::VecDeque<Handle> = old.into();
        let mut kept = Vec::with_capacity(queue.len());
        while let Some(c) = queue.pop_front() {
            match name(&c) {
                Some(n) if DROP.contains(&n.as_str()) => continue,
                Some(n) if UNWRAP.contains(&n.as_str()) => {
                    let kids = std::mem::take(&mut *c.children.borrow_mut());
                    for k in kids.into_iter().rev() {
                        queue.push_front(k);
                    }
                    continue;
                }
                Some(n) => clean_attrs(&c, &n, remote_images, inline),
                None => {}
            }
            if let NodeData::Text { contents } = &c.data
                && name(&node).as_deref() == Some("style")
            {
                let text = contents.borrow().to_string();
                if text.contains("cid:") {
                    *contents.borrow_mut() = StrTendril::from(inline_css(&text, inline));
                }
            }
            if matches!(c.data, NodeData::ProcessingInstruction { .. }) {
                continue;
            }
            kept.push(c);
        }
        for c in &kept {
            c.parent.set(Some(std::rc::Rc::downgrade(&node)));
        }
        stack.extend(kept.iter().cloned());
        *node.children.borrow_mut() = kept;
    }
    if let Some(head) = &head {
        let img = if remote_images { "data: https: http:" } else { "data:" };
        let csp = format!("default-src 'none'; img-src {img}; style-src 'unsafe-inline'; font-src 'none'; form-action 'none'; base-uri 'none'");
        let metas = [
            meta(&[("charset", "utf-8")]),
            meta(&[("http-equiv", "Content-Security-Policy"), ("content", &csp)]),
            meta(&[("name", "referrer"), ("content", "no-referrer")]),
        ];
        let mut kids = head.children.borrow_mut();
        for (k, m) in metas.into_iter().enumerate() {
            m.parent.set(Some(std::rc::Rc::downgrade(head)));
            kids.insert(k, m);
        }
    }
    let mut out = Vec::new();
    let has_doctype = dom.document.children.borrow().iter().any(|c| matches!(c.data, NodeData::Doctype { .. }));
    if !has_doctype {
        out.extend_from_slice(b"<!DOCTYPE html>\n");
    }
    let opts = html5ever::serialize::SerializeOpts { traversal_scope: html5ever::serialize::TraversalScope::ChildrenOnly(None), ..Default::default() };
    let _ = html5ever::serialize(&mut out, &SerializableHandle::from(dom.document.clone()), opts);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn name(h: &Handle) -> Option<String> {
    if let NodeData::Element { name, .. } = &h.data { Some(name.local.as_ref().to_ascii_lowercase()) } else { None }
}

fn meta(attrs: &[(&str, &str)]) -> Handle {
    Node::new(NodeData::Element {
        name: QualName::new(None, ns!(html), LocalName::from("meta")),
        attrs: std::cell::RefCell::new(attrs.iter().map(|(k, v)| Attribute { name: QualName::new(None, Namespace::from(""), LocalName::from(*k)), value: StrTendril::from(*v) }).collect()),
        template_contents: std::cell::RefCell::new(None),
        mathml_annotation_xml_integration_point: false,
    })
}

/// A URL as browsers read it: without ASCII whitespace and control characters, lowercased
/// up to its scheme's colon.
fn scheme(url: &str) -> Option<String> {
    let squeezed: String = url.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control()).collect();
    let colon = squeezed.find(':')?;
    let s = &squeezed[..colon];
    // `a/b:c` and `#x:y` are relative.
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

/// Some(new value), or None to remove the attribute.
fn check_url(value: &str, kind: Kind, remote_images: bool, inline: &mut dyn FnMut(&str) -> Option<String>) -> Option<String> {
    let v = value.trim();
    match (scheme(v).as_deref(), kind) {
        (None, Kind::Link) => Some(v.to_owned()),
        // A relative picture would load from the file's folder: nothing there to show.
        (None, Kind::Picture) => None,
        (Some("http" | "https" | "mailto" | "tel"), Kind::Link) => Some(v.to_owned()),
        (Some("http" | "https"), Kind::Picture) => remote_images.then(|| v.to_owned()),
        (Some("cid"), Kind::Picture) => inline(v),
        (Some("data"), Kind::Picture) => {
            let lower = v.to_ascii_lowercase();
            ["data:image/png", "data:image/jpeg", "data:image/jpg", "data:image/gif", "data:image/webp", "data:image/bmp"].iter().any(|p| lower.starts_with(p)).then(|| v.to_owned())
        }
        _ => None,
    }
}

fn clean_attrs(h: &Handle, tag: &str, remote_images: bool, inline: &mut dyn FnMut(&str) -> Option<String>) {
    let NodeData::Element { attrs, .. } = &h.data else { return };
    let mut attrs = attrs.borrow_mut();
    let old = std::mem::take(&mut *attrs);
    for mut a in old {
        let n = a.name.local.as_ref().to_ascii_lowercase();
        if n.starts_with("on") || DROP_ATTRS.contains(&n.as_str()) {
            continue;
        }
        let kind = match n.as_str() {
            "href" if matches!(tag, "a" | "area") => Some(Kind::Link),
            "href" | "src" | "background" | "poster" | "cite" | "usemap" | "xlink:href" => Some(Kind::Picture),
            _ => None,
        };
        // `xlink:href` arrives as local name `href` in the xlink namespace.
        let kind = if n == "href" && !a.name.ns.is_empty() && !matches!(tag, "a") { Some(Kind::Picture) } else { kind };
        match kind {
            Some(Kind::Picture) if n == "usemap" || n == "cite" => {
                // Map names and quote sources load nothing; keep in-page ones only.
                if a.value.starts_with('#') {
                    attrs.push(a);
                }
            }
            Some(k) => {
                if let Some(v) = check_url(&a.value, k, remote_images, inline) {
                    a.value = StrTendril::from(v);
                    attrs.push(a);
                }
            }
            None if n == "style" && a.value.contains("cid:") => {
                a.value = StrTendril::from(inline_css(&a.value, inline));
                attrs.push(a);
            }
            None => attrs.push(a),
        }
    }
    if matches!(tag, "a" | "area") && attrs.iter().any(|a| a.name.local.as_ref() == "href") {
        attrs.retain(|a| a.name.local.as_ref() != "rel");
        attrs.push(Attribute { name: QualName::new(None, Namespace::from(""), LocalName::from("rel")), value: StrTendril::from("noopener noreferrer") });
    }
}

/// CSS with its `url(cid:…)` replaced by the embedded pictures (or nothing). One pass; the
/// first 64 embedded pictures are inlined, later ones dropped (the CSP blocks `cid:` anyway).
fn inline_css(css: &str, inline: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let b = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let (mut i, mut copied, mut inlined) = (0, 0, 0);
    while i + 4 <= b.len() {
        if !b[i..i + 4].eq_ignore_ascii_case(b"url(") {
            i += 1;
            continue;
        }
        let start = i + 4;
        let end = css[start..].find(')').map_or(b.len(), |e| start + e);
        let url = css[start..end].trim().trim_matches(|c| c == '"' || c == '\'');
        if url.len() >= 4 && url[..4].eq_ignore_ascii_case("cid:") {
            out.push_str(&css[copied..start]);
            if inlined < 64 {
                inlined += 1;
                out.push_str(&inline(url).map(|d| format!("'{d}'")).unwrap_or_default());
            }
            copied = end;
        }
        i = end.max(i + 1);
    }
    out.push_str(&css[copied..]);
    out
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

    #[test]
    fn active_content_is_removed() {
        let html = r##"<html><head><meta http-equiv="refresh" content="0;url=https://evil.example"><base href="https://evil.example/"><link rel=stylesheet href="https://evil.example/a.css"><script>alert(1)</script><title>T</title></head>
            <body onload="steal()"><p onclick="x()" style="color:red">Hi <a href="javascript:alert(1)">bad</a> <a href=" java	script:alert(1)">tabbed</a> <a href="https://ok.example/" ping="https://t.example">good</a> <a href="#top">top</a> <a href="mailto:a@b.c">mail</a></p>
            <iframe src="https://evil.example"></iframe><object data="x.swf"></object><embed src="x"><form action="https://evil.example/post"><input name=q><button>Go</button><p>In a form</p></form>
            <svg><script>alert(2)</script><a xlink:href="javascript:alert(3)"><text>svg</text></a></svg><noscript><img src="https://t.example/ns.gif"></noscript><video src="https://e.x/v.mp4"></video></body></html>"##;
        let out = run(html, true);
        for bad in ["<script", "alert(", "javascript", "onload", "onclick", "refresh", "<base", "<link", "evil.example", "<iframe", "<object", "<embed", "<input", "<button", "<form", "ping=", "<noscript", "ns.gif", "<video", "steal"] {
            assert!(!out.to_ascii_lowercase().contains(bad), "{bad} survived:\n{out}");
        }
        for good in ["Hi ", "https://ok.example/", "#top", "mailto:a@b.c", "In a form", "color:red", "<title>T</title>", "noopener noreferrer"] {
            assert!(out.contains(good), "{good} lost:\n{out}");
        }
    }

    #[test]
    fn a_strict_policy_comes_first() {
        let out = run("<p>x</p>", false);
        let head = out.find("<head>").expect("a head") + "<head>".len();
        let rest = &out[head..];
        assert!(rest.starts_with(r#"<meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src 'none'; form-action 'none'; base-uri 'none'"><meta name="referrer" content="no-referrer">"#), "{out}");
        assert!(out.starts_with("<!DOCTYPE html>"));
        assert!(run("<p>x</p>", true).contains("img-src data: https: http:;"));
    }

    #[test]
    fn pictures_follow_the_image_setting_and_embedded_ones_inline() {
        let html = r#"<img src="https://img.example/a.png" alt="A"><img src="cid:logo@x"><img src="cid:missing@x" alt="M"><table><tr><td background="https://img.example/b.png">c</td></tr></table>
            <div style="background:url('cid:logo@x') no-repeat">s</div><style>.h { background: url(cid:logo@x) } .r { background: url(https://img.example/c.png) }</style>
            <img src="data:image/png;base64,AAAA"><img src="data:text/html;base64,PHNjcmlwdD4="><img src="file:///etc/passwd"><img src="relative.png"><img srcset="https://img.example/2x.png 2x">"#;
        let blocked = run(html, false);
        assert!(!blocked.contains("https://img.example/a.png") && !blocked.contains("https://img.example/b.png"), "{blocked}");
        assert!(blocked.contains(r#"alt="A""#), "alt text stays");
        assert_eq!(blocked.matches("data:image/png;base64,iVBORw0KGgo=").count(), 3, "img, style attribute and sheet: {blocked}");
        assert!(blocked.contains("data:image/png;base64,AAAA"));
        for bad in ["data:text/html", "file:", "relative.png", "srcset", "cid:"] {
            assert!(!blocked.contains(bad), "{bad} survived:\n{blocked}");
        }
        let allowed = run(html, true);
        assert!(allowed.contains(r#"src="https://img.example/a.png""#) && allowed.contains(r#"background="https://img.example/b.png""#));
    }

    #[test]
    fn deep_documents_dont_overflow() {
        let html = format!("{}x{}", "<div><form>".repeat(50_000), "</form></div>".repeat(50_000));
        let t = std::thread::Builder::new().stack_size(2 << 20).spawn(move || export(&html, false, &mut |_| None).len()).unwrap();
        assert!(t.join().unwrap() > 0);
    }
}
