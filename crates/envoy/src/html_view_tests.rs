//! The no-overflow guard: every fixture laid out by Rune as the reader lays it out, at a
//! phone-narrow, a default-pane and a wide content width, images hidden and loaded, colours
//! adapted and original. No element's box and no line of text (a word too long for its line)
//! may reach past the width, and fluid columns stack below the breakpoint and stand side by
//! side above it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use envoy_core::html::{self, Doc, Node};
use rune::{Cx, El, Env, Runtime, col};

use super::{Ctx, render};
use crate::app::ImageState;
use crate::theme::Theme;

/// Content widths, in points: a phone-narrow pane, the default reading pane beside the list,
/// and a wide one.
const WIDTHS: [f32; 3] = [320.0, 520.0, 900.0];

/// How far past the edge counts as overflow: half a point of rounding.
const SLACK: f32 = 0.5;

/// A line's box may end past its text box by the space after its last word, which hangs
/// there as in browsers and selects with it: about a third of the line's height.
const HANGING_SPACE: f32 = 0.35;

struct S {
    doc: Arc<Doc>,
    theme: Theme,
    images: HashMap<String, ImageState>,
    width: f32,
    adapt: bool,
}

thread_local! {
    /// The line boxes of all the text, as the last view saw them (`Cx::selection_rects`).
    static LINES: RefCell<Vec<[f32; 4]>> = const { RefCell::new(Vec::new()) };
}

fn view(s: &S, cx: &mut Cx<S>) -> El {
    let lines = cx.selection_rects();
    LINES.with_borrow_mut(|l| *l = lines);
    // `reading.maxWidth` at its default, or widened to the pane.
    let max_w = s.width.max(760.0);
    let hc = Ctx { t: &s.theme, images: &s.images, adapt: s.adapt, load_images: true, copy_links: false, max_w, width: s.width };
    col().key("pane").selection_group(true).w(s.width).child(render(&hc, &s.doc))
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

/// `doc` laid out `width` points wide, every text selected so its line boxes are known.
fn lay_out(doc: &Arc<Doc>, width: f32, images: HashMap<String, ImageState>, adapt: bool) -> Runtime<S> {
    let theme = crate::theme::build(&envoy_core::settings::Settings::default(), false);
    let mut rt = Runtime::new(S { doc: doc.clone(), theme, images, width, adapt }, view, (width, 30_000.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt.update(|_, tx| tx.select_all_text("pane"));
    rt.frame(0.0);
    rt.frame(0.0);
    rt
}

/// What reaches past `width`, or out of the box it's in: element boxes and text lines,
/// described.
fn overflow(rt: &Runtime<S>, width: f32) -> Vec<String> {
    let mut out = Vec::new();
    let els = rt.elements();
    let name = |i: usize| {
        let e = &els[i];
        e.text.as_deref().map(|t| format!("{:?}", t.chars().take(40).collect::<String>())).unwrap_or_else(|| e.path.clone())
    };
    let outside = |x: f32, w: f32, lo: f32, hi: f32| w > 0.0 && (x < lo - SLACK || x + w > hi + SLACK);
    for (i, e) in els.iter().enumerate() {
        let [x, _, w, _] = e.rect;
        if outside(x, w, 0.0, width) {
            out.push(format!("{} spans {x:.1}..{:.1}", name(i), x + w));
        } else if let Some(p) = e.parent.filter(|_| e.path.contains("html") && !e.path.contains("#link-")) {
            // (A link's hit area covers its text's lines, hanging spaces too: those are
            // checked as lines below.)
            let [px, _, pw, _] = els[p].rect;
            if outside(x, w, px, px + pw) {
                out.push(format!("{} spans {x:.1}..{:.1}, out of {} at {px:.1}..{:.1}", name(i), x + w, name(p), px + pw));
            }
        }
    }
    let texts: Vec<[f32; 4]> = els.iter().filter(|e| e.text.is_some()).map(|e| e.rect).collect();
    for [x, y, w, h] in LINES.with_borrow(|l| l.clone()) {
        // The text box the line starts in.
        let home = texts.iter().find(|r| x + 0.5 >= r[0] && x + 0.5 <= r[0] + r[2] && y + h / 2.0 >= r[1] && y + h / 2.0 <= r[1] + r[3]);
        let hi = home.map_or(width, |r| (r[0] + r[2]).min(width));
        if outside(x, w, 0.0, hi + h * HANGING_SPACE) {
            out.push(format!("a line of text at y {y:.0} spans {x:.1}..{:.1}, past {hi:.1}", x + w));
        }
    }
    out
}

#[test]
fn no_fixture_overflows_the_reader() {
    let mut failures = Vec::new();
    let mut lines_seen = 0;
    for (name, src) in fixtures() {
        let doc = Arc::new(html::parse(&src));
        for width in WIDTHS {
            for (pictures, adapt) in [(false, true), (true, true), (true, false)] {
                let images = if pictures { loaded(&doc) } else { HashMap::new() };
                let rt = lay_out(&doc, width, images, adapt);
                lines_seen += LINES.with_borrow(|l| l.len());
                let bad = overflow(&rt, width);
                if !bad.is_empty() {
                    let mode = format!("{}{}", if pictures { "images" } else { "no images" }, if adapt { "" } else { ", original" });
                    failures.push(format!("{name} at {width} pt ({mode}): {} overflow, e.g. {}", bad.len(), bad.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
                }
            }
        }
    }
    assert!(lines_seen > 1_000, "text line boxes were read ({lines_seen})");
    assert!(failures.is_empty(), "content reaches past the reader:\n{}", failures.join("\n"));
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
