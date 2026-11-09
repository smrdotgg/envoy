//! Table column widths, shared by every row: the column model behind `colspan`.
//!
//! A simplified version of the browser's automatic table layout. Each column gets an explicit
//! width (the widest `width` a single-column cell declares, in pixels or percent) or, failing
//! that, a share of what's left in proportion to its content: an estimate of the narrowest
//! (longest word, image) and widest (all on one line) the column's cells can be. Spanning
//! cells spread their content over the columns they cover. The result is each column's
//! fraction of the table's width, so every row lays its cells out on the same grid at any
//! pane width.

use super::{Len, Node};

/// Average advance of a character, as a fraction of the font size (Inter and the common
/// email sans fonts sit near 0.5; a little over keeps estimates from wrapping words).
const CHAR_EM: f32 = 0.55;

/// One cell's claim on the columns it covers.
#[derive(Clone, Debug)]
pub struct Claim {
    pub col: usize,
    pub span: usize,
    pub width: Option<Len>,
    /// Content estimate, padding included: (narrowest, widest) in points.
    pub min: f32,
    pub max: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Columns {
    /// Each column's share of the table's width; they sum to 1.
    pub fracs: Vec<f32>,
    /// Each column's width in points at the table's own width.
    pub widths: Vec<f32>,
    /// Columns that declared a pixel width.
    pub fixed: Vec<bool>,
    /// The table's width in points (its declared width, or what its content needs).
    pub width: f32,
}

/// Resolves `n` columns for a table `width` points wide; `declared` says whether the table
/// set its width (otherwise it shrinks to what the content needs, up to `width`).
pub fn resolve(n: usize, claims: &[Claim], width: f32, declared: bool) -> Columns {
    let n = n.max(1);
    let mut px = vec![0.0f32; n];
    let mut pct = vec![0.0f32; n];
    let mut min = vec![0.0f32; n];
    let mut max = vec![0.0f32; n];
    for c in claims.iter().filter(|c| c.span == 1 && c.col < n) {
        match c.width {
            Some(Len::Px(w)) => px[c.col] = px[c.col].max(w),
            Some(Len::Pct(p)) => pct[c.col] = pct[c.col].max(p),
            None => {}
        }
        min[c.col] = min[c.col].max(c.min);
        max[c.col] = max[c.col].max(c.max);
    }
    // Spanning cells: spread what they need beyond their columns' own, and a width they
    // declare over columns that declare none.
    for c in claims.iter().filter(|c| c.span > 1 && c.col < n) {
        let cols = c.col..(c.col + c.span).min(n);
        let k = cols.len() as f32;
        let (have_min, have_max): (f32, f32) = cols.clone().fold((0.0, 0.0), |(a, b), i| (a + min[i], b + max[i]));
        for i in cols.clone() {
            if c.min > have_min {
                min[i] += (c.min - have_min) / k;
            }
            if c.max > have_max {
                max[i] += (c.max - have_max) / k;
            }
        }
        if cols.clone().all(|i| px[i] == 0.0 && pct[i] == 0.0) {
            match c.width {
                Some(Len::Px(w)) => cols.for_each(|i| px[i] = w / k),
                Some(Len::Pct(p)) => cols.for_each(|i| pct[i] = p / k),
                None => {}
            }
        }
    }
    for i in 0..n {
        max[i] = max[i].max(min[i]);
    }
    // Percentages past 100 scale down, as browsers do.
    let pct_sum: f32 = pct.iter().sum();
    if pct_sum > 100.0 {
        pct.iter_mut().for_each(|p| *p *= 100.0 / pct_sum);
    }
    let mut w = vec![0.0f32; n];
    let mut fixed_sum = 0.0;
    let auto: Vec<usize> = (0..n).filter(|&i| px[i] == 0.0 && pct[i] == 0.0).collect();
    for i in 0..n {
        if pct[i] > 0.0 {
            w[i] = (pct[i] / 100.0 * width).max(min[i]);
            fixed_sum += w[i];
        } else if px[i] > 0.0 {
            w[i] = px[i].max(min[i]);
            fixed_sum += w[i];
        }
    }
    let auto_min: f32 = auto.iter().map(|&i| min[i]).sum();
    let auto_max: f32 = auto.iter().map(|&i| max[i]).sum();
    let table = if declared { width } else { width.min(fixed_sum + auto_max).max(fixed_sum + auto_min) };
    let room = table - fixed_sum;
    if !auto.is_empty() {
        if room >= auto_max {
            let extra = room - auto_max;
            for &i in &auto {
                let share = if auto_max > 0.0 { max[i] / auto_max } else { 1.0 / auto.len() as f32 };
                w[i] = max[i] + extra * share;
            }
        } else if room >= auto_min && auto_max > auto_min {
            let t = (room - auto_min) / (auto_max - auto_min);
            for &i in &auto {
                w[i] = min[i] + (max[i] - min[i]) * t;
            }
        } else {
            for &i in &auto {
                w[i] = min[i];
            }
        }
    } else if declared && fixed_sum > 0.0 && fixed_sum < table {
        // Every column fixed and the table wider: the room goes to them in proportion.
        let k = table / fixed_sum;
        w.iter_mut().for_each(|x| *x *= k);
    }
    let total: f32 = w.iter().sum();
    let fracs = if total > 0.0 { w.iter().map(|x| x / total).collect() } else { vec![1.0 / n as f32; n] };
    let width = if declared { width } else { total.max(1.0) };
    Columns { fracs, widths: w, fixed: px.iter().map(|&p| p > 0.0).collect(), width }
}

/// (narrowest, widest) a run of nodes can be laid out, in points.
pub fn estimate(nodes: &[Node]) -> (f32, f32) {
    estimate_capped(nodes, f32::INFINITY)
}

/// [`estimate`], stopping early once the widest passes `cap` (a big subtree needn't be walked
/// to learn it's too wide to shrink to).
pub fn estimate_capped(nodes: &[Node], cap: f32) -> (f32, f32) {
    let estimate = |n: &[Node]| estimate_capped(n, cap);
    let mut lo: f32 = 0.0;
    let mut hi: f32 = 0.0;
    for n in nodes {
        if hi > cap {
            break;
        }
        let (a, b) = match n {
            Node::Para(p) => {
                let mut longest_word: f32 = 0.0;
                let mut line: f32 = 0.0;
                let mut widest: f32 = 0.0;
                for r in &p.runs {
                    let size = r.size.unwrap_or(14.0);
                    let k = size * if r.mono { 0.62 } else { CHAR_EM } * if r.weight >= 600 { 1.06 } else { 1.0 };
                    let track = p.tracking;
                    for line_text in r.text.split_inclusive('\n') {
                        for word in line_text.split(' ') {
                            let wl = word.trim_end_matches('\n').chars().count() as f32 * (k + track);
                            longest_word = longest_word.max(wl);
                        }
                        line += line_text.trim_end_matches('\n').chars().count() as f32 * (k + track);
                        if line_text.ends_with('\n') {
                            widest = widest.max(line);
                            line = 0.0;
                        }
                    }
                }
                (longest_word, widest.max(line))
            }
            Node::Box { style, children } => {
                let (a, b) = estimate(children);
                let edge = style.pad[1] + style.pad[3] + style.margin[1] + style.margin[3] + style.border.as_ref().map_or(0.0, |b| b.widths[1] + b.widths[3]);
                match style.width {
                    Some(Len::Px(w)) if !style.tight => (a.min(w) + edge, w.max(a + edge)),
                    _ => (a + edge, b + edge),
                }
            }
            Node::Row { cells, gap, .. } => {
                let mut a = 0.0;
                let mut b = 0.0;
                for (s, kids) in cells {
                    let (x, y) = estimate(kids);
                    let edge = s.pad[1] + s.pad[3];
                    match s.width {
                        Some(Len::Px(w)) => {
                            a += w.max(x + edge);
                            b += w.max(x + edge);
                        }
                        _ => {
                            a += x + edge;
                            b += y + edge;
                        }
                    }
                }
                let g = gap * cells.len().saturating_sub(1) as f32;
                (a + g, b + g)
            }
            Node::Columns { cells, .. } => {
                let mut a: f32 = 0.0;
                let mut b = 0.0;
                for (s, kids) in cells {
                    let (x, y) = estimate(kids);
                    let w = s.max_width.unwrap_or(y);
                    a = a.max(s.min_width.max(x));
                    b += w;
                }
                (a, b)
            }
            Node::Image(img) => {
                let w = img.width.unwrap_or(100.0);
                (w.min(120.0), w)
            }
            Node::List { items, .. } => {
                let mut a: f32 = 0.0;
                let mut b: f32 = 0.0;
                for it in items {
                    let (x, y) = estimate(it);
                    a = a.max(x);
                    b = b.max(y);
                }
                (a + 24.0, b + 24.0)
            }
            Node::Quote(kids) => {
                let (a, b) = estimate(kids);
                (a + 14.0, b + 14.0)
            }
            Node::Pre(t) => {
                let w = t.lines().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * 12.5 * 0.62 + 20.0;
                (w.min(200.0), w)
            }
            Node::Rule | Node::Space(_) | Node::Blank(_) => (0.0, 0.0),
        };
        lo = lo.max(a);
        hi = hi.max(b);
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(col: usize, span: usize, width: Option<Len>, min: f32, max: f32) -> Claim {
        Claim { col, span, width, min, max }
    }

    #[test]
    fn explicit_widths_then_content() {
        // 600 wide: a 64px image column, an auto text column, an 80px price column.
        let c = resolve(3, &[claim(0, 1, Some(Len::Px(64.0)), 56.0, 56.0), claim(1, 1, None, 80.0, 300.0), claim(2, 1, Some(Len::Px(80.0)), 40.0, 50.0)], 600.0, true);
        assert_eq!(c.widths[0], 64.0);
        assert_eq!(c.widths[2], 80.0);
        assert!((c.widths[1] - 456.0).abs() < 0.01, "{c:?}");
        assert!((c.fracs.iter().sum::<f32>() - 1.0).abs() < 1e-4);
        assert_eq!(c.fixed, vec![true, false, true]);
    }

    #[test]
    fn percentages_and_spans() {
        // 80% / 20% and a header spanning both.
        let c = resolve(2, &[claim(0, 2, None, 100.0, 400.0), claim(0, 1, Some(Len::Pct(80.0)), 50.0, 200.0), claim(1, 1, Some(Len::Pct(20.0)), 30.0, 40.0)], 500.0, true);
        assert!((c.fracs[0] - 0.8).abs() < 0.01 && (c.fracs[1] - 0.2).abs() < 0.01, "{c:?}");
        // Columns without widths share by content; a spanning cell's need is spread.
        let c = resolve(2, &[claim(0, 2, None, 300.0, 600.0), claim(0, 1, None, 10.0, 20.0), claim(1, 1, None, 10.0, 20.0)], 1000.0, false);
        assert!((c.width - 600.0).abs() < 1.0, "shrinks to the spanning cell's width: {c:?}");
        assert!((c.fracs[0] - 0.5).abs() < 0.01);
    }

    #[test]
    fn shrink_to_fit_and_overflow() {
        // No declared width: as wide as the content wants, up to the room there is.
        let c = resolve(2, &[claim(0, 1, None, 20.0, 60.0), claim(1, 1, None, 20.0, 140.0)], 600.0, false);
        assert!((c.width - 200.0).abs() < 0.01, "{c:?}");
        let c = resolve(2, &[claim(0, 1, None, 100.0, 900.0), claim(1, 1, None, 100.0, 100.0)], 400.0, false);
        assert!((c.width - 400.0).abs() < 0.01 && c.widths[1] < c.widths[0], "{c:?}");
        // All fixed, inside a wider declared table: they grow in proportion.
        let c = resolve(2, &[claim(0, 1, Some(Len::Px(100.0)), 0.0, 0.0), claim(1, 1, Some(Len::Px(300.0)), 0.0, 0.0)], 800.0, true);
        assert!((c.fracs[0] - 0.25).abs() < 0.01);
    }
}
