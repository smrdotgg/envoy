//! HTML email → a small layout tree the UI draws natively.
//!
//! Email HTML is table soup from the 2000s: layout tables nested dozens deep, inline styles,
//! spacer cells, hidden preheaders and tracking pixels. This keeps what a reader sees —
//! paragraphs of styled text, rows of cells with their widths and alignment, coloured boxes
//! and buttons, images — and drops the rest. The result is plain data (`Send`), built off the
//! UI thread and cheap to turn into elements.

use std::rc::Rc;

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// sRGB colour with alpha, 0–255.
pub type Rgba = [u8; 4];

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Len {
    Px(f32),
    Pct(f32),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub mono: bool,
    pub size: Option<f32>,
    pub color: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub href: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Para {
    pub runs: Vec<Run>,
    pub align: Align,
}

impl Para {
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct BoxStyle {
    pub align: Align,
    pub bg: Option<Rgba>,
    /// top, right, bottom, left
    pub pad: [f32; 4],
    pub width: Option<Len>,
    pub radius: f32,
    pub border: Option<(f32, Rgba)>,
    pub href: Option<String>,
    /// `valign="top"` (cells default to middle, as in HTML).
    pub top: bool,
    /// Size to content instead of sharing the row (`nowrap`, small fixed cells).
    pub tight: bool,
}

impl BoxStyle {
    fn is_plain(&self) -> bool {
        self.bg.is_none() && self.pad == [0.0; 4] && self.width.is_none() && self.border.is_none() && self.href.is_none() && self.radius == 0.0
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
    Row { cells: Vec<(BoxStyle, Vec<Node>)> },
    Image(Img),
    Rule,
    Space(f32),
    List { ordered: bool, items: Vec<Vec<Node>> },
    Quote(Vec<Node>),
    Pre(String),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Doc {
    pub nodes: Vec<Node>,
    /// Every image source, in order: `http(s)://…` and `cid:…`.
    pub images: Vec<String>,
    /// The email's own page background, if it set one.
    pub background: Option<Rgba>,
}

#[derive(Clone, Default)]
struct Ctx {
    bold: bool,
    italic: bool,
    underline: bool,
    mono: bool,
    size: Option<f32>,
    color: Option<Rgba>,
    href: Option<String>,
    align: Align,
    pre: bool,
}

pub fn parse(html: &str) -> Doc {
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).from_utf8().read_from(&mut html.as_bytes());
    let Ok(dom) = dom else { return Doc::default() };
    let mut b = Builder { doc: Doc::default(), depth: 0 };
    let mut out = Vec::new();
    let mut para = Para::default();
    b.children(&dom.document, &Ctx::default(), &mut out, &mut para);
    flush(&mut out, &mut para);
    b.doc.nodes = tidy(out);
    b.doc
}

struct Builder {
    doc: Doc,
    depth: usize,
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

/// `style="a: b; c: d"` as lowercase pairs.
fn styles(h: &Handle) -> Vec<(String, String)> {
    let Some(s) = attr(h, "style") else { return Vec::new() };
    s.split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().trim_end_matches("!important").trim().to_ascii_lowercase()))
        })
        .collect()
}

fn style<'a>(st: &'a [(String, String)], key: &str) -> Option<&'a str> {
    st.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn hidden(h: &Handle, st: &[(String, String)]) -> bool {
    if attr(h, "hidden").is_some() || attr(h, "aria-hidden").is_some_and(|v| v == "true") && tag(h).as_deref() == Some("div") {
        return true;
    }
    let zero = |v: Option<&str>| v.is_some_and(|v| v.trim_end_matches("px").trim() == "0");
    style(st, "display") == Some("none")
        || style(st, "visibility") == Some("hidden")
        || style(st, "mso-hide") == Some("all")
        || zero(style(st, "opacity"))
        || ((zero(style(st, "max-height")) || zero(style(st, "height"))) && style(st, "overflow") == Some("hidden"))
        || (zero(style(st, "font-size")) && zero(style(st, "line-height")))
}

pub fn parse_color(v: &str) -> Option<Rgba> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        let p = |s: &str| u8::from_str_radix(s, 16).ok();
        return match hex.len() {
            3 => {
                let c: Vec<char> = hex.chars().collect();
                Some([p(&c[0].to_string().repeat(2))?, p(&c[1].to_string().repeat(2))?, p(&c[2].to_string().repeat(2))?, 255])
            }
            6 => Some([p(&hex[0..2])?, p(&hex[2..4])?, p(&hex[4..6])?, 255]),
            8 => Some([p(&hex[0..2])?, p(&hex[2..4])?, p(&hex[4..6])?, p(&hex[6..8])?]),
            _ => None,
        };
    }
    if let Some(inner) = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb(")) {
        let parts: Vec<f32> = inner.trim_end_matches(')').split(',').filter_map(|p| p.trim().trim_end_matches('%').parse().ok()).collect();
        if parts.len() >= 3 {
            let a = parts.get(3).map_or(255, |a| if *a <= 1.0 { (a * 255.0) as u8 } else { *a as u8 });
            return Some([parts[0] as u8, parts[1] as u8, parts[2] as u8, a]);
        }
        return None;
    }
    Some(match v {
        "white" => [255, 255, 255, 255],
        "black" => [0, 0, 0, 255],
        "red" => [255, 0, 0, 255],
        "green" => [0, 128, 0, 255],
        "blue" => [0, 0, 255, 255],
        "gray" | "grey" => [128, 128, 128, 255],
        "silver" => [192, 192, 192, 255],
        "navy" => [0, 0, 128, 255],
        "orange" => [255, 165, 0, 255],
        "purple" => [128, 0, 128, 255],
        "transparent" => [0, 0, 0, 0],
        _ => return None,
    })
}

/// A CSS length in points (`16px`, `12pt`, `1.2em`, `80%` of `base`).
fn length(v: &str, base: f32) -> Option<f32> {
    let v = v.trim();
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(n) = v.strip_suffix("px") {
        num(n)
    } else if let Some(n) = v.strip_suffix("pt") {
        num(n).map(|n| n * 4.0 / 3.0)
    } else if let Some(n) = v.strip_suffix("rem") {
        num(n).map(|n| n * 16.0)
    } else if let Some(n) = v.strip_suffix("em") {
        num(n).map(|n| n * base)
    } else if let Some(n) = v.strip_suffix('%') {
        num(n).map(|n| n / 100.0 * base)
    } else {
        num(v)
    }
}

fn len_attr(v: &str) -> Option<Len> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        p.trim().parse().ok().map(Len::Pct)
    } else {
        v.trim_end_matches("px").trim().parse().ok().filter(|n: &f32| *n > 0.0).map(Len::Px)
    }
}

fn padding(st: &[(String, String)], cellpadding: f32) -> [f32; 4] {
    let mut p = [cellpadding; 4];
    if let Some(v) = style(st, "padding") {
        let vals: Vec<f32> = v.split_whitespace().filter_map(|x| length(x, 16.0)).collect();
        p = match vals.as_slice() {
            [a] => [*a; 4],
            [a, b] => [*a, *b, *a, *b],
            [a, b, c] => [*a, *b, *c, *b],
            [a, b, c, d, ..] => [*a, *b, *c, *d],
            _ => p,
        };
    }
    for (i, k) in ["padding-top", "padding-right", "padding-bottom", "padding-left"].iter().enumerate() {
        if let Some(v) = style(st, k).and_then(|v| length(v, 16.0)) {
            p[i] = v;
        }
    }
    p.map(|v| v.clamp(0.0, 64.0))
}

fn align_of(v: &str) -> Option<Align> {
    match v.trim() {
        "center" | "middle" => Some(Align::Center),
        "right" | "end" => Some(Align::Right),
        "left" | "start" | "justify" => Some(Align::Left),
        _ => None,
    }
}

fn box_style(h: &Handle, st: &[(String, String)], ctx: &mut Ctx, cellpadding: f32) -> BoxStyle {
    let mut b = BoxStyle { align: ctx.align, ..BoxStyle::default() };
    if let Some(a) = attr(h, "align").as_deref().and_then(align_of).or_else(|| style(st, "text-align").and_then(align_of)) {
        b.align = a;
        ctx.align = a;
    }
    b.bg = style(st, "background-color")
        .or_else(|| style(st, "background").and_then(|v| v.split_whitespace().find(|t| t.starts_with('#') || t.starts_with("rgb"))))
        .and_then(parse_color)
        .or_else(|| attr(h, "bgcolor").as_deref().and_then(parse_color))
        .filter(|c| c[3] > 0);
    b.pad = padding(st, cellpadding);
    b.width = style(st, "max-width").and_then(len_attr).or_else(|| style(st, "width").and_then(len_attr)).or_else(|| attr(h, "width").as_deref().and_then(len_attr));
    b.radius = style(st, "border-radius").and_then(|v| v.split_whitespace().next().and_then(|x| length(x, 16.0))).unwrap_or(0.0).min(24.0);
    if let Some(v) = style(st, "border").or_else(|| style(st, "border-top")) {
        let w = v.split_whitespace().find_map(|t| length(t, 16.0)).unwrap_or(0.0);
        let c = v.split_whitespace().find_map(parse_color);
        if w > 0.0 && !v.contains("none") {
            b.border = c.map(|c| (w.min(4.0), c));
        }
    }
    b.top = attr(h, "valign").is_some_and(|v| v.eq_ignore_ascii_case("top")) || style(st, "vertical-align") == Some("top");
    b.tight = attr(h, "nowrap").is_some() || style(st, "white-space") == Some("nowrap");
    b
}

fn inline_style(h: &Handle, st: &[(String, String)], ctx: &mut Ctx) {
    if let Some(c) = style(st, "color").and_then(parse_color).or_else(|| attr(h, "color").as_deref().and_then(parse_color)) {
        ctx.color = Some(c);
    }
    let base = ctx.size.unwrap_or(14.0);
    if let Some(s) = style(st, "font-size").and_then(|v| length(v, base)) {
        ctx.size = Some(s.clamp(9.0, 48.0));
    }
    if let Some(w) = style(st, "font-weight") {
        ctx.bold = w == "bold" || w == "bolder" || w.parse::<u32>().is_ok_and(|n| n >= 600);
    }
    if style(st, "font-style") == Some("italic") {
        ctx.italic = true;
    }
    if let Some(d) = style(st, "text-decoration").or_else(|| style(st, "text-decoration-line")) {
        ctx.underline = d.contains("underline");
    }
    if let Some(a) = style(st, "text-align").and_then(align_of) {
        ctx.align = a;
    }
    if style(st, "font-family").is_some_and(|f| f.contains("mono") || f.contains("courier") || f.contains("consolas")) {
        ctx.mono = true;
    }
}

fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{034f}' | '\u{feff}' | '\u{00ad}' | '\u{2060}'))
        .map(|c| if c == '\u{a0}' { ' ' } else { c })
        .collect()
}

fn push_text(para: &mut Para, ctx: &Ctx, text: &str) {
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
    if para.runs.is_empty() {
        para.align = ctx.align;
    }
    let run = Run {
        text: String::new(),
        bold: ctx.bold,
        italic: ctx.italic,
        underline: ctx.underline,
        mono: ctx.mono,
        size: ctx.size,
        color: ctx.color,
        bg: None,
        href: ctx.href.clone(),
    };
    match para.runs.last_mut() {
        Some(last) if Run { text: String::new(), ..last.clone() } == run => last.text.push_str(&text),
        _ => para.runs.push(Run { text, ..run }),
    }
}

fn flush(out: &mut Vec<Node>, para: &mut Para) {
    let mut p = std::mem::take(para);
    // Trim whitespace at the ends.
    while p.runs.first().is_some_and(|r| r.text.trim().is_empty() && !r.text.contains('\n')) {
        p.runs.remove(0);
    }
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
const SKIP: &[&str] = &["head", "style", "script", "title", "meta", "link", "noscript", "template", "svg", "map", "object", "iframe", "video", "audio", "input", "select", "textarea"];

impl Builder {
    fn children(&mut self, h: &Handle, ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        for c in h.children.borrow().iter() {
            self.node(c, ctx, out, para);
        }
    }

    fn node(&mut self, h: &Handle, ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        match &h.data {
            NodeData::Text { contents } => push_text(para, ctx, &contents.borrow()),
            NodeData::Element { .. } => self.element(h, ctx, out, para),
            NodeData::Document => self.children(h, ctx, out, para),
            _ => {}
        }
    }

    fn element(&mut self, h: &Handle, parent: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let Some(t) = tag(h) else { return };
        if SKIP.contains(&t.as_str()) {
            return;
        }
        let st = styles(h);
        if hidden(h, &st) {
            return;
        }
        if t == "body"
            && let Some(bg) = style(&st, "background-color").and_then(parse_color).or_else(|| attr(h, "bgcolor").as_deref().and_then(parse_color))
        {
            self.doc.background = Some(bg);
        }
        self.depth += 1;
        if self.depth > 200 {
            self.depth -= 1;
            return;
        }
        let mut ctx = parent.clone();
        inline_style(h, &st, &mut ctx);
        match t.as_str() {
            "br" => push_text(para, &Ctx { pre: true, ..ctx }, "\n"),
            "b" | "strong" => {
                ctx.bold = true;
                self.children(h, &ctx, out, para);
            }
            "i" | "em" | "cite" => {
                ctx.italic = true;
                self.children(h, &ctx, out, para);
            }
            "u" | "ins" => {
                ctx.underline = true;
                self.children(h, &ctx, out, para);
            }
            "code" | "tt" | "kbd" | "samp" => {
                ctx.mono = true;
                self.children(h, &ctx, out, para);
            }
            "small" => {
                ctx.size = Some(ctx.size.unwrap_or(14.0) * 0.85);
                self.children(h, &ctx, out, para);
            }
            "big" => {
                ctx.size = Some(ctx.size.unwrap_or(14.0) * 1.2);
                self.children(h, &ctx, out, para);
            }
            "font" => {
                if let Some(s) = attr(h, "size").and_then(|s| s.trim().parse::<i32>().ok()) {
                    ctx.size = Some([10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0][(s.clamp(1, 7) - 1) as usize]);
                }
                self.children(h, &ctx, out, para);
            }
            "a" => {
                let href = attr(h, "href").filter(|u| !u.trim().is_empty() && !u.starts_with('#') && !u.to_lowercase().starts_with("javascript:"));
                ctx.href = href.clone().or(ctx.href);
                let bs = box_style(h, &st, &mut ctx.clone(), 0.0);
                // A link drawn as a button: its own coloured box.
                if bs.bg.is_some() && (bs.pad != [0.0; 4] || style(&st, "display").is_some_and(|d| d.contains("block"))) {
                    flush(out, para);
                    let mut inner = Vec::new();
                    let mut p = Para::default();
                    self.children(h, &ctx, &mut inner, &mut p);
                    flush(&mut inner, &mut p);
                    if !inner.is_empty() {
                        out.push(Node::Box { style: BoxStyle { href, tight: true, ..bs }, children: inner });
                    }
                } else {
                    self.children(h, &ctx, out, para);
                }
            }
            "img" => self.image(h, &st, &ctx, out, para),
            "hr" => {
                flush(out, para);
                out.push(Node::Rule);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                flush(out, para);
                let lvl = t.as_bytes()[1] - b'0';
                ctx.bold = true;
                if ctx.size.is_none() || style(&st, "font-size").is_none() {
                    ctx.size = Some([26.0, 22.0, 18.0, 16.0, 14.0, 13.0][(lvl as usize - 1).min(5)]);
                }
                self.block(h, &st, &mut ctx, out, 0.0);
            }
            "ul" | "ol" => {
                flush(out, para);
                let mut items = Vec::new();
                for c in h.children.borrow().iter() {
                    if tag(c).as_deref() == Some("li") {
                        let mut v = Vec::new();
                        let mut p = Para::default();
                        let mut cctx = ctx.clone();
                        inline_style(c, &styles(c), &mut cctx);
                        self.children(c, &cctx, &mut v, &mut p);
                        flush(&mut v, &mut p);
                        if !v.is_empty() {
                            items.push(tidy(v));
                        }
                    } else {
                        self.node(c, &ctx, out, para);
                    }
                }
                if !items.is_empty() {
                    out.push(Node::List { ordered: t == "ol", items });
                }
            }
            "blockquote" => {
                flush(out, para);
                let mut v = Vec::new();
                let mut p = Para::default();
                self.children(h, &ctx, &mut v, &mut p);
                flush(&mut v, &mut p);
                if !v.is_empty() {
                    out.push(Node::Quote(tidy(v)));
                }
            }
            "pre" => {
                flush(out, para);
                let mut p = Para::default();
                self.children(h, &Ctx { pre: true, mono: true, ..ctx }, &mut Vec::new(), &mut p);
                let text = p.text();
                if !text.trim().is_empty() {
                    out.push(Node::Pre(text.trim_end().to_owned()));
                }
            }
            "table" => {
                flush(out, para);
                self.table(h, &st, &mut ctx, out);
            }
            "p" => {
                flush(out, para);
                let before = out.len();
                self.block(h, &st, &mut ctx, out, 0.0);
                // Paragraphs keep a little air between them.
                if out.len() > before {
                    out.push(Node::Space(8.0));
                }
            }
            _ if BLOCKS.contains(&t.as_str()) => {
                flush(out, para);
                self.block(h, &st, &mut ctx, out, 0.0);
            }
            _ => self.children(h, &ctx, out, para),
        }
        self.depth -= 1;
    }

    /// A block element: its content, wrapped in a box when it has a visual style of its own.
    fn block(&mut self, h: &Handle, st: &[(String, String)], ctx: &mut Ctx, out: &mut Vec<Node>, cellpadding: f32) {
        let bs = box_style(h, st, ctx, cellpadding);
        let mut inner = Vec::new();
        let mut p = Para::default();
        self.children(h, ctx, &mut inner, &mut p);
        flush(&mut inner, &mut p);
        push_box(out, bs, inner);
    }

    fn image(&mut self, h: &Handle, st: &[(String, String)], ctx: &Ctx, out: &mut Vec<Node>, para: &mut Para) {
        let Some(src) = attr(h, "src").map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) else { return };
        let dim = |a: &str, s: &str| attr(h, a).and_then(|v| v.trim_end_matches("px").trim().parse::<f32>().ok()).or_else(|| style(st, s).and_then(|v| length(v, 16.0)));
        let width = dim("width", "width");
        let height = dim("height", "height");
        // Tracking pixels and spacers.
        if width.is_some_and(|w| w <= 3.0) || height.is_some_and(|h| h <= 3.0) {
            return;
        }
        let lower = src.to_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("cid:")) {
            return;
        }
        flush(out, para);
        if !self.doc.images.contains(&src) {
            self.doc.images.push(src.clone());
        }
        out.push(Node::Image(Img { src, alt: attr(h, "alt").unwrap_or_default().trim().to_owned(), width, height, href: ctx.href.clone() }));
    }

    fn table(&mut self, h: &Handle, st: &[(String, String)], ctx: &mut Ctx, out: &mut Vec<Node>) {
        let cellpadding = attr(h, "cellpadding").and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(0.0).min(32.0);
        // `<table align>` places the table; it doesn't align the text inside it.
        let saved = ctx.align;
        let table_style = box_style(h, st, ctx, 0.0);
        ctx.align = if style(st, "text-align").is_some() { ctx.align } else { saved };
        let mut rows: Vec<Handle> = Vec::new();
        let mut caption = Vec::new();
        for c in h.children.borrow().iter() {
            match tag(c).as_deref() {
                Some("tr") => rows.push(c.clone()),
                Some("tbody") | Some("thead") | Some("tfoot") => {
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
        let mut inner: Vec<Node> = caption;
        for r in rows {
            let rst = styles(&r);
            if hidden(&r, &rst) {
                continue;
            }
            let mut rctx = ctx.clone();
            inline_style(&r, &rst, &mut rctx);
            let row_style = box_style(&r, &rst, &mut rctx, 0.0);
            let mut cells: Vec<(BoxStyle, Vec<Node>)> = Vec::new();
            let mut spacer_height: f32 = 0.0;
            for c in r.children.borrow().iter() {
                let Some(ct) = tag(c) else { continue };
                if ct != "td" && ct != "th" {
                    continue;
                }
                let cst = styles(c);
                if hidden(c, &cst) {
                    continue;
                }
                let mut cctx = rctx.clone();
                if ct == "th" {
                    cctx.bold = true;
                }
                inline_style(c, &cst, &mut cctx);
                let bs = box_style(c, &cst, &mut cctx, cellpadding);
                let mut v = Vec::new();
                let mut p = Para::default();
                self.children(c, &cctx, &mut v, &mut p);
                flush(&mut v, &mut p);
                let v = tidy(v);
                if v.is_empty() && bs.bg.is_none() {
                    let hgt = attr(c, "height").and_then(|x| x.trim_end_matches("px").parse::<f32>().ok()).or_else(|| style(&cst, "height").and_then(|x| length(x, 16.0)));
                    spacer_height = spacer_height.max(hgt.unwrap_or(0.0));
                    // An empty cell with a fixed width is a horizontal gap.
                    if let Some(Len::Px(w)) = bs.width
                        && w <= 80.0
                    {
                        cells.push((BoxStyle { width: Some(Len::Px(w)), tight: true, ..BoxStyle::default() }, Vec::new()));
                    }
                    continue;
                }
                cells.push((bs, v));
            }
            let content_cells = cells.iter().filter(|(_, v)| !v.is_empty() || false).count() + cells.iter().filter(|(s, v)| v.is_empty() && s.bg.is_some()).count();
            if content_cells == 0 {
                if spacer_height > 0.0 {
                    inner.push(Node::Space(spacer_height.min(40.0)));
                }
                continue;
            }
            let row_node = if content_cells == 1 {
                // A one-cell row is just its cell (dropping the gap cells around it).
                let (bs, v) = cells.into_iter().find(|(s, v)| !v.is_empty() || s.bg.is_some()).expect("one content cell");
                let mut tmp = Vec::new();
                push_box(&mut tmp, BoxStyle { width: bs.width.filter(|w| matches!(w, Len::Px(_))), ..bs }, v);
                tmp
            } else {
                vec![Node::Row { cells }]
            };
            push_box(&mut inner, row_style, row_node);
        }
        push_box(out, table_style, tidy(inner));
    }
}

fn push_box(out: &mut Vec<Node>, style: BoxStyle, children: Vec<Node>) {
    if children.is_empty() && style.bg.is_none() {
        return;
    }
    if style.is_plain() {
        // Alignment still matters for images and rows inside.
        if style.align != Align::Left && children.iter().any(|c| matches!(c, Node::Image(_) | Node::Box { .. })) {
            out.push(Node::Box { style, children });
        } else {
            out.extend(children);
        }
    } else if let [Node::Box { style: inner, children: kids }] = children.as_slice()
        && style.bg.is_none()
        && style.border.is_none()
        && inner.width.is_none()
    {
        // Merge a styled box into a padding-only wrapper.
        let mut merged = inner.clone();
        for i in 0..4 {
            merged.pad[i] += style.pad[i];
        }
        merged.width = style.width;
        out.push(Node::Box { style: merged, children: kids.clone() });
    } else {
        out.push(Node::Box { style, children });
    }
}

/// Collapses runs of spacers and drops spacers at the ends.
fn tidy(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(nodes.len());
    for n in nodes {
        if let Node::Space(h) = n {
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
    fn walk(nodes: &[Node], out: &mut String) {
        for n in nodes {
            match n {
                Node::Para(p) => {
                    out.push_str(&p.text());
                    out.push_str("\n\n");
                }
                Node::Box { children, .. } | Node::Quote(children) => walk(children, out),
                Node::Row { cells } => {
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
                Node::Image(_) | Node::Rule | Node::Space(_) => {}
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
    let _ = Rc::new(());
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn find_row(n: &[Node]) -> bool {
            n.iter().any(|n| match n {
                Node::Row { cells } => cells.iter().any(|(s, _)| s.align == Align::Right && s.tight),
                Node::Box { children, .. } => find_row(children),
                _ => false,
            })
        }
        assert!(find_row(&doc.nodes), "{:#?}", doc.nodes);
        fn find_button(n: &[Node]) -> bool {
            n.iter().any(|n| match n {
                Node::Box { style, children } => style.href.is_some() && style.bg == Some([0x63, 0x5b, 0xff, 255]) || find_button(children),
                _ => false,
            })
        }
        assert!(find_button(&doc.nodes));
    }

    #[test]
    fn inline_styles_and_breaks() {
        let doc = parse("<p>Hello <b>bold</b> <a href='https://x.y'>link</a><br>next&nbsp;line</p>");
        let Node::Para(p) = &doc.nodes[0] else { panic!("{:?}", doc.nodes) };
        assert_eq!(p.text(), "Hello bold link\nnext line");
        assert!(p.runs.iter().any(|r| r.bold && r.text == "bold"));
        assert!(p.runs.iter().any(|r| r.href.as_deref() == Some("https://x.y")));
    }
}
