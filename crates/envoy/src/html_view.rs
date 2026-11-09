//! Draws an email's layout tree (`envoy_core::html::Doc`) as Rune elements: styled text runs
//! with links, boxes and buttons with borders and background pictures, table rows on their
//! column grid, fluid columns that wrap, images.
//!
//! Nothing reaches past the reader, whatever widths the mail asks for: the tree is drawn at
//! the width the reader has ([`Ctx::width`]), and each box knows the room it gets from it
//! (`Local::room`). Fixed widths, minimums, pictures and padding are capped at that room, as
//! `max-width: 100%` does in mail clients; text breaks inside a word that's longer than its
//! line (long links); fluid columns stack when the message is narrow, as responsive mail does
//! on phones. The tree itself doesn't change with the width: dragging the divider rebuilds
//! these elements, never the tree.
//!
//! Colours adapt to the theme by default: near-white backgrounds drop away, near-black text
//! becomes the theme's ink, dark accents brighten in dark mode. Coloured boxes (buttons,
//! banners, pictures) keep their own colours and their text's. "Original" draws the mail as
//! designed on a light page instead.

use std::collections::HashMap;

use rune::{Color, El, Link, Span, Tx, col, el, rich, row, text};

use envoy_core::html::{Align, BoxStyle, Doc, Family, Img, Len, LineHeight, Node, Para, Rgba, RowGrid, VAlign};

use crate::app::{App, ImageState};
use crate::theme::Theme;

pub struct Ctx<'a> {
    pub t: &'a Theme,
    pub images: &'a HashMap<String, ImageState>,
    pub adapt: bool,
    pub load_images: bool,
    pub copy_links: bool,
    /// `reading.maxWidth`: the widest a message is drawn.
    pub max_w: f32,
    /// The width the reader gives the message, in points, as last laid out (up to a few points
    /// under: it's read in steps). What the mail sizes is fitted to it.
    pub width: f32,
}

impl Ctx<'_> {
    /// The width the message's content is laid out at: the reader's, up to `reading.maxWidth`,
    /// less the page's margin when it's drawn on a page of its own ("Original").
    fn content_width(&self) -> f32 {
        if self.adapt { self.width.min(self.max_w) } else { self.width.min(self.max_w + 2.0 * PAGE_PAD) - 2.0 * PAGE_PAD }.max(0.0)
    }
}

/// The margin round mail drawn as designed, on a light page of its own.
const PAGE_PAD: f32 = 16.0;

/// Fluid columns stack, one under another, when the message is laid out narrower than this:
/// two columns of 240 points, where MJML and Cerberus templates switch to one column.
const STACK_BELOW: f32 = 480.0;

/// Fluid columns also stack when one of them would be narrower than this.
const MIN_COLUMN: f32 = 120.0;

/// The most of a box's room its side padding (or margins) may take: past it, both sides shrink
/// in proportion, so a padded box in a narrow column still has room for its content.
const MAX_INSET: f32 = 1.0 / 3.0;

/// The serif face for `font-family: Georgia, serif` and kin: one every install of the OS has
/// (Rune falls back to the sans face when it's missing).
#[cfg(any(target_os = "macos", target_os = "windows"))]
const SERIF: &str = "Georgia";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const SERIF: &str = "DejaVu Serif";

#[derive(Clone, Copy)]
struct Local {
    /// Inside a coloured box: colours are the author's, not adapted.
    on_color: bool,
    /// Inside a box sized by its content: children can't use percentage widths.
    tight: bool,
    /// Default text colour here.
    ink: Color,
    /// The email's own background behind this point (before adapting), if it set one.
    under: Option<Rgba>,
    /// The widest the content here can be, in points: the message's width less the padding,
    /// margins and column shares around it. Fixed sizes are capped at it.
    room: f32,
}

/// Text beside the message's own, a list's marker or a picture's description: not selected or
/// copied with the message. The overflow guard measures text by its lines, which only
/// selectable text exposes, so under test it's selectable too (selecting never changes layout).
fn aside(e: El) -> El {
    e.selectable(cfg!(test))
}

/// Left and right insets (padding or margins) fitted to `room`: as given, or shrunk in
/// proportion to [`MAX_INSET`] of it.
fn fit_sides(left: f32, right: f32, room: f32) -> (f32, f32) {
    let (sum, most) = (left + right, room.max(0.0) * MAX_INSET);
    if sum > most && sum > 0.0 { (left * most / sum, right * most / sum) } else { (left, right) }
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

    fn picture(&self, url: &Option<String>) -> Option<&rune::Pixels> {
        match self.images.get(url.as_deref()?) {
            Some(ImageState::Ready(i)) => Some(i.pixels()),
            _ => None,
        }
    }
}

/// What pressing a link in mail does: open it, or copy it with `behavior.copyLinks`.
fn follow(url: &str, copy: bool, tx: &mut Tx) {
    if copy {
        tx.copy(url.to_owned());
    } else {
        let _ = tx.open_url(url);
    }
}

fn link_handler(e: El, copy: bool) -> El {
    e.on_link(move |_: &mut App, url: &str, tx: &mut Tx| follow(url, copy, tx))
}

/// A box or picture that's a link: announced as one, and pressed as text links are
/// ([`follow`]). The only place mail sets `href` on an element: `El::href` alone always opens.
fn linked(e: El, url: &str, copy: bool) -> El {
    let u = url.to_owned();
    e.href(url.to_owned()).cursor(rune::Cursor::Pointer).on_press_tx(move |_: &mut App, tx: &mut Tx| follow(&u, copy, tx))
}

fn weight(w: u16) -> f32 {
    if w >= 600 {
        650.0
    } else if w >= 500 {
        520.0
    } else {
        400.0
    }
}

fn para(cx: &Ctx, p: &Para, l: Local) -> El {
    let mut content = String::new();
    let mut spans = Vec::new();
    let mut links = Vec::new();
    let mut strikes = Vec::new();
    let base = p.runs.iter().filter_map(|r| r.size).fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s)))).unwrap_or(14.0).clamp(9.0, 64.0);
    for r in &p.runs {
        let start = content.len();
        content.push_str(&r.text);
        let end = content.len();
        if start == end {
            continue;
        }
        let color = if r.href.is_some() && r.color.is_none() { cx.t.accent } else { cx.text_color(r.color, l) };
        let mut s = Span::new(start, end).color(color).weight(weight(r.weight));
        if let Some(size) = r.size {
            s = s.size(size.clamp(9.0, 64.0));
        }
        if r.italic {
            s = s.italic();
        }
        if r.mono && p.family != Family::Mono {
            s = s.mono();
        }
        if r.underline {
            s = s.underline(color.alpha(0.6));
        }
        if r.strike {
            strikes.push((start, end, Some(color)));
        }
        if let Some(bg) = r.bg.and_then(|c| if l.on_color { Some(rgba(c)) } else { cx.fill(Some(c)) }) {
            s = s.bg(bg, 2.0);
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
    let lh = match p.line_height {
        Some(LineHeight::Mult(m)) => base * m,
        Some(LineHeight::Px(px)) => px,
        None => base * 1.45,
    }
    .clamp(base, base * 3.0)
    .round();
    let mut font = rune::TextStyle::new(base, 400.0).lh(lh).tracking(p.tracking);
    font = match p.family {
        Family::Sans => font,
        Family::Serif => font.family(rune::Family::Named(SERIF)),
        Family::Mono => font.mono(),
    };
    // A word longer than its line (a long link) breaks inside rather than running off.
    let mut e = rich(content, spans).font(font).wrap_anywhere().selectable(true).align(align);
    if !strikes.is_empty() {
        e = e.strikes(strikes);
    }
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
    let room = l.room.max(1.0);
    let mut e = match state {
        Some(ImageState::Ready(i)) => {
            let [pw, ph] = i.size().unwrap_or([img.width.unwrap_or(100.0) as u32, img.height.unwrap_or(100.0) as u32]);
            let ratio = pw.max(1) as f32 / ph.max(1) as f32;
            let w = img.width.unwrap_or(pw as f32).min(cx.max_w);
            let e = el().pixels(i.pixels());
            // In a box sized by its content an image takes its own size, scaled down to the
            // room it has; elsewhere it fills the column up to its width, scaling down when
            // the column is narrower.
            if l.tight {
                let w = w.min(room);
                e.w(w).h(w / ratio)
            } else {
                e.w_full().max_w(w).aspect_ratio(ratio)
            }
        }
        Some(ImageState::Loading) if cx.load_images => {
            let (w, h) = (img.width.unwrap_or(0.0).min(cx.max_w), img.height.unwrap_or(0.0));
            if w > 0.0 && h > 0.0 {
                let e = el().radius(4.0).fill(k.c.layer_01);
                if l.tight {
                    let fit = w.min(room);
                    e.w(fit).h(fit * h / w)
                } else {
                    e.w_full().max_w(w).aspect_ratio(w / h)
                }
            } else {
                el()
            }
        }
        _ => {
            if img.alt.is_empty() || img.alt.len() > 120 {
                return el();
            }
            // The picture's description in its place, kept to the picture's width and the
            // room it has: two lines at most, ending in "…" when it's longer.
            let w = img.width.map_or(room, |w| w.max(48.0).min(room));
            aside(text(format!("[{}]", img.alt)).font(rune::TextStyle::new(12.0, 400.0).italic()).color(k.c.text_faint).wrap_anywhere().max_lines(2).max_w(w))
        }
    };
    if let Some(h) = &img.href {
        e = linked(e, h, cx.copy_links);
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

/// A box's own look and content; its width and margins are the caller's, and `l.room` is the
/// widest the box can be.
fn styled_box(cx: &Ctx, s: &BoxStyle, children: &[Node], l: Local) -> El {
    let picture = cx.picture(&s.bg_image);
    let own = s.bg.map(rgba);
    // A box that keeps its author's colour (or picture) keeps its text's colours too, and so
    // does everything inside it; a light box that was dropped or mapped to the theme hands
    // its content back to the theme.
    let (fill, on_color, ink) = if !cx.adapt {
        (own, l.on_color, l.ink)
    } else if l.on_color {
        (own, true, own.map_or(l.ink, |f| cx.ink_on(f)))
    } else if picture.is_some() {
        (own, true, cx.ink_on(own.unwrap_or(Color::BLACK)))
    } else if let Some(c) = s.bg {
        let mut fill = cx.fill(Some(c));
        // A near-white panel on a whiter card (an info box) stays a panel, in the theme's tone.
        if fill.is_none() && l.under.is_some_and(|u| lab(rgba(u)).0 > lab(rgba(c)).0 + 0.01) {
            fill = Some(cx.t.k.c.layer_01);
        }
        let (light, chroma) = lab(rgba(c));
        if fill.is_some() && (chroma >= 0.035 || light < 0.8) { (fill, true, cx.ink_on(fill.unwrap_or(Color::BLACK))) } else { (fill, false, cx.t.k.c.text_base) }
    } else {
        (None, false, l.ink)
    };
    let (pl, pr) = fit_sides(s.pad[3], s.pad[1], l.room);
    let inner = Local { on_color, tight: s.tight || l.tight, ink, under: s.bg.or(l.under), room: (l.room - pl - pr).max(0.0) };
    let mut e = items(s.align)(col()).pt(s.pad[0]).pr(pr).pb(s.pad[2]).pl(pl);
    e = match s.valign {
        VAlign::Top => e.justify_start(),
        VAlign::Middle => e.justify_center(),
        VAlign::Bottom => e.justify_end(),
    };
    for n in children {
        e = e.child(node(cx, n, inner));
    }
    if let Some(f) = fill {
        e = e.fill(f);
    }
    if let Some(p) = picture {
        e = e.pixels_fit(p, rune::SurfaceFit::Fill);
    }
    if s.radius > 0.0 {
        e = e.radius(s.radius);
    }
    if let Some(b) = &s.border {
        let c = if cx.adapt && !on_color && lab(rgba(b.color)).1 < 0.035 { cx.t.k.c.border_base } else { rgba(b.color) };
        let w = b.widths;
        e = if w.iter().all(|x| *x == w[0]) { e.border(w[0], c) } else { e.border_sides(w, c) };
        if b.dashed {
            let max = w.iter().fold(0.0f32, |a, x| a.max(*x));
            e = e.border_dash((max * 3.0).max(3.0), (max * 2.0).max(2.0));
        }
    }
    if let Some(h) = &s.href {
        e = linked(e, h, cx.copy_links).hover(Color::WHITE.alpha(0.08)).press(Color::BLACK.alpha(0.08));
    }
    e
}

/// A box in a column of blocks: its width, then its margins.
fn block_box(cx: &Ctx, s: &BoxStyle, children: &[Node], l: Local) -> El {
    let tight = s.tight || l.tight;
    let (ml, mr) = fit_sides(s.margin[3], s.margin[1], l.room);
    let outer = (l.room - ml - mr).max(0.0);
    // The widest the box can be: its own width, capped by its maximum and the room it has.
    let own = match s.width {
        Some(Len::Px(w)) => w.min(outer),
        Some(Len::Pct(p)) if !tight && p < 99.5 => outer * (p / 100.0).clamp(0.05, 1.0),
        _ => outer,
    }
    .min(s.max_width.unwrap_or(f32::MAX))
    .max(s.min_width.min(outer));
    let mut e = styled_box(cx, s, children, Local { room: own, ..l });
    if tight {
        if let Some(Len::Px(w)) = s.width {
            e = e.w_full().max_w(w);
        }
    } else {
        e = match s.width {
            Some(Len::Px(w)) => e.w_full().max_w(w),
            Some(Len::Pct(p)) if p < 99.5 => e.w_frac((p / 100.0).clamp(0.05, 1.0)),
            _ => e.w_full(),
        };
    }
    if let Some(m) = s.max_width {
        e = e.max_w(m.min(match s.width {
            Some(Len::Px(w)) => w,
            _ => f32::MAX,
        }));
    }
    if s.min_width > 0.0 {
        e = e.min_w(s.min_width.min(outer));
    }
    if s.margin != [0.0; 4] {
        // Margins as padding round it, so a full-width box with margins still fits.
        let wrap = items(s.align)(col()).pt(s.margin[0]).pr(mr).pb(s.margin[2]).pl(ml);
        e = if tight { wrap.child(e) } else { wrap.w_full().child(e) };
    }
    e
}

fn cells(cx: &Ctx, cells: &[(BoxStyle, Vec<Node>)], gap: f32, grid: Option<&RowGrid>, l: Local) -> El {
    // A row the email stacks on phones, in a narrow pane: its cells one under another.
    let filled = || cells.iter().filter(|(_, kids)| !kids.is_empty());
    if cx.content_width() < STACK_BELOW && filled().count() > 1 && filled().all(|(s, _)| s.stack) {
        let mut c = col().w_full().gap(gap);
        for (s, kids) in filled() {
            c = c.child(styled_box(cx, s, kids, Local { tight: false, ..l }).w_full());
        }
        return c;
    }
    // A table's row: its columns' widths at this room, the same for every row of the table, so
    // cells line up down it whatever each row holds. Each is drawn as its fraction of the room:
    // the room is read in steps, a few points under the row's real width, and every row scales
    // up to that alike, filling it.
    if let Some(g) = grid.filter(|g| !l.tight && g.spans.len() == cells.len()) {
        let widths = g.grid.widths(l.room, gap);
        let mut r = row().w_full().gap(gap).items_stretch();
        for ((s, kids), &(at, span)) in cells.iter().zip(&g.spans) {
            let (a, b) = ((at as usize).min(widths.len()), ((at + span) as usize).min(widths.len()));
            let w = widths[a..b].iter().sum::<f32>() + gap * b.saturating_sub(a + 1) as f32;
            let share = if l.room > 0.0 { w / l.room } else { 0.0 };
            r = r.child(styled_box(cx, s, kids, Local { tight: false, room: w, ..l }).w_frac(share).shrink());
        }
        return r;
    }
    // A row sized by its content (a menu, a button row) or without a table's grid: the room
    // the cells share, and their minimums (small fixed columns, longest words) as given while
    // they all fit, so a cell may take more than its share at the expense of the others;
    // scaled down together when they don't.
    let free = (l.room - gap * cells.len().saturating_sub(1) as f32).max(0.0);
    let mins: f32 = cells.iter().map(|(s, _)| s.min_width.min(free)).sum();
    let scale = if mins > free && mins > 0.0 { free / mins } else { 1.0 };
    // Cells stretch to the row's height, as table cells do; `valign` places their content.
    let mut r = row().w_full().gap(gap).items_stretch();
    for (s, kids) in cells {
        let min = s.min_width.min(free) * scale;
        let w = match s.width {
            Some(Len::Px(w)) if kids.is_empty() => w.min(40.0).min(free),
            Some(Len::Px(w)) => w.min(free),
            Some(Len::Pct(p)) => l.room * (p / 100.0).clamp(0.01, 1.0),
            None => free,
        }
        .max(min)
        .min(free);
        let mut c = styled_box(cx, s, kids, Local { tight: false, room: w, ..l });
        c = match s.width {
            Some(Len::Px(w)) if kids.is_empty() => c.w(w.min(40.0)).shrink(),
            Some(Len::Px(w)) => c.w(w).shrink(),
            // In a box sized by its content (a menu, a button row), shares would be of a width
            // taken from the content: too narrow by the padding. Cells take their content's.
            Some(Len::Pct(_)) if l.tight => c.shrink(),
            Some(Len::Pct(p)) => c.w_frac((p / 100.0).clamp(0.01, 1.0)).shrink(),
            None if s.tight => c,
            None => c.grow(1.0).min_w(0.0),
        };
        if min > 0.0 {
            c = c.min_w(min);
        }
        r = r.child(c);
    }
    r
}

/// Whether a row of fluid columns stands one under another: when the message is narrow (as
/// responsive mail's phone layout does), or one column would be too narrow to read. Columns
/// of fixed widths wrap by themselves when they don't fit.
fn stacks(cx: &Ctx, cells: &[(BoxStyle, Vec<Node>)], room: f32) -> bool {
    let shares = || cells.iter().filter_map(|(s, _)| match s.width {
        Some(Len::Pct(p)) if p < 99.5 => Some(room * p / 100.0),
        _ => None,
    });
    cells.len() > 1 && shares().next().is_some() && (cx.content_width() < STACK_BELOW || shares().any(|w| w < MIN_COLUMN))
}

/// The widest a column can be in `room`: its width, within its minimum and maximum.
fn column_width(s: &BoxStyle, room: f32) -> f32 {
    match s.width {
        Some(Len::Pct(p)) => room * (p / 100.0).clamp(0.05, 1.0),
        Some(Len::Px(w)) => w.min(room),
        None => room,
    }
    .min(s.max_width.unwrap_or(f32::MAX))
    .max(s.min_width.min(room))
}

fn columns(cx: &Ctx, cells: &[(BoxStyle, Vec<Node>)], align: Align, rtl: bool, l: Local) -> El {
    let room = l.room;
    // Right-to-left columns that won't fit side by side stack in source order, as they do in
    // browsers (a row reversed would wrap last column first).
    let wraps = rtl && cells.iter().map(|(s, _)| column_width(s, room)).sum::<f32>() > room + 0.5;
    if wraps || stacks(cx, cells, room) {
        let mut c = items(align)(col()).w_full();
        for (s, kids) in cells {
            let own = room.min(s.max_width.unwrap_or(f32::MAX));
            let mut e = styled_box(cx, s, kids, Local { tight: false, room: own, ..l }).w_full();
            if let Some(m) = s.max_width {
                e = e.max_w(m);
            }
            c = c.child(e);
        }
        return c;
    }
    let mut r = row().w_full().flex_wrap().items_start();
    r = match align {
        Align::Left => r.justify_start(),
        Align::Center => r.justify_center(),
        Align::Right => r.justify_end(),
    };
    let ordered: Box<dyn Iterator<Item = &(BoxStyle, Vec<Node>)>> = if rtl { Box::new(cells.iter().rev()) } else { Box::new(cells.iter()) };
    for (s, kids) in ordered {
        let mut c = styled_box(cx, s, kids, Local { tight: false, room: column_width(s, room), ..l });
        // A hair under the exact share, so columns that add up to the row stay on one line.
        c = match s.width {
            Some(Len::Pct(p)) => c.w_frac((p / 100.0 - 0.001).clamp(0.05, 1.0)),
            Some(Len::Px(w)) => c.w(w.min(room)),
            None => c.w_full(),
        };
        if let Some(m) = s.max_width {
            c = c.max_w((m - 0.5).max(1.0));
        }
        if s.min_width > 0.0 {
            c = c.min_w(s.min_width.min(room));
        }
        c = match s.valign {
            VAlign::Top => c.self_start(),
            VAlign::Middle => c.self_center(),
            VAlign::Bottom => c.self_end(),
        };
        r = r.child(c);
    }
    r
}

fn node(cx: &Ctx, n: &Node, l: Local) -> El {
    let k = &cx.t.k;
    match n {
        Node::Para(p) => para(cx, p, l),
        Node::Box { style, children } => block_box(cx, style, children, l),
        Node::Row { cells: c, gap, grid } => cells(cx, c, *gap, grid.as_ref(), l),
        Node::Columns { cells: c, align, rtl } => columns(cx, c, *align, *rtl, l),
        Node::Image(img) => image(cx, img, l),
        Node::Rule => el().h(1.0).w_full().my(10.0).fill(k.c.border_muted),
        Node::Space(h) => el().h(h.min(40.0)),
        Node::Blank(h) => el().h(h.min(48.0)),
        Node::List { ordered, marker, items } => {
            let mut c = col().w_full().gap(4.0).pl(if *marker { 4.0 } else { 0.0 });
            // The marker's column and the gap beside it.
            let inner = Local { room: (l.room - if *marker { 32.0 } else { 0.0 }).max(0.0), ..l };
            for (i, it) in items.iter().enumerate() {
                let mut body = col().grow(1.0).min_w(0.0);
                for n in it {
                    body = body.child(node(cx, n, inner));
                }
                let mut line = row().w_full().gap(8.0).items_start();
                if *marker {
                    let bullet = if *ordered { format!("{}.", i + 1) } else { "•".to_owned() };
                    line = line.child(aside(text(bullet).font(rune::TextStyle::new(14.0, 400.0).lh(20.0)).color(l.ink)));
                }
                c = c.child(line.child(body));
            }
            c
        }
        Node::Quote(kids) => {
            let mut c = col().w_full().pl(12.0).my(4.0).border_sides([0.0, 0.0, 0.0, 2.0], k.c.border_strong);
            for n in kids {
                c = c.child(node(cx, n, Local { ink: k.c.text_muted, room: (l.room - 14.0).max(0.0), ..l }));
            }
            c
        }
        Node::Pre(t) => text(t.clone())
            .font(rune::TextStyle::new(12.5, 400.0).mono().lh(19.0))
            .color(l.ink)
            .wrap_anywhere()
            .selectable(true)
            .w_full()
            .padding(10.0)
            .radius(6.0)
            .fill(k.c.layer_01),
    }
}

#[cfg(test)]
thread_local! {
    /// How many times [`render`] ran: a message opening should build its elements once.
    pub static RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn render(cx: &Ctx, doc: &Doc) -> El {
    #[cfg(test)]
    RENDERS.with(|r| r.set(r.get() + 1));
    let original = !cx.adapt;
    let ink = if original { srgb8(0x22, 0x22, 0x22) } else { cx.t.k.c.text_base };
    let l = Local { on_color: false, tight: false, ink, under: doc.background, room: cx.content_width() };
    let mut c = col().key("html").w_full().min_w(0.0).max_w(cx.max_w);
    for n in &doc.nodes {
        c = c.child(node(cx, n, l));
    }
    if original {
        let page = doc.background.map(rgba).unwrap_or(Color::WHITE);
        col().w_full().min_w(0.0).max_w(cx.max_w + 2.0 * PAGE_PAD).padding(PAGE_PAD).radius(12.0).fill(page).child(c)
    } else {
        c
    }
}

#[cfg(test)]
#[path = "html_view_tests.rs"]
mod fit_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn every_linked_element_honours_copy_links() {
        // `El::href` installs a press handler that always opens the URL, ignoring
        // `behavior.copyLinks`; boxes and pictures used it directly. Now only `linked` may
        // call it, and it replaces that handler with the one text links use.
        let src = include_str!("html_view.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let call = [".hr", "ef("].concat();
        assert_eq!(code.matches(&call).count(), 1, "every link goes through `linked`");
        let at = code.find(&call).unwrap();
        let def = code[..at].rfind("fn ").unwrap();
        assert!(code[def..].starts_with("fn linked("), "the one call is in `linked`");
        assert!(code[at..].lines().next().unwrap().contains("follow(&u, copy, tx)"));
    }
}
