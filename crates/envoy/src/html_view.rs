//! Draws an email's layout tree (`envoy_core::html::Doc`) as Rune elements: styled text runs
//! with links, boxes and buttons, rows of cells, images.
//!
//! Colours adapt to the theme by default: near-white backgrounds drop away, near-black text
//! becomes the theme's ink, dark accents brighten in dark mode. Coloured boxes (buttons,
//! banners) keep their own colours and their text's. "Original" draws the mail as designed
//! on a light page instead.

use std::collections::HashMap;

use rune::{Color, El, Link, Span, Tx, col, el, rich, row, text};

use envoy_core::html::{Align, BoxStyle, Doc, Img, Len, Node, Para, Rgba};

use crate::app::{App, ImageState};
use crate::theme::Theme;

pub struct Ctx<'a> {
    pub t: &'a Theme,
    pub images: &'a HashMap<String, ImageState>,
    pub adapt: bool,
    pub load_images: bool,
    pub copy_links: bool,
    pub max_w: f32,
}

#[derive(Clone, Copy)]
struct Local {
    /// Inside a coloured box: colours are the author's, not adapted.
    on_color: bool,
    /// Inside a box sized by its content: children can't use percentage widths.
    tight: bool,
    /// Default text colour here.
    ink: Color,
}

fn srgb8(r: u8, g: u8, b: u8) -> Color {
    rune::color::hex(&format!("#{r:02x}{g:02x}{b:02x}")).unwrap_or(Color::BLACK)
}

fn rgba(c: Rgba) -> Color {
    srgb8(c[0], c[1], c[2]).alpha(c[3] as f32 / 255.0)
}

fn lab(c: Color) -> (f32, f32) {
    let o = c.to_oklab();
    (o[0], o[1].hypot(o[2]))
}

impl Ctx<'_> {
    fn text_color(&self, c: Option<Rgba>, l: Local) -> Color {
        let Some(c) = c.map(rgba) else { return l.ink };
        if !self.adapt || l.on_color {
            return c;
        }
        let k = &self.t.k;
        let (light, chroma) = lab(c);
        if self.t.dark {
            if chroma < 0.04 {
                // Greys: keep their rank (strong, muted, faint) in the theme's inks.
                return if light < 0.4 { k.c.text_base } else if light < 0.65 { k.c.text_muted } else { k.c.text_faint };
            }
            if light < 0.62 {
                let o = c.to_oklab();
                let scale = 0.78 / light.max(0.01);
                return Color::from_oklab([0.78, o[1] * scale.min(1.4), o[2] * scale.min(1.4), 1.0]);
            }
            c
        } else if light > 0.9 && chroma < 0.04 {
            k.c.text_base
        } else {
            c
        }
    }

    /// A box's fill, or None to let the reader's own background show.
    fn fill(&self, c: Option<Rgba>) -> Option<Color> {
        let c = rgba(c?);
        if !self.adapt {
            return Some(c);
        }
        let (light, chroma) = lab(c);
        if chroma < 0.035 && light > 0.93 {
            return None;
        }
        if chroma < 0.035 && light > 0.8 {
            return Some(self.t.k.c.layer_01);
        }
        Some(c)
    }

    fn ink_on(&self, fill: Color) -> Color {
        if lab(fill).0 > 0.6 { srgb8(0x1f, 0x1f, 0x1f) } else { Color::WHITE }
    }
}

fn link_handler(e: El, copy: bool) -> El {
    e.on_link(move |_: &mut App, url: &str, tx: &mut Tx| {
        if copy {
            tx.copy(url.to_owned());
        } else {
            let _ = tx.open_url(url);
        }
    })
}

fn para(cx: &Ctx, p: &Para, l: Local) -> El {
    let mut content = String::new();
    let mut spans = Vec::new();
    let mut links = Vec::new();
    let base = p.runs.iter().filter_map(|r| r.size).fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s)))).unwrap_or(14.0).clamp(10.0, 40.0);
    for r in &p.runs {
        let start = content.len();
        content.push_str(&r.text);
        let end = content.len();
        if start == end {
            continue;
        }
        let color = if r.href.is_some() && r.color.is_none() { cx.t.accent } else { cx.text_color(r.color, l) };
        let mut s = Span::new(start, end).color(color).weight(if r.bold { 650.0 } else { 400.0 });
        if let Some(size) = r.size {
            s = s.size(size.clamp(9.0, 48.0));
        }
        if r.italic {
            s = s.italic();
        }
        if r.mono {
            s = s.mono();
        }
        if r.underline || (r.href.is_some() && r.color.is_none()) {
            s = s.underline(color.alpha(0.6));
        }
        spans.push(s);
        if let Some(h) = &r.href {
            links.push(Link::new(start..end, h.clone()));
        }
    }
    let align = match p.align {
        Align::Left => rune::Align::Start,
        Align::Center => rune::Align::Center,
        Align::Right => rune::Align::End,
    };
    let mut e = rich(content, spans).font(rune::TextStyle::new(base, 400.0).lh((base * 1.45).round())).wrap().selectable(true).align(align);
    if !l.tight {
        e = e.w_full();
    }
    for link in links {
        e = e.link(link);
    }
    if e.link_url().is_none() {
        e = link_handler(e, cx.copy_links);
    }
    e
}

fn image(cx: &Ctx, img: &Img, l: Local) -> El {
    let k = &cx.t.k;
    let state = cx.images.get(&img.src);
    let mut e = match state {
        Some(ImageState::Ready(i)) => {
            let [pw, ph] = i.size().unwrap_or([img.width.unwrap_or(100.0) as u32, img.height.unwrap_or(100.0) as u32]);
            let ratio = pw.max(1) as f32 / ph.max(1) as f32;
            let w = img.width.unwrap_or(pw as f32).min(cx.max_w);
            let e = el().pixels(i.pixels());
            // Small images get an exact box; wide ones fill the column up to their width.
            if l.tight || w <= 320.0 { e.w(w).h(w / ratio) } else { e.w_full().max_w(w).aspect_ratio(ratio) }
        }
        Some(ImageState::Loading) if cx.load_images => {
            let (w, h) = (img.width.unwrap_or(0.0).min(cx.max_w), img.height.unwrap_or(0.0));
            if w > 0.0 && h > 0.0 {
                el().w(w).h(h * (w / img.width.unwrap_or(w).max(1.0))).radius(4.0).fill(k.c.layer_01)
            } else {
                el()
            }
        }
        _ => {
            if img.alt.is_empty() || img.alt.len() > 120 {
                return el();
            }
            text(format!("[{}]", img.alt)).font(rune::TextStyle::new(12.0, 400.0).italic()).color(k.c.text_faint)
        }
    };
    if let Some(h) = &img.href {
        e = e.href(h.clone()).cursor(rune::Cursor::Pointer);
    }
    e
}

fn items(a: Align) -> fn(El) -> El {
    match a {
        Align::Left => |e: El| e.items_start(),
        Align::Center => |e: El| e.items_center(),
        Align::Right => |e: El| e.items_end(),
    }
}

fn styled_box(cx: &Ctx, s: &BoxStyle, children: &[Node], l: Local) -> El {
    let fill = cx.fill(s.bg);
    // A box that keeps its author's colour keeps its text's colours too; a light box that
    // was dropped or mapped to the theme hands its content back to the theme.
    let authored = cx.adapt && fill.is_some() && s.bg.is_some_and(|c| lab(rgba(c)).1 >= 0.035 || lab(rgba(c)).0 < 0.8);
    let (on_color, ink) = if !cx.adapt || s.bg.is_none() {
        (l.on_color, l.ink)
    } else if authored {
        (true, cx.ink_on(fill.unwrap_or(Color::WHITE)))
    } else {
        (false, cx.t.k.c.text_base)
    };
    let inner = Local { on_color, tight: s.tight || l.tight, ink };
    let mut e = items(s.align)(col()).pt(s.pad[0]).pr(s.pad[1]).pb(s.pad[2]).pl(s.pad[3]);
    for n in children {
        e = e.child(node(cx, n, inner));
    }
    if let Some(f) = fill {
        e = e.fill(f);
    }
    if s.radius > 0.0 {
        e = e.radius(s.radius);
    }
    if let Some((w, c)) = s.border {
        let c = if cx.adapt && lab(rgba(c)).1 < 0.035 { cx.t.k.c.border_base } else { rgba(c) };
        e = e.border(w, c);
    }
    if !s.tight && !l.tight {
        e = match s.width {
            Some(Len::Px(w)) => e.w_full().max_w(w),
            Some(Len::Pct(p)) if p < 95.0 => e.w_frac((p / 100.0).clamp(0.05, 1.0)),
            _ => e.w_full(),
        };
    }
    if let Some(h) = &s.href {
        e = e.href(h.clone()).cursor(rune::Cursor::Pointer).hover(Color::WHITE.alpha(0.08)).press(Color::BLACK.alpha(0.08));
    }
    e
}

fn cells(cx: &Ctx, cells: &[(BoxStyle, Vec<Node>)], l: Local) -> El {
    let mut r = row().w_full().gap(0.0);
    for (s, kids) in cells {
        let mut c = styled_box(cx, &BoxStyle { width: None, ..s.clone() }, kids, Local { tight: false, ..l });
        c = match s.width {
            Some(Len::Px(w)) if kids.is_empty() => c.w(w.min(40.0)),
            Some(Len::Px(w)) => c.w(w).shrink(),
            Some(Len::Pct(p)) => c.w_frac((p / 100.0).clamp(0.02, 1.0)).shrink(),
            None if s.tight => c,
            None => c.grow(1.0).min_w(0.0),
        };
        c = if s.top { c.self_start() } else { c.self_center() };
        r = r.child(c);
    }
    r
}

fn node(cx: &Ctx, n: &Node, l: Local) -> El {
    let k = &cx.t.k;
    match n {
        Node::Para(p) => para(cx, p, l),
        Node::Box { style, children } => styled_box(cx, style, children, l),
        Node::Row { cells: c } => cells(cx, c, l),
        Node::Image(img) => image(cx, img, l),
        Node::Rule => el().h(1.0).w_full().my(10.0).fill(k.c.border_muted),
        Node::Space(h) => el().h(h.min(40.0)),
        Node::List { ordered, items } => {
            let mut c = col().w_full().gap(4.0).pl(4.0).my(4.0);
            for (i, it) in items.iter().enumerate() {
                let bullet = if *ordered { format!("{}.", i + 1) } else { "•".to_owned() };
                let mut body = col().grow(1.0).min_w(0.0);
                for n in it {
                    body = body.child(node(cx, n, l));
                }
                c = c.child(row().w_full().gap(8.0).items_start().child(text(bullet).font(rune::TextStyle::new(14.0, 400.0).lh(20.0)).color(l.ink)).child(body));
            }
            c
        }
        Node::Quote(kids) => {
            let mut c = col().w_full().pl(12.0).my(4.0).border_sides([2.0, 0.0, 0.0, 0.0], k.c.border_strong);
            for n in kids {
                c = c.child(node(cx, n, Local { ink: k.c.text_muted, ..l }));
            }
            c
        }
        Node::Pre(t) => text(t.clone())
            .font(rune::TextStyle::new(12.5, 400.0).mono().lh(19.0))
            .color(l.ink)
            .wrap()
            .selectable(true)
            .w_full()
            .padding(10.0)
            .radius(6.0)
            .fill(k.c.layer_01),
    }
}

pub fn render(cx: &Ctx, doc: &Doc) -> El {
    let original = !cx.adapt;
    let ink = if original { srgb8(0x22, 0x22, 0x22) } else { cx.t.k.c.text_base };
    let l = Local { on_color: false, tight: false, ink };
    let mut c = col().key("html").w_full().max_w(cx.max_w);
    for n in &doc.nodes {
        c = c.child(node(cx, n, l));
    }
    if original {
        let page = doc.background.map(rgba).unwrap_or(Color::WHITE);
        col().w_full().max_w(cx.max_w + 32.0).padding(16.0).radius(12.0).fill(page).child(c)
    } else {
        c
    }
}
