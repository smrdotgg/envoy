//! HTML email → a small layout tree the UI draws natively.
//!
//! Email HTML is table soup from the 2000s: layout tables nested dozens deep, styles inline and
//! in `<style>` sheets, spacer cells, hidden preheaders and tracking pixels. This keeps what a
//! reader sees — paragraphs of styled text, rows of cells on a shared column grid, coloured
//! boxes with borders and background pictures, buttons, fluid columns that wrap, images — and
//! drops the rest, noting what it couldn't draw ([`Fidelity`]). The result is plain data
//! (`Send`), built off the UI thread and cheap to turn into elements.
//!
//! Styles come from a bounded cascade ([`css`]): the email's sheets and `style=""` attributes
//! over a small built-in sheet, with `@media` evaluated for a desktop pane in light mode.
//!
//! It runs once per message off the UI thread and is cached, so it's built for size
//! (`[profile.dist.package]`); envoy-core re-exports it as `envoy_core::html`.

mod css;
mod fidelity;
mod table;

use std::collections::HashMap;
use std::rc::Rc;

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub use fidelity::{Fidelity, Issue, Verdict};

use css::{Elem, NONE, Sheet, Styles, get as style};

/// sRGB colour with alpha, 0–255.
pub type Rgba = [u8; 4];

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum VAlign {
    Top,
    #[default]
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Len {
    Px(f32),
    Pct(f32),
}

/// The generic family a `font-family` list falls into.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Family {
    #[default]
    Sans,
    Serif,
    Mono,
}

/// CSS `line-height`: a multiple of the font size, or points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineHeight {
    Mult(f32),
    Px(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    /// CSS weight, 100–900.
    pub weight: u16,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub mono: bool,
    pub size: Option<f32>,
    pub color: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub href: Option<String>,
}

impl Default for Run {
    fn default() -> Run {
        Run { text: String::new(), weight: 400, italic: false, underline: false, strike: false, mono: false, size: None, color: None, bg: None, href: None }
    }
}

impl Run {
    pub fn bold(&self) -> bool {
        self.weight >= 600
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Para {
    pub runs: Vec<Run>,
    pub align: Align,
    pub family: Family,
    pub line_height: Option<LineHeight>,
    /// Letter spacing in points.
    pub tracking: f32,
}

impl Para {
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// Border widths per side (top, right, bottom, left) in one colour.
#[derive(Clone, Debug, PartialEq)]
pub struct Border {
    pub widths: [f32; 4],
    pub color: Rgba,
    /// `dashed` or `dotted`.
    pub dashed: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct BoxStyle {
    /// How content lines up inside (text, and narrower boxes and images).
    pub align: Align,
    pub bg: Option<Rgba>,
    /// A background picture (`http(s)://` or `cid:`), drawn under the content, cropped to fill.
    pub bg_image: Option<String>,
    /// top, right, bottom, left
    pub pad: [f32; 4],
    /// Outside the box: top, right, bottom, left.
    pub margin: [f32; 4],
    pub width: Option<Len>,
    pub max_width: Option<f32>,
    pub min_width: f32,
    pub radius: f32,
    pub border: Option<Border>,
    pub href: Option<String>,
    /// Where content sits vertically in a table cell (`valign`; middle by default, as in HTML).
    pub valign: VAlign,
    /// Size to content instead of sharing the row (`nowrap`, small fixed cells, buttons).
    pub tight: bool,
}

impl BoxStyle {
    fn is_plain(&self) -> bool {
        self.bg.is_none()
            && self.bg_image.is_none()
            && self.pad == [0.0; 4]
            && self.margin == [0.0; 4]
            && self.width.is_none()
            && self.max_width.is_none()
            && self.border.is_none()
            && self.href.is_none()
            && self.radius == 0.0
    }

    fn has_visuals(&self) -> bool {
        self.bg.is_some() || self.bg_image.is_some() || self.border.is_some() || self.href.is_some() || self.radius > 0.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Img {
    pub src: String,
    pub alt: String,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub href: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Para(Para),
    Box { style: BoxStyle, children: Vec<Node> },
    /// A table row. Each cell's width is its share of the table (`Len::Pct`), from the column
    /// model every row of the table shares, or a fixed width; `gap` is the cell spacing.
    Row { cells: Vec<(BoxStyle, Vec<Node>)>, gap: f32 },
    /// Inline-block columns: side by side while they fit, wrapping onto new lines when the pane
    /// is narrow (`width`, `min_width`, `max_width` per column).
    Columns { cells: Vec<(BoxStyle, Vec<Node>)>, align: Align },
    Image(Img),
    Rule,
    /// Vertical space; neighbours collapse to the larger (as margins do).
    Space(f32),
    /// An empty line (`<div><br></div>`, `<p>&nbsp;</p>`) of this height; these add up.
    Blank(f32),
    List { ordered: bool, marker: bool, items: Vec<Vec<Node>> },
    Quote(Vec<Node>),
    Pre(String),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Doc {
    pub nodes: Vec<Node>,
    /// Every image source, in order: `http(s)://…` and `cid:…`, background pictures included.
    pub images: Vec<String>,
    /// The email's own page background, if it set one.
    pub background: Option<Rgba>,
    /// What couldn't be drawn as designed.
    pub fidelity: Fidelity,
}

/// Default text size, in points, for mail that sets none.
const BASE: f32 = 14.0;
/// Deepest the builder descends.
const MAX_DEPTH: usize = 200;

/// Stack the builder may use below where parsing started. It recurses once per element (more
/// frames per nested table), so a level count alone can't bound it: the engine's threads have
/// 2 MiB stacks, and an unoptimized build's frames are several times a release build's.
const STACK_BUDGET: usize = 1 << 20;

/// Where the stack is now, to compare with where parsing started.
fn stack_here() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum Transform {
    #[default]
    None,
    Upper,
    Lower,
    Capitalize,
}

/// What children inherit.
#[derive(Clone)]
struct Ctx {
    weight: u16,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
    family: Family,
    size: Option<f32>,
    color: Option<Rgba>,
    /// An inline element's background (`<code>`, `<mark>`, highlighted spans).
    bg: Option<Rgba>,
    href: Option<String>,
    align: Align,
    /// `align` came from an `align=` attribute or `<center>` (`-webkit-center` in browsers):
    /// it reaches descendants but stops at a nested table, unlike CSS `text-align`.
    legacy: bool,
    pre: bool,
    line_height: Option<LineHeight>,
    tracking: f32,
    transform: Transform,
    /// Roughly how wide, in points, the content here can be: turns pixel widths into shares.
    avail: f32,
}

impl Default for Ctx {
    fn default() -> Ctx {
        Ctx {
            weight: 400,
            italic: false,
            underline: false,
            strike: false,
            mono: false,
            family: Family::Sans,
            size: None,
            color: None,
            bg: None,
            href: None,
            align: Align::Left,
            legacy: false,
            pre: false,
            line_height: None,
            tracking: 0.0,
            transform: Transform::None,
            avail: css::VIEWPORT,
        }
    }
}

impl Ctx {
    fn size(&self) -> f32 {
        self.size.unwrap_or(BASE)
    }
}

/// The layout tree of a message's HTML. Never panics: mail is attacker-controlled and this
/// runs while the store is locked, so a bug that panics yields an empty tree (the reader falls
/// back to the message's text) instead of unwinding into the caller.
pub fn parse(html: &str) -> Doc {
    guarded(|| parse_unguarded(html))
}

/// Attributes kept per tag. html5ever checks each attribute against every earlier one of its
/// tag, so 100,000 of them took minutes; real tags have a few dozen at most.
const MAX_ATTRS: usize = 512;

/// Elements open at once (those closed by an explicit end tag). html5ever scans its whole
/// stack of open elements for many tags and every run of text, so depth × length is its cost;
/// browsers cap parser nesting too (Blink at 512).
const MAX_OPEN: usize = 256;

/// Elements whose end tag is optional or that have none: not counted as open.
const IMPLIED: &[&str] = &[
    "p", "li", "dt", "dd", "tr", "td", "th", "tbody", "thead", "tfoot", "colgroup", "option", "optgroup", "rb", "rt", "rp", "rtc", "html", "head", "body", "br", "img", "hr",
    "input", "meta", "link", "area", "base", "col", "embed", "param", "source", "track", "wbr", "keygen", "frame", "image", "isindex",
];

/// Elements whose content is text up to their end tag.
const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "plaintext"];

/// `html` made safe for html5ever's quadratic corners, in one linear pass: every tag cut
/// after [`MAX_ATTRS`] attributes, and tags past [`MAX_OPEN`] open elements dropped with their
/// end tags (their text stays). Borrowed when nothing changes, as for any real mail.
fn prefilter(html: &str) -> std::borrow::Cow<'_, str> {
    let b = html.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut open = 0usize;
    let mut dropped: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    // Replaces html[from..to] with `with` in the output.
    let splice = |out: &mut Option<String>, copied: &mut usize, from: usize, to: usize, with: &str| {
        let o = out.get_or_insert_with(|| String::with_capacity(html.len()));
        o.push_str(&html[*copied..from]);
        o.push_str(with);
        *copied = to;
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"<!--") {
            i = find(b, i + 4, b"-->").map_or(b.len(), |e| e + 3);
            continue;
        }
        let end_tag = b.get(i + 1) == Some(&b'/');
        let name_at = if end_tag { i + 2 } else { i + 1 };
        if !b.get(name_at).is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        let mut n = name_at;
        while n < b.len() && !b[n].is_ascii_whitespace() && b[n] != b'>' && b[n] != b'/' {
            n += 1;
        }
        let name = html[name_at..n].to_ascii_lowercase();
        let (close, cut) = tag_end(b, n);
        let next = (close + 1).min(b.len());
        if end_tag {
            if let Some(d) = dropped.get_mut(&name).filter(|d| **d > 0) {
                *d -= 1;
                splice(&mut out, &mut copied, i, next, "");
            } else if !IMPLIED.contains(&name.as_str()) {
                open = open.saturating_sub(1);
            }
            i = next;
            continue;
        }
        let counted = !IMPLIED.contains(&name.as_str()) && !(close > 0 && b.get(close - 1) == Some(&b'/'));
        if counted && open >= MAX_OPEN {
            *dropped.entry(name.clone()).or_default() += 1;
            splice(&mut out, &mut copied, i, next, "");
        } else {
            if counted {
                open += 1;
            }
            if let Some(at) = cut {
                splice(&mut out, &mut copied, at, next, ">");
            }
        }
        i = next;
        if RAW.contains(&name.as_str()) {
            // Its text, unread, up to `</name`.
            i = find_raw_end(b, i, &name);
        }
    }
    match out {
        Some(mut o) => {
            o.push_str(&html[copied..]);
            std::borrow::Cow::Owned(o)
        }
        None => std::borrow::Cow::Borrowed(html),
    }
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    (from..b.len().saturating_sub(needle.len() - 1)).find(|&k| b[k..].starts_with(needle))
}

/// Where the text of raw element `name` starting at `from` ends (its `</name`).
fn find_raw_end(b: &[u8], from: usize, name: &str) -> usize {
    let mut k = from;
    while let Some(lt) = find(b, k, b"</") {
        if b.len() - lt - 2 >= name.len() && b[lt + 2..lt + 2 + name.len()].eq_ignore_ascii_case(name.as_bytes()) {
            return lt;
        }
        k = lt + 2;
    }
    b.len()
}

/// From inside a tag, its closing `>` and, past [`MAX_ATTRS`] attributes, where to cut it.
/// Quotes count only where HTML has them, right after `=`.
fn tag_end(b: &[u8], from: usize) -> (usize, Option<usize>) {
    let (mut j, mut attrs, mut quote, mut space, mut eq) = (from, 0usize, 0u8, from > 0 && b[from - 1].is_ascii_whitespace(), false);
    let mut cut = None;
    while j < b.len() {
        let c = b[j];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'>' {
            return (j, cut);
        } else if c.is_ascii_whitespace() {
            space = true;
        } else {
            if eq && (c == b'"' || c == b'\'') {
                quote = c;
            } else if space && !eq && c != b'=' && c != b'/' && cut.is_none() {
                attrs += 1;
                if attrs > MAX_ATTRS {
                    cut = Some(j);
                }
            }
            eq = c == b'=';
            space = false;
        }
        j += 1;
    }
    (b.len(), cut)
}

/// Runs `f`, turning a panic into an empty [`Doc`].
fn guarded(f: impl FnOnce() -> Doc + std::panic::UnwindSafe) -> Doc {
    std::panic::catch_unwind(f).unwrap_or_else(|_| {
        log::warn!("html: the renderer panicked on a message; showing its text instead");
        Doc::default()
    })
}

fn parse_unguarded(html: &str) -> Doc {
    let html = prefilter(html);
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).from_utf8().read_from(&mut html.as_bytes());
    let Ok(dom) = dom else { return Doc::default() };
    let mut b = Builder::new(&dom.document);
    b.stack_top = stack_here();
    let mut out = Vec::new();
    let mut para = Para::default();
    b.children(&dom.document, &Ctx::default(), &mut out, &mut para);
    flush(&mut out, &mut para);
    let mut nodes = tidy(out);
    // Blank lines at the very start and end of a message are just whitespace.
    while matches!(nodes.last(), Some(Node::Blank(_) | Node::Space(_))) {
        nodes.pop();
    }
    let lead = nodes.iter().take_while(|n| matches!(n, Node::Blank(_) | Node::Space(_))).count();
    nodes.drain(..lead);
    b.doc.nodes = nodes;
    let stats = &b.sheet.stats;
    b.doc.fidelity.note_n(Issue::BadSelector, stats.bad_selectors);
    b.doc.fidelity.note_n(Issue::Generated, stats.generated);
    if stats.exhausted || stats.dropped_rules > 0 {
        b.doc.fidelity.note(Issue::CssBudget);
    }
    log::debug!("html: fidelity score {} ({:?}): {:?}", b.doc.fidelity.score(), b.doc.fidelity.verdict(), b.doc.fidelity.issues);
    b.doc
}

struct Builder {
    doc: Doc,
    depth: usize,
    sheet: Sheet,
    els: Vec<Elem>,
    /// Element node address → index in `els`.
    index: HashMap<usize, u32>,
    styles: Vec<Option<Rc<Styles>>>,
    /// Counts line breaks and `&nbsp;` met while no text was written: an otherwise empty
    /// block that saw one is a blank line.
    blank_hint: u32,
    too_deep: bool,
    stack_top: usize,
}

fn key(h: &Handle) -> usize {
    Rc::as_ptr(h) as *const () as usize
}

fn attr(h: &Handle, name: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &h.data {
        attrs.borrow().iter().find(|a| a.name.local.as_ref().eq_ignore_ascii_case(name)).map(|a| a.value.to_string())
    } else {
        None
    }
}

fn tag(h: &Handle) -> Option<String> {
    if let NodeData::Element { name, .. } = &h.data { Some(name.local.as_ref().to_ascii_lowercase()) } else { None }
}

impl Builder {
    /// Indexes the document's elements for selectors and reads its `<style>` sheets.
    fn new(root: &Handle) -> Builder {
        let mut b = Builder { doc: Doc::default(), depth: 0, sheet: Sheet::new(), els: Vec::new(), index: HashMap::new(), styles: Vec::new(), blank_hint: 0, too_deep: false, stack_top: stack_here() };
        let mut sheets: Vec<(String, Option<String>)> = Vec::new();
        b.index_children(root, NONE, [0; 8], &mut sheets, 0);
        for (css, media) in sheets {
            b.sheet.add(&css, media.as_deref());
        }
        b.styles = vec![None; b.els.len()];
        b
    }

    fn index_children(&mut self, h: &Handle, parent: u32, ancestors: css::Bloom, sheets: &mut Vec<(String, Option<String>)>, depth: usize) {
        let mut prev = NONE;
        let mut index = 0;
        let kids = h.children.borrow();
        let last_el = kids.iter().rposition(|c| matches!(c.data, NodeData::Element { .. }));
        for (k, c) in kids.iter().enumerate() {
            let NodeData::Element { name, attrs, .. } = &c.data else { continue };
            let t = name.local.as_ref().to_ascii_lowercase();
            if t == "style" {
                let mut css = String::new();
                for t in c.children.borrow().iter() {
                    if let NodeData::Text { contents } = &t.data {
                        css.push_str(&contents.borrow());
                    }
                }
                sheets.push((css, attr(c, "media")));
            }
            index += 1;
            let attrs: Vec<(String, String)> = attrs.borrow().iter().map(|a| (a.name.local.as_ref().to_ascii_lowercase(), a.value.to_string())).collect();
            let id = attrs.iter().find(|(k, _)| k == "id").map(|(_, v)| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
            let mut classes: Vec<String> = attrs.iter().find(|(k, _)| k == "class").map(|(_, v)| v.split_whitespace().map(|c| c.to_ascii_lowercase()).collect()).unwrap_or_default();
            classes.sort_unstable();
            classes.dedup();
            let i = self.els.len() as u32;
            let mut inner = ancestors;
            for cl in &classes {
                css::bloom_add(&mut inner, '.', cl);
            }
            if let Some(id) = &id {
                css::bloom_add(&mut inner, '#', id);
            }
            self.els.push(Elem { tag: t, id, classes, attrs, parent, prev, index, last: Some(k) == last_el, ancestors });
            self.index.insert(key(c), i);
            prev = i;
            if depth < MAX_DEPTH + 50 {
                self.index_children(c, i, inner, sheets, depth + 1);
            }
        }
    }

    /// The element's declarations, cascaded (memoized).
    fn styles(&mut self, h: &Handle) -> Rc<Styles> {
        let Some(&i) = self.index.get(&key(h)) else { return Rc::new(Vec::new()) };
        if let Some(s) = &self.styles[i as usize] {
            return s.clone();
        }
        let inline = self.els[i as usize].attrs.iter().find(|(k, _)| k == "style").map(|(_, v)| v.clone());
        let s = Rc::new(self.sheet.cascade(&self.els, i, inline.as_deref()));
        self.styles[i as usize] = Some(s.clone());
        s
    }
}

fn zero(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim_end_matches("px").trim_end_matches("pt").trim_end_matches("em").trim().parse::<f32>().is_ok_and(|n| n <= 0.0))
}

/// Element children that make a block more than text: these survive `font-size: 0`. Looks a
/// few levels down and at a few hundred nodes at most; past that, assume there's more than
/// text (and show it).
fn has_blockish(h: &Handle) -> bool {
    fn walk(h: &Handle, depth: u32, budget: &mut u32) -> bool {
        for c in h.children.borrow().iter() {
            if *budget == 0 || depth > 6 {
                return true;
            }
            *budget -= 1;
            if let Some(t) = tag(c)
                && (t == "img" || t == "table" || BLOCKS.contains(&t.as_str()) || walk(c, depth + 1, budget))
            {
                return true;
            }
        }
        false
    }
    walk(h, 0, &mut 256)
}

fn hidden(h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)]) -> bool {
    if attr(h, "hidden").is_some() || attr(h, "aria-hidden").is_some_and(|v| v == "true") && t == "div" && !has_blockish(h) {
        return true;
    }
    style(st, "display") == Some("none")
        || style(st, "visibility") == Some("hidden")
        || zero(style(st, "opacity"))
        || ((zero(style(st, "max-height")) || zero(style(st, "height"))) && style(st, "overflow").is_some_and(|o| o == "hidden"))
        || (zero(style(st, "font-size")) && zero(style(st, "line-height")) && !matches!(t, "td" | "th" | "tr" | "table") && !has_blockish(h))
}

pub fn parse_color(v: &str) -> Option<Rgba> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        let p = |s: &str| u8::from_str_radix(s, 16).ok();
        return match hex.len() {
            3 | 4 => {
                let c: Vec<char> = hex.chars().collect();
                let d = |i: usize| p(&c[i].to_string().repeat(2));
                Some([d(0)?, d(1)?, d(2)?, if c.len() == 4 { d(3)? } else { 255 }])
            }
            6 => Some([p(&hex[0..2])?, p(&hex[2..4])?, p(&hex[4..6])?, 255]),
            8 => Some([p(&hex[0..2])?, p(&hex[2..4])?, p(&hex[4..6])?, p(&hex[6..8])?]),
            _ => None,
        };
    }
    if let Some(inner) = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb(")) {
        let parts: Vec<f32> = inner.trim_end_matches(')').split([',', ' ', '/']).filter(|p| !p.is_empty()).filter_map(|p| p.trim().trim_end_matches('%').parse().ok()).collect();
        if parts.len() >= 3 {
            let a = parts.get(3).map_or(255, |a| if *a <= 1.0 { (a * 255.0) as u8 } else { *a as u8 });
            return Some([parts[0] as u8, parts[1] as u8, parts[2] as u8, a]);
        }
        return None;
    }
    Some(match v {
        "white" => [255, 255, 255, 255],
        "black" | "windowtext" => [0, 0, 0, 255],
        "red" => [255, 0, 0, 255],
        "green" => [0, 128, 0, 255],
        "blue" => [0, 0, 255, 255],
        "gray" | "grey" => [128, 128, 128, 255],
        "darkgray" | "darkgrey" => [169, 169, 169, 255],
        "lightgray" | "lightgrey" => [211, 211, 211, 255],
        "silver" => [192, 192, 192, 255],
        "navy" => [0, 0, 128, 255],
        "orange" => [255, 165, 0, 255],
        "purple" => [128, 0, 128, 255],
        "maroon" => [128, 0, 0, 255],
        "teal" => [0, 128, 128, 255],
        "olive" => [128, 128, 0, 255],
        "yellow" => [255, 255, 0, 255],
        "gold" => [255, 215, 0, 255],
        "transparent" => [0, 0, 0, 0],
        _ => return None,
    })
}

/// A CSS length in points (`16px`, `12pt`, `1.2em`, `80%` of `base`), within ±100,000.
fn length(v: &str, base: f32) -> Option<f32> {
    length_raw(v, base).map(|n| n.clamp(-100_000.0, 100_000.0)).filter(|n| n.is_finite())
}

fn length_raw(v: &str, base: f32) -> Option<f32> {
    let v = v.trim();
    let num = |s: &str| s.trim().parse::<f32>().ok().filter(|n| n.is_finite());
    if let Some(n) = v.strip_suffix("px") {
        num(n)
    } else if let Some(n) = v.strip_suffix("pt") {
        num(n).map(|n| n * 4.0 / 3.0)
    } else if let Some(n) = v.strip_suffix("rem") {
        num(n).map(|n| n * 16.0)
    } else if let Some(n) = v.strip_suffix("em") {
        num(n).map(|n| n * base)
    } else if let Some(n) = v.strip_suffix("ex") {
        num(n).map(|n| n * base * 0.5)
    } else if let Some(n) = v.strip_suffix("cm") {
        num(n).map(|n| n * 96.0 / 2.54)
    } else if let Some(n) = v.strip_suffix("mm") {
        num(n).map(|n| n * 96.0 / 25.4)
    } else if let Some(n) = v.strip_suffix("in") {
        num(n).map(|n| n * 96.0)
    } else if let Some(n) = v.strip_suffix('%') {
        num(n).map(|n| n / 100.0 * base)
    } else {
        num(v)
    }
}

/// A width: `600`, `600px`, `80%` (`auto` and the rest: none).
fn len_attr(v: &str) -> Option<Len> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        p.trim().parse().ok().filter(|n: &f32| *n > 0.0 && n.is_finite()).map(|n: f32| Len::Pct(n.min(1_000.0)))
    } else if v.ends_with("em") || v.ends_with("pt") {
        length(v, BASE).filter(|n| *n > 0.0).map(Len::Px)
    } else {
        v.trim_end_matches("px").trim().parse().ok().filter(|n: &f32| *n > 0.0 && n.is_finite()).map(|n: f32| Len::Px(n.min(100_000.0)))
    }
}

fn align_of(v: &str) -> Option<Align> {
    match v.trim().to_ascii_lowercase().as_str() {
        "center" | "middle" | "-webkit-center" | "-moz-center" => Some(Align::Center),
        "right" | "end" | "-webkit-right" => Some(Align::Right),
        "left" | "start" | "justify" | "-webkit-left" => Some(Align::Left),
        _ => None,
    }
}

fn valign_of(v: &str) -> Option<VAlign> {
    match v.trim().to_ascii_lowercase().as_str() {
        "top" | "text-top" | "baseline" => Some(VAlign::Top),
        "middle" | "center" => Some(VAlign::Middle),
        "bottom" | "text-bottom" => Some(VAlign::Bottom),
        _ => None,
    }
}

/// The generic family of a `font-family` list: the first name we know decides.
fn family_of(list: &str) -> Option<Family> {
    for name in list.split(',') {
        let n = name.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase();
        if n.is_empty() {
            continue;
        }
        if n == "monospace" || n.contains("mono") || n.contains("courier") || n.contains("consolas") || n.contains("menlo") || n.contains("monaco") || n == "lucida console" {
            return Some(Family::Mono);
        }
        if n == "serif"
            || ["georgia", "times", "palatino", "garamond", "cambria", "baskerville", "didot", "bodoni", "book antiqua", "merriweather", "playfair", "lora", "charter", "iowan", "hoefler", "minion", "caslon", "constantia", "pt serif", "noto serif", "source serif", "dm serif", "libre baskerville", "crimson", "new york"]
                .iter()
                .any(|s| n.contains(s))
        {
            return Some(Family::Serif);
        }
        if n == "sans-serif"
            || n == "system-ui"
            || n.starts_with("-apple")
            || n.starts_with("blinkmac")
            || ["arial", "helvetica", "verdana", "tahoma", "trebuchet", "segoe", "roboto", "inter", "open sans", "lato", "calibri", "ubuntu", "nunito", "montserrat", "avenir", "futura", "gill sans", "lucida", "geneva", "poppins", "source sans", "noto sans", "work sans", "ui-sans"]
                .iter()
                .any(|s| n.contains(s))
        {
            return Some(Family::Sans);
        }
    }
    None
}

/// Applies an element's inherited text properties.
fn text_style(h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)], ctx: &mut Ctx) {
    if let Some(c) = style(st, "color").and_then(parse_color).or_else(|| (t == "font").then(|| attr(h, "color").as_deref().and_then(parse_color)).flatten()) {
        ctx.color = Some(c);
    }
    let parent_size = ctx.size();
    if let Some(v) = style(st, "font-size") {
        let s = match v {
            "smaller" => Some(parent_size * 0.833),
            "larger" => Some(parent_size * 1.2),
            "xx-small" => Some(9.0),
            "x-small" => Some(10.0),
            "small" => Some(13.0),
            "medium" => Some(BASE),
            "large" => Some(18.0),
            "x-large" => Some(24.0),
            "xx-large" => Some(32.0),
            _ => length(v, parent_size),
        };
        if let Some(s) = s {
            ctx.size = Some(s.clamp(9.0, 64.0));
        }
    } else if t == "font"
        && let Some(s) = attr(h, "size").and_then(|s| s.trim().parse::<i32>().ok())
    {
        ctx.size = Some([10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0][(s.clamp(1, 7) - 1) as usize]);
    }
    if let Some(w) = style(st, "font-weight") {
        ctx.weight = match w {
            "normal" => 400,
            "bold" => 700,
            "bolder" => (ctx.weight + 300).min(900),
            "lighter" => ctx.weight.saturating_sub(300).max(100),
            n => n.parse::<u16>().map(|n| n.clamp(100, 900)).unwrap_or(ctx.weight),
        };
    }
    if let Some(s) = style(st, "font-style") {
        ctx.italic = s == "italic" || s == "oblique";
    }
    if let Some(d) = style(st, "text-decoration-line").or_else(|| style(st, "text-decoration")) {
        if d.contains("none") {
            ctx.underline = false;
            ctx.strike = false;
        }
        if d.contains("underline") {
            ctx.underline = true;
        }
        if d.contains("line-through") {
            ctx.strike = true;
        }
    }
    if let Some(v) = style(st, "text-align")
        && let Some(a) = align_of(v)
    {
        ctx.align = a;
        ctx.legacy = v.starts_with("-webkit") || v.starts_with("-moz");
    }
    if let Some(f) = style(st, "font-family").or_else(|| (t == "font").then(|| attr(h, "face")).flatten().as_deref().map(|_| "")).and_then(|f| if f.is_empty() { None } else { family_of(f) }) {
        ctx.family = f;
        ctx.mono = f == Family::Mono;
    } else if t == "font"
        && let Some(f) = attr(h, "face").and_then(|f| family_of(&f))
    {
        ctx.family = f;
        ctx.mono = f == Family::Mono;
    }
    let size = ctx.size();
    if let Some(v) = style(st, "line-height") {
        ctx.line_height = match v {
            "normal" => None,
            _ => match v.parse::<f32>() {
                Ok(m) if m.is_finite() => Some(LineHeight::Mult(m)),
                _ => length(v, size).map(LineHeight::Px),
            },
        };
    }
    if let Some(v) = style(st, "letter-spacing") {
        ctx.tracking = if v == "normal" { 0.0 } else { length(v, size).unwrap_or(0.0).clamp(-2.0, 12.0) };
    }
    if let Some(v) = style(st, "text-transform") {
        ctx.transform = match v {
            "uppercase" => Transform::Upper,
            "lowercase" => Transform::Lower,
            "capitalize" => Transform::Capitalize,
            _ => Transform::None,
        };
    }
    if let Some(ws) = style(st, "white-space") {
        ctx.pre = matches!(ws, "pre" | "pre-wrap" | "pre-line" | "break-spaces");
    }
}

/// The URL in `url(…)`, if it's one we'd load (`http(s)` or `cid`).
fn css_url(v: &str) -> Option<String> {
    let at = v.find("url(")?;
    let rest = &v[at + 4..];
    let end = rest.find(')')?;
    let u = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'').trim();
    usable_src(u).then(|| u.to_owned())
}

fn usable_src(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("cid:")
}

/// A gradient's first colour stop, standing in for it.
fn gradient_color(v: &str) -> Option<Rgba> {
    let inner = &v[v.find('(')? + 1..];
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => break,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                let part = inner[start..i].trim();
                if let Some(c) = part.split_whitespace().next().and_then(parse_color).or_else(|| parse_color(part)) {
                    return Some(c);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let part = inner[start..].trim_end_matches(')').trim();
    part.split_whitespace().next().and_then(parse_color)
}

fn border_width(v: &str, size: f32) -> f32 {
    match v {
        "thin" => 1.0,
        "medium" => 3.0,
        "thick" => 5.0,
        _ => length(v, size).unwrap_or(0.0),
    }
}

const PADDING: [&str; 4] = ["padding-top", "padding-right", "padding-bottom", "padding-left"];
const MARGIN: [&str; 4] = ["margin-top", "margin-right", "margin-bottom", "margin-left"];
const BORDER_STYLE: [&str; 4] = ["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"];
const BORDER_WIDTH: [&str; 4] = ["border-top-width", "border-right-width", "border-bottom-width", "border-left-width"];
const BORDER_COLOR: [&str; 4] = ["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"];

fn box_style(h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)], ctx: &mut Ctx, cellpadding: f32) -> BoxStyle {
    let mut b = BoxStyle { align: ctx.align, ..BoxStyle::default() };
    // `<table align>` places the table; it doesn't align what's inside.
    let attr_align = if t == "table" || t == "img" { None } else { attr(h, "align").as_deref().and_then(align_of) };
    if let Some(v) = style(st, "text-align")
        && let Some(a) = align_of(v)
    {
        b.align = a;
        ctx.align = a;
        ctx.legacy = v.starts_with("-webkit") || v.starts_with("-moz");
    } else if let Some(a) = attr_align.or((t == "center" || t == "th").then_some(Align::Center)) {
        b.align = a;
        ctx.align = a;
        ctx.legacy = true;
    }
    let size = ctx.size();
    b.bg = style(st, "background-color").and_then(parse_color).or_else(|| attr(h, "bgcolor").as_deref().and_then(parse_color)).filter(|c| c[3] > 0);
    if let Some(img) = style(st, "background-image") {
        if img.contains("gradient(") {
            if b.bg.is_none() {
                b.bg = gradient_color(img);
            }
        } else {
            b.bg_image = css_url(img);
        }
    }
    if b.bg_image.is_none()
        && style(st, "background-image").is_none()
        && matches!(t, "table" | "td" | "th" | "body" | "tr")
        && let Some(u) = attr(h, "background").map(|u| u.trim().to_owned()).filter(|u| usable_src(u))
    {
        b.bg_image = Some(u);
    }
    for (i, k) in PADDING.iter().enumerate() {
        b.pad[i] = style(st, k).and_then(|v| length(v, size)).unwrap_or(cellpadding).clamp(0.0, 80.0);
    }
    for i in [1, 3] {
        b.margin[i] = style(st, MARGIN[i]).and_then(|v| length(v, size)).unwrap_or(0.0).clamp(0.0, 64.0);
    }
    b.width = style(st, "width").and_then(len_attr).or_else(|| if t == "td" || t == "th" || t == "table" || t == "img" || t == "col" { attr(h, "width").as_deref().and_then(len_attr) } else { None });
    b.max_width = style(st, "max-width").and_then(|v| match len_attr(v) {
        Some(Len::Px(w)) => Some(w),
        Some(Len::Pct(p)) if p < 100.0 => Some(p / 100.0 * ctx.avail),
        _ => None,
    });
    b.min_width = style(st, "min-width").and_then(len_attr).map_or(0.0, |l| match l {
        Len::Px(w) => w,
        Len::Pct(p) => p / 100.0 * ctx.avail,
    });
    b.radius = style(st, "border-radius")
        .or_else(|| style(st, "border-top-left-radius"))
        .and_then(|v| v.split_whitespace().next().and_then(|x| if x.ends_with('%') { Some(24.0) } else { length(x, size) }))
        .unwrap_or(0.0)
        .clamp(0.0, 32.0);
    let mut widths = [0.0f32; 4];
    let mut color = None;
    let mut dashed = false;
    for i in 0..4 {
        let st_ = style(st, BORDER_STYLE[i]).unwrap_or("none");
        if matches!(st_, "none" | "hidden") {
            continue;
        }
        let w = style(st, BORDER_WIDTH[i]).map_or(3.0, |v| border_width(v, size)).min(24.0);
        if w <= 0.0 {
            continue;
        }
        let c = match style(st, BORDER_COLOR[i]) {
            Some("currentcolor") | None => ctx.color.or(Some([0, 0, 0, 255])),
            Some(v) => parse_color(v),
        };
        if let Some(c) = c.filter(|c| c[3] > 0) {
            widths[i] = w;
            color = color.or(Some(c));
            dashed |= matches!(st_, "dashed" | "dotted");
        }
    }
    if let Some(c) = color {
        if b.bg.is_some_and(|bg| close(bg, c)) {
            // A border the colour of the box is padding (bulletproof buttons).
            for (p, w) in b.pad.iter_mut().zip(widths) {
                *p += w;
            }
        } else {
            b.border = Some(Border { widths: widths.map(|w| w.min(8.0)), color: c, dashed });
        }
    } else if t == "table" || t == "td" || t == "th" {
        // `border="1"` on a table: a thin rule round it and its cells.
        if attr(h, "border").and_then(|v| v.trim().parse::<f32>().ok()).is_some_and(|w| w >= 1.0) {
            b.border = Some(Border { widths: [1.0; 4], color: [160, 160, 160, 255], dashed: false });
        }
    }
    b.valign = attr(h, "valign").as_deref().and_then(valign_of).or_else(|| style(st, "vertical-align").and_then(valign_of)).unwrap_or_default();
    b.tight = attr(h, "nowrap").is_some() || style(st, "white-space") == Some("nowrap");
    b
}

fn close(a: Rgba, b: Rgba) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= 8)
}

/// Where a box with a width sits in its parent: `margin: 0 auto`, `<table align>`.
fn placement(h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)]) -> Option<Align> {
    // A float sits on its side whatever its margins say.
    match style(st, "float") {
        Some("left") => return Some(Align::Left),
        Some("right") => return Some(Align::Right),
        _ => {}
    }
    let (l, r) = (style(st, "margin-left"), style(st, "margin-right"));
    if l == Some("auto") && r == Some("auto") {
        return Some(Align::Center);
    }
    if l == Some("auto") {
        return Some(Align::Right);
    }
    if t == "table" { attr(h, "align").as_deref().and_then(align_of) } else { None }
}

/// Top and bottom margins, in points.
fn vmargins(st: &[(Rc<str>, Rc<str>)], size: f32) -> (f32, f32) {
    let m = |k: &str| style(st, k).and_then(|v| length(v, size)).unwrap_or(0.0).clamp(0.0, 64.0);
    (m("margin-top"), m("margin-bottom"))
}

fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{034f}' | '\u{feff}' | '\u{00ad}' | '\u{2060}' | '\u{180e}'))
        .map(|c| if c == '\u{a0}' { ' ' } else { c })
        .collect()
}

fn transformed(text: String, t: Transform, at_word_start: bool) -> String {
    match t {
        Transform::None => text,
        Transform::Upper => text.to_uppercase(),
        Transform::Lower => text.to_lowercase(),
        Transform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut start = at_word_start;
            for c in text.chars() {
                if start && c.is_alphabetic() {
                    out.extend(c.to_uppercase());
                    start = false;
                } else {
                    out.push(c);
                    start = c.is_whitespace() || (start && !c.is_alphanumeric());
                }
            }
            out
        }
    }
}

fn flush(out: &mut Vec<Node>, para: &mut Para) {
    let mut p = std::mem::take(para);
    // Trim whitespace at the ends.
    let lead = p.runs.iter().take_while(|r| r.text.trim().is_empty() && !r.text.contains('\n')).count();
    p.runs.drain(..lead);
    if let Some(first) = p.runs.first_mut() {
        first.text = first.text.trim_start_matches([' ', '\n']).to_owned();
    }
    while p.runs.last().is_some_and(|r| r.text.trim().is_empty()) {
        p.runs.pop();
    }
    if let Some(last) = p.runs.last_mut() {
        last.text = last.text.trim_end().to_owned();
    }
    if p.runs.iter().any(|r| !r.text.trim().is_empty()) {
        out.push(Node::Para(p));
    }
}

const BLOCKS: &[&str] = &[
    "div", "p", "h1", "h2", "h3", "h4", "h5", "h6", "table", "tbody", "thead", "tfoot", "tr", "td", "th", "center", "blockquote", "ul", "ol", "li", "hr",
    "pre", "section", "article", "header", "footer", "main", "nav", "aside", "form", "body", "html", "figure", "figcaption", "address", "dl", "dt", "dd", "caption",
];
const SKIP: &[&str] = &["head", "style", "script", "title", "meta", "link", "noscript", "template", "svg", "map", "object", "iframe", "video", "audio", "input", "select", "textarea", "embed", "canvas", "base"];

impl Builder {
    fn children(&mut self, h: &Handle, ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let kids: Vec<Handle> = h.children.borrow().iter().cloned().collect();
        let mut i = 0;
        while i < kids.len() {
            if self.is_column(&kids[i]) {
                // A run of inline-block columns, with only whitespace and comments between.
                let mut run = vec![kids[i].clone()];
                let mut last = i;
                let mut j = i + 1;
                while j < kids.len() {
                    match &kids[j].data {
                        NodeData::Text { contents } if contents.borrow().trim().is_empty() => {}
                        NodeData::Comment { .. } => {}
                        NodeData::Element { .. } if self.is_column(&kids[j]) => {
                            run.push(kids[j].clone());
                            last = j;
                        }
                        _ => break,
                    }
                    j += 1;
                }
                if run.len() > 1 {
                    flush(out, para);
                    // `dir="rtl"` swaps columns on wide screens and keeps their order stacked.
                    let rtl = attr(h, "dir").is_some_and(|d| d.eq_ignore_ascii_case("rtl"));
                    self.columns(&run, ctx, rtl, out);
                    i = last + 1;
                    continue;
                }
            }
            self.node(&kids[i], ctx, out, para);
            i += 1;
        }
    }

    /// An `inline-block` (or floated, sized) block: one of a row of fluid columns.
    fn is_column(&mut self, h: &Handle) -> bool {
        let Some(t) = tag(h) else { return false };
        if !matches!(t.as_str(), "div" | "table" | "section" | "article" | "aside") {
            return false;
        }
        let st = self.styles(h);
        if hidden(h, &t, &st) {
            return false;
        }
        match style(&st, "display") {
            Some("inline-block") => true,
            _ => style(&st, "float").is_some_and(|f| f == "left" || f == "right") && (style(&st, "width").is_some() || style(&st, "max-width").is_some() || attr(h, "width").is_some()),
        }
    }

    fn columns(&mut self, run: &[Handle], ctx: &Ctx, rtl: bool, out: &mut Vec<Node>) {
        let mut cells = Vec::new();
        for c in run {
            let t = tag(c).unwrap_or_default();
            let st = self.styles(c);
            self.classify(c, &t, &st, true);
            let mut cctx = ctx.clone();
            text_style(c, &t, &st, &mut cctx);
            let mut bs = box_style(c, &t, &st, &mut cctx, 0.0);
            cctx.avail = match bs.width {
                Some(Len::Px(w)) => w,
                Some(Len::Pct(p)) => ctx.avail * p / 100.0,
                None => ctx.avail,
            }
            .min(bs.max_width.unwrap_or(f32::MAX))
                - bs.pad[1]
                - bs.pad[3];
            if bs.valign == VAlign::Middle && style(&st, "vertical-align").is_none() {
                // Inline blocks sit on the baseline: their tops line up, near enough.
                bs.valign = VAlign::Top;
            }
            let mut inner = Vec::new();
            if t == "table" {
                self.table(c, &st, &mut cctx, &mut inner);
                if let [Node::Box { style, children }] = inner.as_mut_slice() {
                    // The table is the column: keep its look, the column's size.
                    let kids = std::mem::take(children);
                    let s = std::mem::take(style);
                    bs = BoxStyle { width: bs.width, max_width: bs.max_width, min_width: bs.min_width, valign: bs.valign, ..s };
                    inner = kids;
                }
            } else {
                let mut p = Para::default();
                self.children(c, &cctx, &mut inner, &mut p);
                flush(&mut inner, &mut p);
            }
            cells.push((bs, tidy(inner)));
        }
        if rtl {
            cells.reverse();
        }
        if cells.iter().any(|(s, kids)| !kids.is_empty() || s.bg.is_some()) {
            out.push(Node::Columns { cells, align: ctx.align });
        }
    }

    fn node(&mut self, h: &Handle, ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        match &h.data {
            NodeData::Text { contents } => {
                let text = contents.borrow();
                self.push_text(para, ctx, &text);
            }
            NodeData::Element { .. } => self.element(h, ctx, out, para),
            NodeData::Document => self.children(h, ctx, out, para),
            _ => {}
        }
    }

    fn push_text(&mut self, para: &mut Para, ctx: &Ctx, text: &str) {
        let empty = para.runs.iter().all(|r| r.text.trim().is_empty());
        if empty && text.contains(['\u{a0}', '\n']) && text.trim().is_empty() && (ctx.pre || text.contains('\u{a0}')) {
            self.blank_hint += 1;
        }
        let text = clean_text(text);
        let text = if ctx.pre {
            text
        } else {
            let mut out = String::with_capacity(text.len());
            let mut last_space = para.runs.last().is_none_or(|r| r.text.ends_with(' ') || r.text.ends_with('\n'));
            for c in text.chars() {
                if c.is_whitespace() {
                    if !last_space {
                        out.push(' ');
                    }
                    last_space = true;
                } else {
                    out.push(c);
                    last_space = false;
                }
            }
            out
        };
        if text.is_empty() {
            return;
        }
        let at_word_start = para.runs.last().is_none_or(|r| r.text.ends_with(char::is_whitespace));
        let text = transformed(text, ctx.transform, at_word_start);
        if para.runs.is_empty() {
            para.align = ctx.align;
            para.family = ctx.family;
            para.line_height = ctx.line_height;
            para.tracking = ctx.tracking;
        }
        let run = Run {
            text: String::new(),
            weight: ctx.weight,
            italic: ctx.italic,
            underline: ctx.underline,
            strike: ctx.strike,
            mono: ctx.mono,
            size: ctx.size,
            color: ctx.color,
            bg: ctx.bg,
            href: ctx.href.clone(),
        };
        match para.runs.last_mut() {
            Some(last) if Run { text: String::new(), ..last.clone() } == run => last.text.push_str(&text),
            _ => para.runs.push(Run { text, ..run }),
        }
    }

    /// Notes what this visible element does that can't be drawn as designed.
    fn classify(&mut self, h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)], column: bool) {
        let f = &mut self.doc.fidelity;
        if let Some(p) = style(st, "position")
            && matches!(p, "absolute" | "fixed" | "sticky")
            && !h.children.borrow().is_empty()
        {
            f.note(Issue::Positioned);
        }
        if let Some(d) = style(st, "display")
            && matches!(d, "flex" | "inline-flex" | "grid" | "inline-grid")
            && h.children.borrow().iter().filter(|c| matches!(c.data, NodeData::Element { .. })).count() > 1
        {
            f.note(Issue::FlexGrid);
        }
        if !column
            && let Some(fl) = style(st, "float")
            && (fl == "left" || fl == "right")
            && (t != "img" || attr(h, "width").and_then(|w| w.parse::<f32>().ok()).unwrap_or(0.0) >= 120.0)
        {
            f.note(Issue::Float);
        }
        if ["transform", "clip-path", "mix-blend-mode", "filter"].iter().any(|k| style(st, k).is_some_and(|v| v != "none")) {
            f.note(Issue::Effect);
        }
        if style(st, "column-count").is_some_and(|v| v != "auto" && v != "1") || style(st, "columns").is_some_and(|v| v != "auto") {
            f.note(Issue::MultiColumn);
        }
        if style(st, "background-image").is_some_and(|v| v.contains("gradient(")) {
            f.note(Issue::Gradient);
        }
    }

    /// A skipped element that would have drawn something.
    fn skipped(&mut self, h: &Handle, t: &str) {
        let issue = match t {
            "script" => Issue::Script,
            "svg" => Issue::Svg,
            "iframe" | "video" | "audio" | "object" | "embed" | "canvas" => Issue::Embed,
            "input" | "select" | "textarea" => Issue::Form,
            _ => return,
        };
        if issue != Issue::Script {
            let st = self.styles(h);
            if hidden(h, t, &st) || attr(h, "type").is_some_and(|ty| ty.eq_ignore_ascii_case("hidden")) {
                return;
            }
            // Icon-sized drawings don't change the layout.
            let small = |a: &str| attr(h, a).and_then(|v| v.trim_end_matches("px").trim().parse::<f32>().ok()).is_some_and(|v| v <= 24.0);
            if issue == Issue::Svg && (small("width") || small("height")) {
                return;
            }
        }
        self.doc.fidelity.note(issue);
    }

    fn element(&mut self, h: &Handle, parent: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let Some(t) = tag(h) else { return };
        if SKIP.contains(&t.as_str()) {
            self.skipped(h, &t);
            return;
        }
        let st = self.styles(h);
        if hidden(h, &t, &st) {
            return;
        }
        self.classify(h, &t, &st, false);
        if t == "body"
            && let Some(bg) = style(&st, "background-color").and_then(parse_color).or_else(|| attr(h, "bgcolor").as_deref().and_then(parse_color))
        {
            self.doc.background = Some(bg);
        }
        self.depth += 1;
        if self.depth > MAX_DEPTH || stack_here().abs_diff(self.stack_top) > STACK_BUDGET {
            self.depth -= 1;
            if !self.too_deep {
                self.too_deep = true;
                self.doc.fidelity.note(Issue::TooDeep);
            }
            // Too deep to lay out: keep its text, without recursing further.
            self.flat_text(h, parent, out, para);
            return;
        }
        let mut ctx = parent.clone();
        if t == "a" {
            let href = attr(h, "href").map(|u| u.trim().to_owned()).filter(|u| !u.is_empty() && !u.starts_with('#') && !u.to_lowercase().starts_with("javascript:"));
            ctx.href = href.or(ctx.href);
        }
        text_style(h, &t, &st, &mut ctx);
        let display = style(&st, "display");
        let inline = !BLOCKS.contains(&t.as_str()) && !display.is_some_and(|d| matches!(d, "block" | "flex" | "grid" | "table" | "table-cell" | "table-row" | "list-item"));
        if inline {
            if let Some(bg) = style(&st, "background-color").and_then(parse_color).filter(|c| c[3] > 0) {
                ctx.bg = Some(bg);
            } else if t == "mark" {
                ctx.bg = Some([255, 240, 140, 255]);
            }
        } else {
            ctx.bg = None;
        }
        match t.as_str() {
            "br" => {
                if para.runs.iter().all(|r| r.text.trim().is_empty()) {
                    self.blank_hint += 1;
                }
                let br = Ctx { pre: true, ..ctx };
                self.push_text(para, &br, "\n");
            }
            "a" => self.link(h, &st, &ctx, out, para),
            "img" => self.image(h, &st, &ctx, out, para),
            "hr" => {
                flush(out, para);
                out.push(Node::Rule);
            }
            "ul" | "ol" => {
                flush(out, para);
                let (mt, mb) = vmargins(&st, ctx.size());
                space(out, mt);
                let marker = !style(&st, "list-style-type").or_else(|| style(&st, "list-style")).is_some_and(|v| v.contains("none"));
                let mut items = Vec::new();
                for c in h.children.borrow().iter() {
                    if tag(c).as_deref() == Some("li") {
                        let cst = self.styles(c);
                        if hidden(c, "li", &cst) {
                            continue;
                        }
                        let mut v = Vec::new();
                        let mut p = Para::default();
                        let mut cctx = ctx.clone();
                        text_style(c, "li", &cst, &mut cctx);
                        let bs = box_style(c, "li", &cst, &mut cctx, 0.0);
                        self.children(c, &cctx, &mut v, &mut p);
                        flush(&mut v, &mut p);
                        let mut v = tidy(v);
                        if bs.has_visuals() {
                            let mut b = Vec::new();
                            push_box(&mut b, BoxStyle { margin: [0.0; 4], ..bs }, v);
                            v = b;
                        }
                        if !v.is_empty() {
                            items.push(v);
                        }
                    } else {
                        self.node(c, &ctx, out, para);
                    }
                }
                if !items.is_empty() {
                    out.push(Node::List { ordered: t == "ol", marker, items });
                }
                space(out, mb);
            }
            "blockquote" => {
                flush(out, para);
                let mut v = Vec::new();
                let mut p = Para::default();
                self.children(h, &ctx, &mut v, &mut p);
                flush(&mut v, &mut p);
                if !v.is_empty() {
                    let (mt, mb) = vmargins(&st, ctx.size());
                    space(out, mt.min(12.0));
                    out.push(Node::Quote(tidy(v)));
                    space(out, mb.min(12.0));
                }
            }
            "pre" => {
                flush(out, para);
                let mut p = Para::default();
                self.children(h, &Ctx { pre: true, mono: true, ..ctx }, &mut Vec::new(), &mut p);
                let text = p.text();
                if !text.trim().is_empty() {
                    out.push(Node::Pre(text.trim_matches('\n').trim_end().to_owned()));
                }
            }
            "table" => {
                flush(out, para);
                let (mt, mb) = vmargins(&st, ctx.size());
                space(out, mt);
                self.table(h, &st, &mut ctx, out);
                space(out, mb);
            }
            "li" | "dd" => {
                // Outside a list: an indented block.
                flush(out, para);
                self.block(h, &t, &st, &mut ctx, out, 0.0);
            }
            _ if display.is_some_and(|d| d.starts_with("inline") || d == "contents") && !matches!(t.as_str(), "td" | "th" | "tr") => self.children(h, &ctx, out, para),
            _ if BLOCKS.contains(&t.as_str()) || display.is_some_and(|d| matches!(d, "block" | "flex" | "grid" | "table" | "table-cell" | "table-row" | "list-item")) => {
                flush(out, para);
                self.block(h, &t, &st, &mut ctx, out, 0.0);
            }
            _ => self.children(h, &ctx, out, para),
        }
        self.depth -= 1;
    }

    /// The visible text under `h` as plain paragraphs, walked with a stack of its own.
    fn flat_text(&mut self, h: &Handle, ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let mut stack = vec![h.clone()];
        while let Some(n) = stack.pop() {
            match &n.data {
                NodeData::Text { contents } => {
                    let text = contents.borrow();
                    self.push_text(para, ctx, &text);
                }
                NodeData::Element { .. } => {
                    let t = tag(&n).unwrap_or_default();
                    if SKIP.contains(&t.as_str()) {
                        continue;
                    }
                    let st = self.styles(&n);
                    if hidden(&n, &t, &st) {
                        continue;
                    }
                    if t == "br" {
                        self.push_text(para, &Ctx { pre: true, ..ctx.clone() }, "\n");
                    } else if BLOCKS.contains(&t.as_str()) {
                        flush(out, para);
                    }
                    stack.extend(n.children.borrow().iter().rev().cloned());
                }
                _ => {}
            }
        }
    }

    /// A link: inline text, or a box of its own when it's drawn as a button.
    fn link(&mut self, h: &Handle, st: &[(Rc<str>, Rc<str>)], ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let bs = box_style(h, "a", st, &mut ctx.clone(), 0.0);
        let display = style(st, "display");
        let padded = bs.pad.iter().any(|p| *p > 0.0);
        let blockish = display.is_some_and(|d| d.contains("block"));
        // A link drawn as a button: coloured or bordered and padded, or a padded inline block
        // in a coloured cell (the bulletproof button).
        if ((bs.bg.is_some() || bs.border.is_some() || bs.bg_image.is_some()) && (padded || blockish)) || (padded && blockish) {
            flush(out, para);
            let mut inner = Vec::new();
            let mut p = Para::default();
            self.children(h, ctx, &mut inner, &mut p);
            flush(&mut inner, &mut p);
            if !inner.is_empty() {
                let full = display == Some("block") && bs.width.is_some_and(|w| matches!(w, Len::Pct(p) if p >= 99.0));
                out.push(Node::Box { style: BoxStyle { href: ctx.href.clone(), tight: !full, margin: [0.0; 4], ..bs }, children: inner });
            }
        } else {
            self.children(h, ctx, out, para);
        }
    }

    /// A block element: its content, wrapped in a box when it has a visual style of its own,
    /// with its vertical margins as space around it.
    fn block(&mut self, h: &Handle, t: &str, st: &[(Rc<str>, Rc<str>)], ctx: &mut Ctx, out: &mut Vec<Node>, cellpadding: f32) {
        let place = placement(h, t, st);
        let bs = box_style(h, t, st, ctx, cellpadding);
        let (mt, mb) = vmargins(st, ctx.size());
        let saved = ctx.avail;
        ctx.avail = narrowed(ctx.avail, &bs);
        let hints = self.blank_hint;
        let mut inner = Vec::new();
        let mut p = Para::default();
        self.children(h, ctx, &mut inner, &mut p);
        flush(&mut inner, &mut p);
        ctx.avail = saved;
        space(out, mt);
        if inner.is_empty() && self.blank_hint > hints && !bs.has_visuals() {
            out.push(Node::Blank(line_px(ctx)));
        } else {
            push_placed(out, place, bs, tidy(inner));
        }
        space(out, mb);
    }

    fn image(&mut self, h: &Handle, st: &[(Rc<str>, Rc<str>)], ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let Some(src) = attr(h, "src").map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) else { return };
        let px = |v: &str| match len_attr(v) {
            Some(Len::Px(w)) => Some(w),
            _ => None,
        };
        let zero_attr = |a: &str| attr(h, a).is_some_and(|v| v.trim().trim_end_matches("px").trim() == "0");
        let max = style(st, "max-width").and_then(px);
        let width = attr(h, "width").as_deref().and_then(px).or_else(|| style(st, "width").and_then(px)).map(|w| max.map_or(w, |m| w.min(m))).or(max);
        let height = attr(h, "height").as_deref().and_then(px).or_else(|| style(st, "height").and_then(px));
        // Tracking pixels and spacers.
        if width.is_some_and(|w| w <= 3.0) || height.is_some_and(|h| h <= 3.0) || zero_attr("width") || zero_attr("height") {
            return;
        }
        if !usable_src(&src) {
            return;
        }
        flush(out, para);
        if !self.doc.images.contains(&src) {
            self.doc.images.push(src.clone());
        }
        let img = Node::Image(Img { src, alt: attr(h, "alt").unwrap_or_default().trim().to_owned(), width, height, href: ctx.href.clone() });
        if style(st, "margin-left") == Some("auto") && style(st, "margin-right") == Some("auto") && ctx.align != Align::Center {
            out.push(Node::Box { style: BoxStyle { align: Align::Center, ..BoxStyle::default() }, children: vec![img] });
        } else {
            out.push(img);
        }
    }

    fn note_image(&mut self, s: &BoxStyle) {
        if let Some(u) = &s.bg_image
            && !self.doc.images.contains(u)
        {
            self.doc.images.push(u.clone());
        }
    }

    fn table(&mut self, h: &Handle, st: &[(Rc<str>, Rc<str>)], ctx: &mut Ctx, out: &mut Vec<Node>) {
        let cellpadding = attr(h, "cellpadding").and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(0.0).clamp(0.0, 32.0);
        let collapse = style(st, "border-collapse") == Some("collapse");
        let spacing = if collapse {
            0.0
        } else {
            style(st, "border-spacing")
                .and_then(|v| v.split_whitespace().next().and_then(|x| length(x, BASE)))
                .or_else(|| attr(h, "cellspacing").and_then(|v| v.trim().parse::<f32>().ok()))
                .unwrap_or(0.0)
                .clamp(0.0, 24.0)
        };
        let border_attr = attr(h, "border").and_then(|v| v.trim().parse::<f32>().ok()).is_some_and(|w| w >= 1.0);
        let place = placement(h, "table", st);
        // `<table align>` places the table; it doesn't align the text inside it. Alignment from
        // `align=` and `<center>` outside stops here, as it does in browsers.
        if ctx.legacy {
            ctx.align = Align::Left;
            ctx.legacy = false;
        }
        let saved = ctx.align;
        let mut tstyle = box_style(h, "table", st, ctx, 0.0);
        if style(st, "text-align").is_none() {
            ctx.align = saved;
            tstyle.align = saved;
        }
        self.note_image(&tstyle);
        let declared = tstyle.width.is_some();
        let twidth = (match tstyle.width {
            Some(Len::Px(w)) => w,
            Some(Len::Pct(p)) => ctx.avail * p / 100.0,
            None => ctx.avail,
        })
        .min(tstyle.max_width.unwrap_or(f32::MAX))
            - tstyle.pad[1]
            - tstyle.pad[3];
        let twidth = twidth.max(40.0);
        let mut rows: Vec<Handle> = Vec::new();
        let mut caption = Vec::new();
        for c in h.children.borrow().iter() {
            match tag(c).as_deref() {
                Some("tr") => rows.push(c.clone()),
                Some("tbody") | Some("thead") | Some("tfoot") => {
                    let sst = self.styles(c);
                    if hidden(c, "tbody", &sst) {
                        continue;
                    }
                    for r in c.children.borrow().iter() {
                        if tag(r).as_deref() == Some("tr") {
                            rows.push(r.clone());
                        }
                    }
                }
                Some("caption") => {
                    let mut p = Para::default();
                    self.children(c, ctx, &mut caption, &mut p);
                    flush(&mut caption, &mut p);
                }
                _ => {}
            }
        }

        // The grid: cells placed by colspan and rowspan.
        let mut grid: Vec<GRow> = Vec::new();
        // Per starting column: rows still to cover below, and how many columns.
        let mut pending: Vec<(usize, usize)> = Vec::new();
        let mut ncols = 0usize;
        for r in &rows {
            let rst = self.styles(r);
            if hidden(r, "tr", &rst) {
                continue;
            }
            self.classify(r, "tr", &rst, false);
            let mut rctx = ctx.clone();
            text_style(r, "tr", &rst, &mut rctx);
            let row_style = box_style(r, "tr", &rst, &mut rctx, 0.0);
            self.note_image(&row_style);
            let cell_handles: Vec<Handle> = r.children.borrow().iter().filter(|c| matches!(tag(c).as_deref(), Some("td" | "th"))).cloned().collect();
            // Cells all set to `display: block` (vertical menus) stack: each is a row of its own.
            let stacked = cell_handles.len() > 1
                && cell_handles.iter().all(|c| {
                    let cst = self.styles(c);
                    style(&cst, "display") == Some("block") || hidden(c, "td", &cst)
                });
            let n_here = cell_handles.len().max(1) as f32;
            let mut covered: Vec<(usize, usize)> = Vec::new();
            let mut started: Vec<(usize, usize, usize)> = Vec::new();
            let mut cells = Vec::new();
            let mut col = 0usize;
            let skip = |col: &mut usize, covered: &mut Vec<(usize, usize)>| {
                while *col < pending.len() && pending[*col].0 > 0 {
                    let span = pending[*col].1.max(1);
                    covered.push((*col, span));
                    *col += span;
                }
            };
            for c in &cell_handles {
                let ct = tag(c).unwrap_or_default();
                let cst = self.styles(c);
                if hidden(c, &ct, &cst) {
                    continue;
                }
                self.classify(c, &ct, &cst, false);
                skip(&mut col, &mut covered);
                let span = attr(c, "colspan").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 50);
                let rspan = attr(c, "rowspan").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 50);
                let mut cctx = rctx.clone();
                text_style(c, &ct, &cst, &mut cctx);
                let mut bs = box_style(c, &ct, &cst, &mut cctx, cellpadding);
                if border_attr && bs.border.is_none() {
                    bs.border = Some(Border { widths: [1.0; 4], color: [160, 160, 160, 255], dashed: false });
                }
                self.note_image(&bs);
                let decl = bs.width;
                cctx.avail = (match decl {
                    Some(Len::Px(w)) => w.min(twidth),
                    Some(Len::Pct(p)) => twidth * p / 100.0,
                    None => twidth / n_here * span as f32,
                } - bs.pad[1]
                    - bs.pad[3])
                    .max(20.0);
                let mut v = Vec::new();
                let mut p = Para::default();
                self.children(c, &cctx, &mut v, &mut p);
                flush(&mut v, &mut p);
                let height = attr(c, "height").and_then(|x| x.trim_end_matches("px").trim().parse::<f32>().ok()).or_else(|| style(&cst, "height").and_then(|x| length(x, BASE))).unwrap_or(0.0);
                cells.push(GCell { col, span, rows: rspan, style: bs, decl, nodes: tidy(v), height });
                if rspan > 1 {
                    started.push((col, rspan - 1, span));
                }
                col += span;
            }
            skip(&mut col, &mut covered);
            if stacked {
                for c in cells {
                    grid.push(GRow { style: row_style.clone(), cells: vec![GCell { col: 0, span: 1, ..c }], covered: Vec::new(), full: true });
                }
                ncols = ncols.max(1);
                continue;
            }
            ncols = ncols.max(col);
            for (c, _) in &covered {
                pending[*c].0 -= 1;
            }
            for (c, rest, span) in started {
                if pending.len() < c + 1 {
                    pending.resize(c + 1, (0, 0));
                }
                pending[c] = (rest, span);
            }
            grid.push(GRow { style: row_style, cells, covered, full: false });
        }

        for r in grid.iter_mut().filter(|r| r.full) {
            for c in &mut r.cells {
                c.span = ncols.max(1);
            }
        }
        let mut inner: Vec<Node> = caption;
        if ncols <= 1 {
            if !declared && grid.iter().all(|r| r.cells.iter().all(|c| c.decl.is_none())) {
                // Tables shrink to their content: a button, a badge, a logo.
                let cap = twidth * 0.6;
                tstyle.tight = grid.iter().flat_map(|r| &r.cells).all(|c| {
                    let edge = c.style.pad[1] + c.style.pad[3] + c.style.border.as_ref().map_or(0.0, |b| b.widths[1] + b.widths[3]);
                    table::estimate_capped(&c.nodes, cap).1 + edge < cap
                });
            }
            // A layout table: each row is its cell.
            for row in grid {
                let mut spacer: f32 = 0.0;
                let mut kept = Vec::new();
                for c in row.cells {
                    if c.nodes.is_empty() && c.style.bg.is_none() && c.style.bg_image.is_none() {
                        spacer = spacer.max(c.height);
                        continue;
                    }
                    let width = c.style.width.filter(|w| matches!(w, Len::Px(_)));
                    let tight = c.style.tight || tstyle.tight;
                    push_box(&mut kept, BoxStyle { width, tight, ..c.style }, c.nodes);
                }
                if kept.is_empty() {
                    if spacer > 0.0 {
                        inner.push(Node::Blank(spacer.min(48.0)));
                    }
                    continue;
                }
                push_box(&mut inner, row.style, kept);
                space(&mut inner, spacing);
            }
            push_placed(out, place, tstyle, tidy(inner));
            return;
        }

        // The column model, shared by every row.
        let claims: Vec<table::Claim> = grid
            .iter()
            .flat_map(|r| r.cells.iter())
            .map(|c| {
                let (lo, hi) = table::estimate(&c.nodes);
                let edge = c.style.pad[1] + c.style.pad[3] + c.style.border.as_ref().map_or(0.0, |b| b.widths[1] + b.widths[3]);
                table::Claim { col: c.col, span: c.span, width: c.decl, min: lo + edge, max: hi * 1.1 + edge }
            })
            .collect();
        let model = table::resolve(ncols, &claims, twidth, declared);
        if !declared {
            tstyle.width = Some(Len::Px(model.width + tstyle.pad[1] + tstyle.pad[3] + spacing * (ncols - 1) as f32));
        }
        let frac = |col: usize, span: usize| -> f32 { model.fracs[col.min(ncols)..(col + span).min(ncols)].iter().sum() };
        // Small fixed columns (images, gaps) keep their width when the pane narrows.
        let min_of = |col: usize, span: usize| -> f32 { (col..(col + span).min(ncols)).filter(|&i| model.fixed[i] && model.widths[i] <= 120.0).map(|i| model.widths[i]).sum() };
        let cell = |c: GCell, scale: f32| -> (BoxStyle, Vec<Node>) {
            let f = frac(c.col, c.span) / scale;
            (BoxStyle { width: Some(Len::Pct(f * 100.0)), min_width: c.style.min_width.max(min_of(c.col, c.span)), ..c.style }, c.nodes)
        };
        let nrows = grid.len();
        let mut grid: Vec<Option<GRow>> = grid.into_iter().map(Some).collect();
        let mut ri = 0usize;
        while ri < nrows {
            let Some(mut row) = grid[ri].take() else {
                ri += 1;
                continue;
            };
            // A rowspan down the first or last column over rows with none of their own: the
            // band becomes one row, its other cell holding the band's rows.
            let band = row.cells.iter().position(|c| c.rows > 1 && (c.col == 0 || c.col + c.span == ncols) && ri + c.rows <= nrows).filter(|&k| {
                let n = row.cells[k].rows;
                row.cells.iter().enumerate().all(|(i, x)| i == k || x.rows == 1) && (ri + 1..ri + n).all(|j| grid[j].as_ref().is_some_and(|r| r.cells.iter().all(|x| x.rows == 1)))
            });
            if let Some(k) = band {
                let span_cell = row.cells.remove(k);
                let n = span_cell.rows;
                let first = span_cell.col == 0;
                let span_frac = frac(span_cell.col, span_cell.span);
                let rest = (1.0 - span_frac).max(0.01);
                let mut sub = Vec::new();
                let mut band_rows = vec![row];
                band_rows.extend((ri + 1..ri + n).filter_map(|j| grid[j].take()));
                for r in band_rows {
                    let cells: Vec<(BoxStyle, Vec<Node>)> = r.cells.into_iter().map(|c| cell(c, rest)).collect();
                    push_box(&mut sub, r.style, vec![Node::Row { cells, gap: spacing }]);
                    space(&mut sub, spacing);
                }
                let span_box = (BoxStyle { width: Some(Len::Pct(span_frac * 100.0)), ..span_cell.style }, span_cell.nodes);
                let rest_box = (BoxStyle { width: Some(Len::Pct(rest * 100.0)), valign: VAlign::Top, ..BoxStyle::default() }, tidy(sub));
                let cells = if first { vec![span_box, rest_box] } else { vec![rest_box, span_box] };
                inner.push(Node::Row { cells, gap: spacing });
                space(&mut inner, spacing);
                ri += n;
                continue;
            }
            if row.cells.iter().any(|c| c.rows > 1) {
                // Its content stays in its first row; the rows below keep the column free.
                self.doc.fidelity.note(Issue::Rowspan);
            }
            let filled = |c: &GCell| !c.nodes.is_empty() || c.style.bg.is_some() || c.style.bg_image.is_some();
            let content = row.cells.iter().filter(|c| filled(c)).count();
            if content == 0 {
                let spacer = row.cells.iter().map(|c| c.height).fold(0.0, f32::max);
                if spacer > 0.0 {
                    inner.push(Node::Blank(spacer.min(48.0)));
                }
                ri += 1;
                continue;
            }
            if content == 1
                && row.covered.is_empty()
                && let Some(k) = row.cells.iter().position(|c| filled(c) && c.col == 0 && c.span >= ncols)
            {
                // One cell across the whole table: a block.
                let c = row.cells.swap_remove(k);
                let mut tmp = Vec::new();
                push_box(&mut tmp, BoxStyle { width: None, ..c.style }, c.nodes);
                push_box(&mut inner, row.style, tmp);
                space(&mut inner, spacing);
                ri += 1;
                continue;
            }
            let mut slots: Vec<(usize, (BoxStyle, Vec<Node>))> = row.cells.into_iter().map(|c| (c.col, cell(c, 1.0))).collect();
            for (col, span) in row.covered {
                slots.push((col, (BoxStyle { width: Some(Len::Pct(frac(col, span) * 100.0)), min_width: min_of(col, span), ..BoxStyle::default() }, Vec::new())));
            }
            slots.sort_by_key(|s| s.0);
            let cells = slots.into_iter().map(|s| s.1).collect();
            push_box(&mut inner, row.style, vec![Node::Row { cells, gap: spacing }]);
            space(&mut inner, spacing);
            ri += 1;
        }
        push_placed(out, place, tstyle, tidy(inner));
    }
}

/// A table cell placed on the grid.
struct GCell {
    col: usize,
    span: usize,
    rows: usize,
    style: BoxStyle,
    decl: Option<Len>,
    nodes: Vec<Node>,
    height: f32,
}

struct GRow {
    style: BoxStyle,
    cells: Vec<GCell>,
    /// Columns covered by a cell from a row above: (column, span).
    covered: Vec<(usize, usize)>,
    /// One cell across the whole table (a stacked cell).
    full: bool,
}

fn space(out: &mut Vec<Node>, h: f32) {
    if h > 0.0 {
        out.push(Node::Space(h));
    }
}

/// The room inside a box, for the boxes and tables in it.
fn narrowed(avail: f32, bs: &BoxStyle) -> f32 {
    let w = match bs.width {
        Some(Len::Px(w)) => w.min(avail),
        Some(Len::Pct(p)) => avail * p / 100.0,
        None => avail,
    }
    .min(bs.max_width.unwrap_or(f32::MAX));
    (w - bs.pad[1] - bs.pad[3] - bs.margin[1] - bs.margin[3]).max(40.0)
}

fn line_px(ctx: &Ctx) -> f32 {
    let size = ctx.size();
    match ctx.line_height {
        Some(LineHeight::Mult(m)) => size * m,
        Some(LineHeight::Px(p)) => p,
        None => (size * 1.45).round(),
    }
    .clamp(1.0, 48.0)
}

/// A box placed in its parent (`margin: 0 auto`, `<table align>`) when it's narrower.
fn push_placed(out: &mut Vec<Node>, place: Option<Align>, style: BoxStyle, children: Vec<Node>) {
    match place {
        Some(a) if a != Align::Left && (style.width.is_some() || style.max_width.is_some() || style.tight) => {
            let mut inner = Vec::new();
            push_box(&mut inner, style, children);
            if !inner.is_empty() {
                out.push(Node::Box { style: BoxStyle { align: a, ..BoxStyle::default() }, children: inner });
            }
        }
        _ => push_box(out, style, children),
    }
}

fn push_box(out: &mut Vec<Node>, style: BoxStyle, children: Vec<Node>) {
    if children.is_empty() && style.bg.is_none() && style.bg_image.is_none() {
        return;
    }
    if style.is_plain() {
        // Alignment still matters for images and boxes inside.
        if style.align != Align::Left && children.iter().any(|c| matches!(c, Node::Image(_) | Node::Box { .. })) {
            out.push(Node::Box { style, children });
        } else {
            out.extend(children);
        }
        return;
    }
    if let [Node::Box { style: inner, .. }] = children.as_slice()
        && !style.has_visuals()
        && (inner.width.is_none() || style.width.is_none())
        && (style.align == Align::Left || (inner.width.is_none() && inner.max_width.is_none() && !inner.tight))
        // A button inside a full-width wrapper stays its own size.
        && !(inner.tight && (style.width.is_some() || style.max_width.is_some() || style.min_width > 0.0))
    {
        // A wrapper with only padding, margins or a width: fold it into the box it holds.
        let Some(Node::Box { style: inner, children: kids }) = children.into_iter().next() else { unreachable!() };
        let mut merged = inner.clone();
        for i in 0..4 {
            if inner.has_visuals() {
                merged.margin[i] += style.pad[i];
            } else {
                merged.pad[i] += style.pad[i];
            }
            merged.margin[i] += style.margin[i];
        }
        merged.width = style.width.or(inner.width);
        merged.max_width = match (style.max_width, inner.max_width) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        merged.min_width = merged.min_width.max(style.min_width);
        out.push(Node::Box { style: merged, children: kids });
        return;
    }
    let mut children = children;
    if let Some(u) = &style.bg_image {
        same_picture(&mut children, u, style.bg, 3);
    }
    out.push(Node::Box { style, children });
}

/// Drops a background picture (and colour) a box inside repeats: hybrid templates set it on
/// a `<div>` for most clients and again on a table for Outlook.
fn same_picture(nodes: &mut [Node], url: &str, bg: Option<Rgba>, depth: u32) {
    for n in nodes {
        if let Node::Box { style, children } = n {
            if style.bg_image.as_deref() == Some(url) {
                style.bg_image = None;
                if style.bg == bg {
                    style.bg = None;
                }
            } else if depth > 0 && !style.has_visuals() {
                same_picture(children, url, bg, depth - 1);
            }
        }
    }
}

/// Collapses runs of spacers and drops spacers at the ends.
fn tidy(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(nodes.len());
    for n in nodes {
        if let Node::Space(h) = n {
            if h <= 0.0 {
                continue;
            }
            match out.last_mut() {
                None => continue,
                Some(Node::Space(prev)) => {
                    *prev = prev.max(h);
                    continue;
                }
                _ => {}
            }
        }
        out.push(n);
    }
    while matches!(out.last(), Some(Node::Space(_))) {
        out.pop();
    }
    out
}

/// Plain text of a document, for previews and quoting.
pub fn plain(doc: &Doc) -> String {
    fn cells_text(cells: &[(BoxStyle, Vec<Node>)], out: &mut String) {
        let parts: Vec<String> = cells
            .iter()
            .map(|(_, v)| {
                let mut s = String::new();
                walk(v, &mut s);
                s.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .filter(|s| !s.is_empty())
            .collect();
        out.push_str(&parts.join("   "));
        out.push_str("\n\n");
    }
    fn walk(nodes: &[Node], out: &mut String) {
        for n in nodes {
            match n {
                Node::Para(p) => {
                    out.push_str(&p.text());
                    out.push_str("\n\n");
                }
                Node::Box { children, .. } | Node::Quote(children) => walk(children, out),
                Node::Row { cells, .. } => cells_text(cells, out),
                Node::Columns { cells, .. } => {
                    for (_, v) in cells {
                        walk(v, out);
                    }
                }
                Node::List { items, .. } => {
                    for it in items {
                        out.push_str("• ");
                        walk(it, out);
                    }
                }
                Node::Pre(t) => {
                    out.push_str(t);
                    out.push_str("\n\n");
                }
                Node::Image(_) | Node::Rule | Node::Space(_) | Node::Blank(_) => {}
            }
        }
    }
    let mut s = String::new();
    walk(&doc.nodes, &mut s);
    let mut out = String::new();
    let mut blank = 0;
    for l in s.lines() {
        if l.trim().is_empty() {
            blank += 1;
            if blank == 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(l.trim_end());
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

#[allow(dead_code)]
fn _assert_send() {
    fn send<T: Send + Sync>() {}
    send::<Doc>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(nodes: &'a [Node], f: &impl Fn(&Node) -> bool) -> Option<&'a Node> {
        for n in nodes {
            if f(n) {
                return Some(n);
            }
            let inner = match n {
                Node::Box { children, .. } | Node::Quote(children) => find(children, f),
                Node::Row { cells, .. } | Node::Columns { cells, .. } => cells.iter().find_map(|(_, k)| find(k, f)),
                Node::List { items, .. } => items.iter().find_map(|k| find(k, f)),
                _ => None,
            };
            if inner.is_some() {
                return inner;
            }
        }
        None
    }

    #[test]
    fn receipt_rows_stay_rows() {
        let html = r##"<html><head><style>.x{}</style></head><body bgcolor="#f6f9fc">
          <div style="display:none;max-height:0;overflow:hidden">Preheader text</div>
          <table width="600" align="center"><tr><td>
            <table><tr><td style="font-size:24px;font-weight:bold;color:#1a1a1a">$5.00</td></tr></table>
            <table><tr><td width="20"></td>
              <td><table><tr><td>Object Storage</td></tr></table><table><tr><td height="3"></td></tr></table><table><tr><td style="color:#687385">Qty 0</td></tr></table></td>
              <td width="20"></td><td align="right" nowrap>$0.00</td></tr></table>
            <img src="https://track.example/p.gif" width="1" height="1">
            <a href="https://pay.example" style="background:#635bff;padding:12px 20px;color:#fff;border-radius:6px">View invoice</a>
          </td></tr></table></body></html>"##;
        let doc = parse(html);
        let text = plain(&doc);
        assert!(!text.contains("Preheader"), "{text}");
        assert!(text.contains("Object Storage Qty 0   $0.00"), "{text}");
        assert!(doc.images.is_empty(), "tracking pixel dropped");
        assert_eq!(doc.background, Some([0xf6, 0xf9, 0xfc, 255]));
        assert!(find(&doc.nodes, &|n| matches!(n, Node::Row { cells, .. } if cells.iter().any(|(s, _)| s.align == Align::Right && s.tight))).is_some(), "{:#?}", doc.nodes);
        assert!(find(&doc.nodes, &|n| matches!(n, Node::Box { style, .. } if style.href.is_some() && style.bg == Some([0x63, 0x5b, 0xff, 255]))).is_some());
    }

    #[test]
    fn inline_styles_and_breaks() {
        let doc = parse("<p>Hello <b>bold</b> <a href='https://x.y'>link</a><br>next&nbsp;line <s>old</s></p>");
        let Node::Para(p) = &doc.nodes[0] else { panic!("{:?}", doc.nodes) };
        assert_eq!(p.text(), "Hello bold link\nnext line old");
        assert!(p.runs.iter().any(|r| r.bold() && r.text == "bold"));
        assert!(p.runs.iter().any(|r| r.href.as_deref() == Some("https://x.y") && r.underline));
        assert!(p.runs.iter().any(|r| r.strike && r.text == "old"));
    }

    #[test]
    fn style_sheets_style_elements() {
        let doc = parse(
            "<style>p.lead { font-size: 18px; line-height: 1.5; color: #333 } .caps { text-transform: uppercase; letter-spacing: 2px } #x { font-family: Georgia, serif }</style>
             <p class='lead'>Big</p><p class='caps'>shout</p><p id='x'>Serif</p>",
        );
        let ps: Vec<&Para> = doc.nodes.iter().filter_map(|n| if let Node::Para(p) = n { Some(p) } else { None }).collect();
        assert_eq!((ps[0].runs[0].size, ps[0].line_height, ps[0].runs[0].color), (Some(18.0), Some(LineHeight::Mult(1.5)), Some([0x33, 0x33, 0x33, 255])));
        assert_eq!((ps[1].text(), ps[1].tracking), ("SHOUT".to_owned(), 2.0));
        assert_eq!(ps[2].family, Family::Serif);
    }

    #[test]
    fn colspan_shares_the_grid() {
        let doc = parse("<table width='400'><tr><td colspan='2'>Head</td></tr><tr><td width='100'>a</td><td>b</td></tr></table>");
        let row = find(&doc.nodes, &|n| matches!(n, Node::Row { .. })).expect("row");
        let Node::Row { cells, .. } = row else { unreachable!() };
        assert_eq!(cells[0].0.width, Some(Len::Pct(25.0)));
        assert_eq!(cells[1].0.width, Some(Len::Pct(75.0)));
        assert!(find(&doc.nodes, &|n| matches!(n, Node::Para(p) if p.text() == "Head")).is_some());
    }

    #[test]
    fn background_pictures_and_privacy() {
        let doc = parse(
            "<table background='https://img.example/a.jpg'><tr><td style=\"background:url('cid:hero@x') #123\">Hi</td></tr></table>
             <div style='background-image:url(data:image/png;base64,AAAA)'>x</div><div style='background:linear-gradient(#f00, #00f)'>y</div>",
        );
        assert_eq!(doc.images, vec!["https://img.example/a.jpg".to_owned(), "cid:hero@x".to_owned()], "only loadable URLs, through the same path as <img>");
        assert!(find(&doc.nodes, &|n| matches!(n, Node::Box { style, .. } if style.bg == Some([255, 0, 0, 255]))).is_some(), "a gradient's first colour");
        assert_eq!(doc.fidelity.count(Issue::Gradient), 1);
    }

    #[test]
    fn blank_lines_and_spacers() {
        let doc = parse("<div>one</div><div><br></div><div>two</div><p>&nbsp;</p><table><tr><td height='24'>&nbsp;</td></tr></table><div>three</div>");
        let kinds: Vec<&str> = doc
            .nodes
            .iter()
            .map(|n| match n {
                Node::Para(_) => "p",
                Node::Blank(_) => "blank",
                Node::Space(_) => "space",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["p", "blank", "p", "space", "blank", "space", "blank", "p"], "{:#?}", doc.nodes);
    }

    #[test]
    fn tags_keep_a_bounded_number_of_attributes() {
        let many = (0..2_000).map(|i| format!("a{i}=\"x y\" ")).collect::<String>();
        let html = format!("<p class=it's title=\"a > b\" {many}>Hello</p><b>world</b>");
        let limited = prefilter(&html);
        assert!(limited.len() < html.len() / 2);
        assert!(limited.ends_with(">Hello</p><b>world</b>"), "{}", &limited[limited.len() - 40..]);
        assert_eq!(plain(&parse(&html)), "Hello\n\nworld");
        let ordinary = "<p class='a' style=\"x\" data-x=it's>Hi <b>there</b> a<b and > c</p>";
        assert!(matches!(prefilter(ordinary), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn deep_nesting_is_capped_before_html5ever() {
        // Past MAX_OPEN open elements, start tags (and their end tags) go; their text stays.
        let html = format!("{}deep{}<p>after</p>", "<div>".repeat(1_000), "</div>".repeat(1_000));
        let f = prefilter(&html);
        assert_eq!(f.matches("<div>").count(), MAX_OPEN);
        assert_eq!(f.matches("</div>").count(), MAX_OPEN);
        assert_eq!(plain(&parse(&html)), "deep\n\nafter");
        // Comments and raw text aren't tags.
        let html = format!("<!--{}--><style>{}</style><p>x</p>", "<div>".repeat(1_000), "<div>".repeat(1_000));
        assert!(matches!(prefilter(&html), std::borrow::Cow::Borrowed(_)));
        // Real mail passes through untouched.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../envoy-core/tests/fixtures/html");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "html") {
                let html = std::fs::read_to_string(&p).unwrap();
                assert!(matches!(prefilter(&html), std::borrow::Cow::Borrowed(_)), "{} was changed", p.display());
            }
        }
    }

    #[test]
    fn a_panic_while_parsing_becomes_an_empty_doc() {
        // Parsing runs while the store is locked: a panic must not unwind into the lock.
        let doc = guarded(|| panic!("a renderer bug"));
        assert_eq!(doc, Doc::default());
    }

    #[test]
    fn hostile_nth_child_doesnt_panic() {
        let doc = parse("<style>p:nth-child(-n-2147483647){color:red}</style><p>Hello</p>");
        assert_eq!(plain(&doc), "Hello");
    }

}
