//! The no-overflow guard: every fixture laid out by Rune as the reader lays it out, at a
//! phone-narrow, a default-pane and a wide content width, pictures hidden and loaded, colours
//! adapted and original. No element's box may reach past the width or out of the box it's in
//! (zero-wide ones included), and no line of text past either edge of its own text box (a word
//! too long for its line). Fluid columns stack below the breakpoint and stand side by side
//! above it, and a table's columns line up down it.

use std::collections::HashMap;
use std::sync::Arc;

use envoy_core::html::{self, Doc, Node};
use rune::accesskit::{Node as AkNode, NodeId, Role};
use rune::{Cx, El, Env, Runtime, col, el, text};

use super::{Ctx, render};
use crate::app::ImageState;
use crate::theme::Theme;

/// Content widths, in points: a phone-narrow pane, the default reading pane beside the list,
/// and a wide one.
const WIDTHS: [f32; 3] = [320.0, 520.0, 900.0];

/// How far past an edge counts as overflow: half a point of rounding.
const SLACK: f32 = 0.5;

/// Pictures hidden or loaded, colours adapted or original ("Original" draws a page round it).
const MODES: [(bool, bool); 4] = [(false, true), (false, false), (true, true), (true, false)];

struct S {
    doc: Arc<Doc>,
    theme: Theme,
    images: HashMap<String, ImageState>,
    width: f32,
    adapt: bool,
}

fn view(s: &S, _: &mut Cx<S>) -> El {
    // `reading.maxWidth` at its default, or widened to the pane.
    let max_w = s.width.max(760.0);
    let hc = Ctx { t: &s.theme, images: &s.images, adapt: s.adapt, load_images: true, copy_links: false, max_w, width: s.width };
    col().key("pane").w(s.width).child(render(&hc, &s.doc))
}

fn fixtures() -> Vec<(String, String)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../envoy-core/tests/fixtures/html");
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&p).unwrap()))
        .collect();
    out.sort();
    out
}

fn image_sizes(nodes: &[Node], out: &mut HashMap<String, (u32, u32)>) {
    for n in nodes {
        match n {
            Node::Image(i) => {
                let w = i.width.unwrap_or(640.0).clamp(1.0, 1200.0);
                let h = i.height.unwrap_or(w / 2.0).clamp(1.0, 1200.0);
                out.entry(i.src.clone()).or_insert((w as u32, h as u32));
            }
            Node::Box { style, children } => {
                if let Some(u) = &style.bg_image {
                    out.entry(u.clone()).or_insert((600, 300));
                }
                image_sizes(children, out);
            }
            Node::Row { cells, .. } | Node::Columns { cells, .. } => {
                for (style, kids) in cells {
                    if let Some(u) = &style.bg_image {
                        out.entry(u.clone()).or_insert((600, 300));
                    }
                    image_sizes(kids, out);
                }
            }
            Node::List { items, .. } => items.iter().for_each(|it| image_sizes(it, out)),
            Node::Quote(kids) => image_sizes(kids, out),
            _ => {}
        }
    }
}

/// Every picture the message draws, decoded: grey, at the size it declares (or 640 wide).
fn loaded(doc: &Doc) -> HashMap<String, ImageState> {
    let cache = rune::ImageCache::new(Arc::new(|| {}));
    let mut sizes = HashMap::new();
    image_sizes(&doc.nodes, &mut sizes);
    sizes
        .into_iter()
        .map(|(u, (w, h))| {
            let img = cache.rgba(w, h, vec![160u8; (w * h * 4) as usize]);
            img.wait().expect("decodes");
            (u, ImageState::Ready(img))
        })
        .collect()
}

/// `doc` laid out `width` points wide.
fn lay_out(doc: &Arc<Doc>, width: f32, images: HashMap<String, ImageState>, adapt: bool) -> Runtime<S> {
    let theme = crate::theme::build(&envoy_core::settings::Settings::default(), false);
    let mut rt = Runtime::new(S { doc: doc.clone(), theme, images, width, adapt }, view, (width.max(1.0), 30_000.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

/// A line of text as drawn: where its first and last visible characters start and end. The
/// space after its last word, which hangs past the edge as in browsers, isn't counted.
fn ink(run: &AkNode) -> Option<(f32, f32)> {
    let b = run.bounds()?;
    let (value, at) = (run.value().unwrap_or(""), b.x0 as f32);
    let (lefts, widths) = (run.character_positions()?, run.character_widths()?);
    let mut byte = 0;
    let mut span: Option<(f32, f32)> = None;
    for (k, len) in run.character_lengths().iter().enumerate() {
        let ch = value.get(byte..byte + *len as usize).unwrap_or("");
        byte += *len as usize;
        if ch.chars().all(char::is_whitespace) {
            continue;
        }
        let (l, r) = (at + lefts.get(k)?, at + lefts.get(k)? + widths.get(k)?);
        span = Some(span.map_or((l, r), |(a, b)| (a.min(l), b.max(r))));
    }
    span
}

/// What reaches past `width`, or out of the box it's in, inside the element keyed `scope`:
/// element boxes (zero-wide ones too), and each line of text against both edges of its own
/// text box. Also the lines measured. A text's lines come from the accessibility tree, which
/// has them for selectable text (all text in mail is, under test: `aside`).
fn overflow<T: 'static>(rt: &Runtime<T>, width: f32, scope: &str) -> (Vec<String>, Vec<String>) {
    overflow_between(rt, 0.0, width, scope)
}

/// [`overflow`] for what's laid out between `left` and `right`, a pane of a window: the
/// reader beside the list, say.
pub(crate) fn overflow_between<T: 'static>(rt: &Runtime<T>, left: f32, right: f32, scope: &str) -> (Vec<String>, Vec<String>) {
    let width = right;
    let mut out = Vec::new();
    let els = rt.elements();
    let inside = |path: &str| path.split('/').any(|p| p == scope);
    let name = |i: usize| {
        let e = &els[i];
        e.text.as_deref().map(|t| format!("{:?}", t.chars().take(40).collect::<String>())).unwrap_or_else(|| e.path.clone())
    };
    let outside = |lo: f32, hi: f32, from: f32, to: f32| lo < from - SLACK || hi > to + SLACK;
    for (i, e) in els.iter().enumerate().filter(|(_, e)| inside(&e.path)) {
        let [x, _, w, _] = e.rect;
        if !(x.is_finite() && w.is_finite() && w >= 0.0) {
            out.push(format!("{} has no proper box: {:?}", name(i), e.rect));
        } else if outside(x, x + w, left, width) {
            out.push(format!("{} spans {x:.1}..{:.1}", name(i), x + w));
        } else if let Some(p) = e.parent.filter(|&p| inside(&els[p].path) && !e.path.contains("#link-")) {
            // (A link's hit area covers its text's lines, hanging spaces too: those are
            // measured as lines below.)
            let [px, _, pw, _] = els[p].rect;
            if outside(x, x + w, px, px + pw) {
                out.push(format!("{} spans {x:.1}..{:.1}, out of {} at {px:.1}..{:.1}", name(i), x + w, name(p), px + pw));
            }
        }
    }
    let tree = rt.accessibility_tree();
    let nodes: HashMap<NodeId, &AkNode> = tree.nodes.iter().map(|(id, n)| (*id, n)).collect();
    let mut lines = Vec::new();
    for n in nodes.values() {
        let runs: Vec<&AkNode> = n.children().iter().filter_map(|c| nodes.get(c).copied()).filter(|c| c.role() == Role::TextRun).collect();
        let Some(b) = n.bounds().filter(|_| !runs.is_empty()) else { continue };
        let (bx0, bx1) = (b.x0 as f32, b.x1 as f32);
        for run in runs {
            lines.push(run.value().unwrap_or("").trim_end().to_owned());
            let Some((l, r)) = ink(run) else { continue };
            if outside(l, r, bx0.max(left), bx1.min(width)) {
                let what: String = run.value().unwrap_or("").chars().take(40).collect();
                out.push(format!("the line {what:?} spans {l:.1}..{r:.1}, out of its text at {bx0:.1}..{bx1:.1}"));
            }
        }
    }
    (out, lines)
}

#[test]
fn no_fixture_overflows_the_reader() {
    let mut failures = Vec::new();
    let (mut lines, mut aside) = (0, 0);
    // Cerberus's pictures' descriptions and its lists' markers: text that isn't the message's
    // own and isn't selectable outside tests, measured all the same.
    let beside = |line: &str| line == "•" || line.starts_with("[alt_text]");
    for (name, src) in fixtures() {
        let doc = Arc::new(html::parse(&src));
        for width in WIDTHS {
            for (pictures, adapt) in MODES {
                let images = if pictures { loaded(&doc) } else { HashMap::new() };
                let rt = lay_out(&doc, width, images, adapt);
                let (bad, seen) = overflow(&rt, width, "pane");
                lines += seen.len();
                aside += seen.iter().filter(|l| beside(l)).count();
                if !bad.is_empty() {
                    let mode = format!("{}, {}", if pictures { "pictures" } else { "no pictures" }, if adapt { "adapted" } else { "original" });
                    failures.push(format!("{name} at {width} pt ({mode}): {} overflow, e.g. {}", bad.len(), bad.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
                }
            }
        }
    }
    assert!(lines > 5_000 && aside > 100, "lines of text measured: {lines}, of alt labels and list markers: {aside}");
    assert!(failures.is_empty(), "content reaches past the reader:\n{}", failures.join("\n"));
}

#[test]
fn nothing_breaks_at_no_width() {
    // A pane squeezed to nothing: every box still a proper one (finite, not negative).
    for (name, src) in fixtures() {
        let doc = Arc::new(html::parse(&src));
        for (pictures, adapt) in MODES {
            let rt = lay_out(&doc, 0.0, if pictures { loaded(&doc) } else { HashMap::new() }, adapt);
            for e in rt.elements() {
                assert!(e.rect.iter().all(|v| v.is_finite()) && e.rect[2] >= 0.0 && e.rect[3] >= 0.0, "{name}: {:?} at {:?}", e.text.as_deref().unwrap_or(&e.path), e.rect);
            }
        }
    }
}

/// One of the guard's own cases, in a 200-point pane.
struct Case(u8);

fn case(c: &Case, _: &mut Cx<Case>) -> El {
    let inner = match c.0 {
        0 => el().w(200.0).h(10.0),
        1 => el().w(201.0).h(10.0),
        2 => el().w(0.0).h(10.0).absolute(230.0, 0.0),
        3 => text("Unbreakableword").selectable(true).wrap().align(rune::Align::Center).w(40.0).ml(80.0),
        4 => text("a few short words").selectable(true).wrap().w(120.0),
        5 => text("Unbreakableword").selectable(true).wrap().w(40.0).ml(80.0),
        // Its second line falls below its box: no box holds where the line starts.
        _ => text("Hi Unbreakableword").selectable(true).wrap().w(40.0).h(8.0).ml(80.0),
    };
    col().w(200.0).child(col().key("html").w(200.0).child(inner))
}

fn reported(n: u8) -> Vec<String> {
    let mut rt = Runtime::new(Case(n), case, (200.0, 100.0), Env::new(false, 1.0));
    rt.frame(0.0);
    overflow(&rt, 200.0, "html").0
}

#[test]
fn the_guard_catches_what_reaches_out() {
    assert!(reported(0).is_empty(), "a box exactly as wide as the pane: {:?}", reported(0));
    assert!(!reported(1).is_empty(), "a point too wide");
    assert!(!reported(2).is_empty(), "a zero-wide box past the edge");
    assert!(!reported(3).is_empty(), "a centred line wider than its own box, inside the pane");
    assert!(!reported(5).is_empty(), "a line running past its own box's right edge, inside the pane");
    assert!(!reported(6).is_empty(), "a line outside its box's height, judged by its own box, not the pane");
    assert!(reported(4).is_empty(), "text that fits: {:?}", reported(4));
}

#[test]
fn columns_stack_on_the_breakpoints_sides() {
    // MJML's two columns: side by side from 480 points, stacked an 8-point step below.
    let mjml = Arc::new(html::parse(&fixtures().into_iter().find(|(n, _)| n == "mjml-newsletter").unwrap().1));
    let first = |width: f32| column_spots(&lay_out(&mjml, width, HashMap::new(), true), &mjml)[0].clone();
    assert!(stacked(&first(472.0)), "{:?}", first(472.0));
    assert!(side_by_side(&first(480.0)), "{:?}", first(480.0));
    // Five fifths: side by side while each is 120 points, stacked when one would be less.
    let fifths = Arc::new(html::parse(&(1..=5).map(|i| format!("<div style='display:inline-block;width:20%'>Column {i}</div>")).collect::<String>()));
    let row = |width: f32| column_spots(&lay_out(&fifths, width, HashMap::new(), true), &fifths)[0].clone();
    assert!(side_by_side(&row(600.0)), "{:?}", row(600.0));
    assert!(stacked(&row(592.0)), "{:?}", row(592.0));
}

#[test]
fn table_columns_line_up_down_the_table() {
    // An 80/20 receipt with a small header row, data rows in a larger size, and a three-column
    // table with a colspan total: every row's cells end on the same column edges.
    let doc = Arc::new(html::parse(
        r#"<table width="100%"><tr><th width="80%" align="left" style="font-size:12px">Description</th><th width="20%" align="left" style="font-size:12px">Amount</th></tr>
           <tr><td style="font-size:16px">Pro plan (monthly)</td><td style="font-size:16px">$1,234.56</td></tr>
           <tr><td style="font-size:16px">Extra seat</td><td style="font-size:16px">$12.00</td></tr></table>
           <table width="100%"><tr><td>Item</td><td>Qty</td><td style="font-size:16px">$1,234.50</td></tr><tr><td colspan="2">Subtotal, before tax and shipping</td><td>$9.00</td></tr></table>"#,
    ));
    for width in [320.0, 520.0, 900.0] {
        let rt = lay_out(&doc, width, HashMap::new(), true);
        let els = rt.elements();
        // The left edge of the cell holding `t`.
        let cell = |t: &str| {
            let e = els.iter().find(|e| e.text.as_deref() == Some(t)).unwrap_or_else(|| panic!("{t}"));
            els[e.parent.unwrap()].rect[0]
        };
        let amounts = [cell("Amount"), cell("$1,234.56"), cell("$12.00")];
        assert!(amounts.iter().all(|x| (x - amounts[0]).abs() < 0.5), "the amount column at {width} pt: {amounts:?}");
        let last = [cell("$1,234.50"), cell("$9.00")];
        assert!((last[0] - last[1]).abs() < 0.5, "below a colspan at {width} pt: {last:?}");
    }
}

/// Every paragraph's text in tree order, and per row of columns (fluid columns, and table
/// rows the email stacks on phones) the paragraph each column with text starts with.
fn paragraphs(nodes: &[Node], paras: &mut Vec<String>, rows: &mut Vec<Vec<usize>>) {
    for n in nodes {
        match n {
            Node::Para(p) => paras.push(p.text()),
            Node::Columns { cells, .. } => {
                let mut row = Vec::new();
                for (_, kids) in cells {
                    let before = paras.len();
                    paragraphs(kids, paras, rows);
                    if paras.len() > before {
                        row.push(before);
                    }
                }
                if row.len() > 1 {
                    rows.push(row);
                }
            }
            Node::Box { children, .. } | Node::Quote(children) => paragraphs(children, paras, rows),
            // A table row the email stacks on phones counts as a row of columns.
            Node::Row { cells, .. } if cells.iter().filter(|(_, k)| !k.is_empty()).count() > 1 && cells.iter().all(|(s, k)| k.is_empty() || s.stack) => {
                let mut row = Vec::new();
                for (_, kids) in cells {
                    let before = paras.len();
                    paragraphs(kids, paras, rows);
                    if paras.len() > before {
                        row.push(before);
                    }
                }
                if row.len() > 1 {
                    rows.push(row);
                }
            }
            Node::Row { cells, .. } => cells.iter().for_each(|(_, k)| paragraphs(k, paras, rows)),
            Node::List { items, .. } => items.iter().for_each(|k| paragraphs(k, paras, rows)),
            _ => {}
        }
    }
}

/// Where each row's columns start, `[x, y]` per column in source order: the text element of
/// each column's first paragraph (the k-th that reads the same, as in the tree).
fn column_spots(rt: &Runtime<S>, doc: &Doc) -> Vec<Vec<[f32; 2]>> {
    let (mut paras, mut rows) = (Vec::new(), Vec::new());
    paragraphs(&doc.nodes, &mut paras, &mut rows);
    let els = rt.elements();
    let texts: Vec<(&str, [f32; 4])> = els.iter().filter(|e| !e.path.contains("#link-")).filter_map(|e| Some((e.text.as_deref()?, e.rect))).collect();
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|&i| {
                    let head: String = paras[i].chars().take(32).collect();
                    let k = paras[..i].iter().filter(|p| p.starts_with(&head)).count();
                    let r = texts.iter().filter(|t| t.0.starts_with(&head)).nth(k).unwrap_or_else(|| panic!("no text element for {head:?}")).1;
                    [r[0], r[1]]
                })
                .collect()
        })
        .collect()
}

fn stacked(row: &[[f32; 2]]) -> bool {
    row.windows(2).all(|p| p[1][1] > p[0][1] + 1.0)
}

fn side_by_side(row: &[[f32; 2]]) -> bool {
    row.windows(2).all(|p| (p[1][1] - p[0][1]).abs() < 40.0 && (p[1][0] - p[0][0]).abs() > 40.0)
}

#[test]
fn fluid_columns_stack_when_narrow_and_stand_side_by_side_when_wide() {
    let mut seen = 0;
    for (name, src) in fixtures() {
        let doc = Arc::new(html::parse(&src));
        // Phone-narrow: every row of columns reads top to bottom, in source order.
        let rows = column_spots(&lay_out(&doc, 320.0, HashMap::new(), true), &doc);
        for row in &rows {
            assert!(stacked(row), "{name} at 320 pt: columns at {row:?} don't stack");
        }
        seen += rows.len();
        // Wide enough for the template's own desktop layout: side by side. MJML's columns are
        // shares of the row above its 480 px breakpoint; Cerberus's hybrid columns are up to
        // 330 px each, so two need its whole 680 px container, as in a browser.
        let wide: &[f32] = match name.as_str() {
            "mjml-newsletter" | "cerberus-responsive" | "foundation-hero" => &[520.0, 900.0],
            "cerberus-hybrid" => &[900.0],
            _ => &[],
        };
        {
            for &width in wide {
                let rows = column_spots(&lay_out(&doc, width, HashMap::new(), true), &doc);
                assert!(!rows.is_empty(), "{name} has rows of columns");
                for row in &rows {
                    assert!(side_by_side(row), "{name} at {width} pt: columns at {row:?} should stand side by side");
                }
            }
        }
    }
    assert!(seen >= 7, "rows of columns checked: {seen}");
}
