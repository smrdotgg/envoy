//! How faithfully the native renderer can show a message: what [`super::parse`] dropped or
//! approximated, weighed into a score. Local only: it decides whether the reader offers the
//! original, and logs at debug level. Nothing leaves the machine.

/// Something in the email the native renderer can't draw as designed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Issue {
    /// `position: absolute | fixed | sticky` on visible content: overlays, badges, pinned bars.
    Positioned,
    /// `display: flex | grid` containers: laid out as plain blocks here.
    FlexGrid,
    /// Inline `<svg>` drawings, charts and icons.
    Svg,
    /// `<iframe>`, `<video>`, `<audio>`, `<object>`, `<embed>`, `<canvas>`.
    Embed,
    /// Form fields (`<input>`, `<select>`, `<textarea>`).
    Form,
    /// `float` on content that isn't a column we can lay out (text wraps differently).
    Float,
    /// `transform`, `clip-path`, `mix-blend-mode`, `filter`: drawn untransformed.
    Effect,
    /// `columns` / `column-count`: multi-column text drawn as one column.
    MultiColumn,
    /// Gradient backgrounds, drawn as their first colour.
    Gradient,
    /// `rowspan` that couldn't keep its shape (its content stays in its first row).
    Rowspan,
    /// `::before` / `::after` text the email adds with `content`.
    Generated,
    /// Selectors the CSS reader couldn't parse.
    BadSelector,
    /// The selector-matching budget ran out (huge sheet × huge document).
    CssBudget,
    /// Nesting deeper than the renderer descends.
    TooDeep,
    /// `<script>` (never run; emails shouldn't carry it).
    Script,
}

impl Issue {
    /// (points per occurrence, occurrences that count).
    fn weight(self) -> (u32, u32) {
        match self {
            Issue::Positioned => (12, 3),
            Issue::FlexGrid => (8, 3),
            Issue::Svg => (8, 3),
            Issue::Embed => (8, 2),
            Issue::Form => (6, 2),
            Issue::Float => (3, 4),
            Issue::Effect => (3, 2),
            Issue::MultiColumn => (6, 1),
            Issue::Gradient => (1, 3),
            Issue::Rowspan => (4, 2),
            Issue::Generated => (2, 3),
            Issue::BadSelector => (1, 5),
            Issue::CssBudget => (20, 1),
            Issue::TooDeep => (20, 1),
            Issue::Script => (1, 1),
        }
    }
}

/// How a message is likely to look natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// As designed, or near enough.
    Fine,
    /// Some details differ; the content and its order are all there.
    Approximate,
    /// Layout the reader can't reproduce: worth offering the original.
    Poor,
}

/// Scores at or above this are [`Verdict::Approximate`].
pub const APPROXIMATE: u32 = 12;
/// Scores at or above this are [`Verdict::Poor`].
pub const POOR: u32 = 30;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fidelity {
    /// Each issue seen and how often, in [`Issue`] order.
    pub issues: Vec<(Issue, u32)>,
}

impl Fidelity {
    pub fn note(&mut self, issue: Issue) {
        self.note_n(issue, 1);
    }

    pub fn note_n(&mut self, issue: Issue, n: u32) {
        if n == 0 {
            return;
        }
        match self.issues.binary_search_by(|(i, _)| i.cmp(&issue)) {
            Ok(at) => self.issues[at].1 += n,
            Err(at) => self.issues.insert(at, (issue, n)),
        }
    }

    pub fn count(&self, issue: Issue) -> u32 {
        self.issues.iter().find(|(i, _)| *i == issue).map_or(0, |(_, n)| *n)
    }

    pub fn score(&self) -> u32 {
        self.issues
            .iter()
            .map(|(i, n)| {
                let (w, cap) = i.weight();
                w * (*n).min(cap)
            })
            .sum()
    }

    pub fn verdict(&self) -> Verdict {
        match self.score() {
            s if s >= POOR => Verdict::Poor,
            s if s >= APPROXIMATE => Verdict::Approximate,
            _ => Verdict::Fine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_bands() {
        let mut f = Fidelity::default();
        assert_eq!(f.verdict(), Verdict::Fine);
        f.note_n(Issue::BadSelector, 40);
        assert_eq!(f.score(), 5, "one kind of nit can't add up to a warning");
        f.note(Issue::Positioned);
        assert_eq!(f.verdict(), Verdict::Approximate);
        f.note(Issue::Svg);
        f.note(Issue::FlexGrid);
        assert_eq!(f.verdict(), Verdict::Poor);
        assert_eq!(f.count(Issue::BadSelector), 40);
        assert!(f.issues.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
