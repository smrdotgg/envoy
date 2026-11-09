//! The CSS that email uses, and no more: `<style>` sheets and `style=""` attributes cascaded
//! per element.
//!
//! Selectors: type, class, id, universal and attribute selectors (`[a]`, `=`, `~=`, `|=`, `^=`,
//! `$=`, `*=`), descendant, child and sibling combinators, selector lists, `:first-child`,
//! `:last-child`, `:only-child`, `:nth-child(an+b)`, `:root`, `:link` and `:not(compound)`.
//! Dynamic states (`:hover`, `:checked`…) and pseudo-elements never match. The cascade orders
//! by importance, origin (the built-in sheet, then the email's sheets, then its `style=""`)
//! and specificity, then source order. `@media` is evaluated against a desktop reading pane
//! ([`VIEWPORT`]) in light mode; `@supports`, `@font-face`, `@keyframes` and `@import` are
//! dropped (nothing is ever fetched). Shorthands (`margin`, `padding`, `border*`, `background`,
//! `font`) expand into longhands as they're read, so the last declaration of a longhand wins.
//!
//! Matching is bounded: rules are indexed by their subject's id, class or tag, and a message
//! gets [`MATCH_BUDGET`] compound-selector checks in all; past that, sheets stop applying.

use std::collections::HashMap;
use std::rc::Rc;

/// Width, in points, `@media` width queries are evaluated against: a desktop reading pane.
/// Mobile rules (`max-width: 600px`) don't apply; narrow panes are fitted when the tree is
/// drawn, at the reader's width, not by the email (only which table cells it stacks on phones
/// is read, [`Sheet::cascade_phone`]).
pub const VIEWPORT: f32 = 760.0;

/// Width a phone's `@media` rules are read at, to learn which table cells the email stacks on
/// phones ([`Sheet::cascade_phone`]): the rules for widths up to 600, 480 or 414 px, not those for
/// the smallest phones.
pub const NARROW: f32 = 400.0;

/// Compound-selector checks one message may spend matching its sheets.
pub const MATCH_BUDGET: u32 = 400_000;

/// Rules one message's sheets may hold; the rest are dropped (and counted).
const MAX_RULES: usize = 8_000;

/// Candidate rules one message may consider in all, even those the ancestor filter rejects
/// before they cost [`MATCH_BUDGET`].
const CANDIDATE_BUDGET: u32 = 4_000_000;

/// Declarations read from one block or `style=""`; the rest are ignored.
const MAX_DECLS: usize = 256;

/// Declarations one element collects from the sheets, highest priority first.
const MAX_ELEMENT_DECLS: usize = 1_024;

/// A selector longer than this, or with more compounds, is refused unread: matching recurses
/// once per compound, and nothing in real mail comes close.
const MAX_SELECTOR_LEN: usize = 1_024;
const MAX_COMPOUNDS: usize = 32;

/// Longest argument a functional pseudo-class (`:not(…)`, `:nth-child(…)`) may have.
const MAX_PSEUDO_ARG: usize = 256;

/// The built-in sheet: what browsers give elements that the email doesn't style.
const UA: &str = "
b, strong, th { font-weight: bold }
i, em, cite, var, dfn, address { font-style: italic }
u, ins { text-decoration: underline }
s, strike, del { text-decoration: line-through }
code, tt, kbd, samp, pre { font-family: monospace }
small { font-size: smaller }
big { font-size: larger }
h1 { font-size: 2em; font-weight: bold; margin: .67em 0 }
h2 { font-size: 1.5em; font-weight: bold; margin: .83em 0 }
h3 { font-size: 1.17em; font-weight: bold; margin: 1em 0 }
h4 { font-size: 1em; font-weight: bold; margin: 1.33em 0 }
h5 { font-size: .83em; font-weight: bold; margin: 1.67em 0 }
h6 { font-size: .75em; font-weight: bold; margin: 2.33em 0 }
p, ul, ol, dl, blockquote, figure, pre { margin: 1em 0 }
a:link { text-decoration: underline }
";

/// A declaration's name and value, both lowercase except inside `url(…)`.
pub type Styles = Vec<(Rc<str>, Rc<str>)>;

#[derive(Clone, Debug)]
struct Decl {
    name: Rc<str>,
    value: Rc<str>,
    important: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Comb {
    Descendant,
    Child,
    Adjacent,
    Sibling,
}

#[derive(Clone, Debug, PartialEq)]
enum AttrOp {
    Exists,
    Eq,
    Word,
    Dash,
    Prefix,
    Suffix,
    Contains,
}

#[derive(Clone, Debug, PartialEq)]
struct AttrSel {
    name: String,
    op: AttrOp,
    value: String,
    ci: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Pseudo {
    FirstChild,
    LastChild,
    OnlyChild,
    Root,
    Link,
    Nth(i32, i32),
    Not(Box<Compound>),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudos: Vec<Pseudo>,
}

/// Compounds subject first; `combs[i]` joins `parts[i]` to `parts[i + 1]` on its left.
#[derive(Clone, Debug)]
struct Selector {
    parts: Vec<Compound>,
    combs: Vec<Comb>,
}

#[derive(Clone, Debug)]
struct Rule {
    sel: Selector,
    spec: u32,
    order: u32,
    block: u32,
    ua: bool,
    screen: Screen,
}

/// Where a rule applies, by the `@media` it's in: everywhere, or only on the desktop pane
/// ([`VIEWPORT`]) or a phone ([`NARROW`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Screen {
    #[default]
    Both,
    Desktop,
    Phone,
}

impl Screen {
    /// For rules under media that hold on the desktop pane (`desk`) and on a phone (`phone`).
    fn of(desk: bool, phone: bool) -> Option<Screen> {
        match (desk, phone) {
            (true, true) => Some(Screen::Both),
            (true, false) => Some(Screen::Desktop),
            (false, true) => Some(Screen::Phone),
            (false, false) => None,
        }
    }
}

/// A 512-bit Bloom filter of class and id names.
pub type Bloom = [u64; 8];

/// The filter bit for a class (`kind` '.') or id ('#'): FNV-1a of the two, mod 512.
fn bloom_bit(kind: char, name: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for b in std::iter::once(kind as u8).chain(name.bytes()) {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    (h % 512) as u16
}

/// Adds a class (`.name`) or id (`#name`) to a filter.
pub fn bloom_add(bloom: &mut Bloom, kind: char, name: &str) {
    let bit = bloom_bit(kind, name);
    bloom[bit as usize / 64] |= 1 << (bit % 64);
}

/// Up to four filter bits a rule's ancestors must have (`NO_BIT` for none).
type Need = [u16; 4];
const NO_BIT: u16 = u16::MAX;

fn bloom_has(have: &Bloom, need: &Need) -> bool {
    need.iter().all(|&b| b == NO_BIT || have[b as usize / 64] & (1 << (b % 64)) != 0)
}

/// What a selector's ancestor compounds need, when it reaches them only by descendant and
/// child combinators (siblings aren't ancestors).
fn needs(sel: &Selector) -> Need {
    let mut need = [NO_BIT; 4];
    if sel.combs.iter().any(|c| matches!(c, Comb::Adjacent | Comb::Sibling)) {
        return need;
    }
    let bits = sel.parts[1..].iter().flat_map(|c| c.classes.iter().map(|cl| bloom_bit('.', cl)).chain(c.id.iter().map(|id| bloom_bit('#', id))));
    for (slot, bit) in need.iter_mut().zip(bits) {
        *slot = bit;
    }
    need
}

/// An element as selectors see it.
#[derive(Clone, Debug)]
pub struct Elem {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<(String, String)>,
    pub parent: u32,
    /// The previous element sibling.
    pub prev: u32,
    /// 1-based position among element siblings.
    pub index: u32,
    pub last: bool,
    /// Every ancestor's classes and ids.
    pub ancestors: Bloom,
}

pub const NONE: u32 = u32::MAX;

impl Elem {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// What the sheets held that the renderer drops, for the fidelity score.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SheetStats {
    /// Selectors it couldn't read (the rest of their rule still applies).
    pub bad_selectors: u32,
    /// `::before` / `::after` rules with `content`: text the email adds that we don't draw.
    pub generated: u32,
    /// Rules dropped past the cap.
    pub dropped_rules: u32,
    /// The match budget ran out: later elements got inline styles only.
    pub exhausted: bool,
}

#[derive(Default)]
pub struct Sheet {
    rules: Vec<Rule>,
    /// Each rule's ancestor filter bits, beside the rules so the hot loop stays in cache.
    need: Vec<Need>,
    blocks: Vec<Vec<Decl>>,
    by_id: HashMap<String, Vec<u32>>,
    by_class: HashMap<String, Vec<u32>>,
    by_tag: HashMap<String, Vec<u32>>,
    universal: Vec<u32>,
    order: u32,
    budget: u32,
    candidates: u32,
    /// The budgets of [`Sheet::cascade_phone`]: a quarter of the main ones.
    phone_budget: u32,
    phone_candidates: u32,
    /// Rules `0..ua_rules` are the built-in sheet's.
    ua_rules: u32,
    /// It holds rules for phones only.
    phone_rules: bool,
    pub stats: SheetStats,
}

impl Sheet {
    /// The built-in sheet and nothing else yet.
    pub fn new() -> Sheet {
        let mut s = Sheet {
            budget: MATCH_BUDGET,
            candidates: CANDIDATE_BUDGET,
            phone_budget: MATCH_BUDGET / 4,
            phone_candidates: CANDIDATE_BUDGET / 4,
            ..Sheet::default()
        };
        s.parse(UA, true, (true, true));
        s.ua_rules = s.rules.len() as u32;
        s
    }

    /// Whether the email has rules for phones only (`@media (max-width: 600px)`), which
    /// [`Sheet::cascade`] leaves out and [`Sheet::cascade_phone`] reads.
    pub fn has_phone_rules(&self) -> bool {
        self.phone_rules
    }

    /// Adds one `<style>` element's text; `media` is its `media` attribute, if any.
    pub fn add(&mut self, css: &str, media: Option<&str>) {
        let on = media.map_or((true, true), |m| (media_matches_at(m, VIEWPORT), media_matches_at(m, NARROW)));
        if on.0 || on.1 {
            self.parse(css, false, on);
        }
    }

    fn parse(&mut self, css: &str, ua: bool, on: (bool, bool)) {
        let css = strip_comments(css);
        self.rules_in(&css, ua, 0, on);
    }

    /// The rules in `s`, under media that hold on the desktop pane and on a phone (`on`).
    fn rules_in(&mut self, s: &str, ua: bool, depth: u32, on: (bool, bool)) {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            // Whitespace and the HTML comment markers Outlook wraps sheets in.
            if b[i].is_ascii_whitespace() || b[i] == b';' {
                i += 1;
                continue;
            }
            if s[i..].starts_with("<!--") {
                i += 4;
                continue;
            }
            if s[i..].starts_with("-->") {
                i += 3;
                continue;
            }
            let Some((prelude_end, ender)) = find_top(b, i, b"{;") else { break };
            let prelude = s[i..prelude_end].trim();
            if ender == b';' {
                // `@import`, `@charset`, `@namespace`, or junk: skip it.
                i = prelude_end + 1;
                continue;
            }
            let Some(close) = matching_brace(b, prelude_end) else { break };
            let block = &s[prelude_end + 1..close];
            i = close + 1;
            if let Some(at) = prelude.strip_prefix('@') {
                let name: String = at.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>().to_ascii_lowercase();
                let query = &at[name.len()..];
                let inner = (on.0 && media_matches_at(query, VIEWPORT), on.1 && media_matches_at(query, NARROW));
                if name == "media" && depth < 4 && (inner.0 || inner.1) {
                    self.rules_in(block, ua, depth + 1, inner);
                }
                // @supports (kinetic email behind feature checks), @font-face, @keyframes,
                // @page, @document: nothing here draws without them.
                continue;
            }
            if let Some(screen) = Screen::of(on.0, on.1) {
                self.rule(prelude, block, ua, screen);
            }
        }
    }

    fn rule(&mut self, prelude: &str, block: &str, ua: bool, screen: Screen) {
        // Phone rules never draw here: what they hold isn't counted against the score.
        let counted = screen != Screen::Phone;
        let mut decls = Vec::new();
        parse_decls(block, &mut decls);
        let mut sels = Vec::new();
        let mut generated = false;
        for part in split_top(prelude, b',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match parse_selector(part) {
                Ok(Some(sel)) => sels.push(sel),
                Ok(None) => generated |= is_generated(part),
                Err(()) => self.stats.bad_selectors += u32::from(counted),
            }
        }
        if counted && generated && decls.iter().any(|d| &*d.name == "content" && !matches!(&*d.value, "none" | "normal" | "\"\"" | "''")) {
            self.stats.generated += 1;
        }
        if decls.is_empty() || sels.is_empty() {
            return;
        }
        let block = self.blocks.len() as u32;
        self.blocks.push(decls);
        self.order += 1;
        self.phone_rules |= screen == Screen::Phone;
        for sel in sels {
            if self.rules.len() >= MAX_RULES {
                self.stats.dropped_rules += 1;
                continue;
            }
            let spec = specificity(&sel);
            let idx = self.rules.len() as u32;
            let subject = &sel.parts[0];
            if let Some(id) = &subject.id {
                self.by_id.entry(id.clone()).or_default().push(idx);
            } else if let Some(c) = subject.classes.first() {
                self.by_class.entry(c.clone()).or_default().push(idx);
            } else if let Some(t) = &subject.tag {
                self.by_tag.entry(t.clone()).or_default().push(idx);
            } else {
                self.universal.push(idx);
            }
            self.need.push(needs(&sel));
            self.rules.push(Rule { sel, spec, order: self.order, block, ua, screen });
        }
    }

    /// Element `i`'s declarations in cascade order, lowest priority first: look a property up
    /// from the end ([`get`]). On the desktop pane: rules for phones only are left out.
    pub fn cascade(&mut self, els: &[Elem], i: u32, inline: Option<&str>) -> Styles {
        self.cascade_on(els, i, inline, Screen::Phone)
    }

    /// [`Sheet::cascade`] as a phone ([`NARROW`]) would have it: its rules for phones in, those
    /// for the desktop pane only out, in the same cascade. Its matching has budgets of its own
    /// (a quarter of the main ones), and running out of them isn't counted against the score.
    pub fn cascade_phone(&mut self, els: &[Elem], i: u32, inline: Option<&str>) -> Styles {
        std::mem::swap(&mut self.budget, &mut self.phone_budget);
        std::mem::swap(&mut self.candidates, &mut self.phone_candidates);
        let exhausted = self.stats.exhausted;
        let st = self.cascade_on(els, i, inline, Screen::Desktop);
        self.stats.exhausted = exhausted;
        std::mem::swap(&mut self.budget, &mut self.phone_budget);
        std::mem::swap(&mut self.candidates, &mut self.phone_candidates);
        st
    }

    /// The cascade without the rules for `skip` only.
    fn cascade_on(&mut self, els: &[Elem], i: u32, inline: Option<&str>, skip: Screen) -> Styles {
        let e = &els[i as usize];
        // Each rule sits in one bucket, and an element's classes are distinct: no rule comes
        // up twice. Once a budget runs out only the built-in sheet applies (its rules come
        // first in every bucket).
        let empty: &[u32] = &[];
        let id = e.id.as_ref().and_then(|id| self.by_id.get(id)).map_or(empty, |v| v.as_slice());
        let tag = self.by_tag.get(&e.tag).map_or(empty, |v| v.as_slice());
        let ua_only = self.budget == 0 || self.candidates == 0;
        if ua_only {
            self.stats.exhausted |= self.ua_rules < self.rules.len() as u32;
        }
        let ua_rules = self.ua_rules;
        let mut left = self.candidates;
        let mut cands: Vec<u32> = Vec::new();
        let mut take = |bucket: &[u32]| {
            // Buckets are in rule order, built-in rules first.
            let bucket = if ua_only { &bucket[..bucket.partition_point(|&r| r < ua_rules)] } else { bucket };
            for &r in bucket {
                if r >= ua_rules {
                    if left == 0 {
                        break;
                    }
                    left -= 1;
                }
                if bloom_has(&e.ancestors, &self.need[r as usize]) {
                    cands.push(r);
                }
            }
        };
        take(id);
        for c in &e.classes {
            if let Some(v) = self.by_class.get(c) {
                take(v);
            }
        }
        take(tag);
        take(&self.universal);
        self.candidates = left;
        // (layer, specificity, order, block)
        let mut hits: Vec<(u8, u32, u32, u32)> = Vec::new();
        for r in cands {
            let rule = &self.rules[r as usize];
            if rule.screen == skip {
                continue;
            }
            if !rule.ua && self.budget == 0 {
                self.stats.exhausted = true;
                continue;
            }
            let mut budget = if rule.ua { u32::MAX } else { self.budget };
            let ok = match_from(&rule.sel, 0, els, i, &mut budget);
            if !rule.ua {
                self.budget = budget;
            }
            if ok {
                hits.push((if rule.ua { 0 } else { 1 }, rule.spec, rule.order, rule.block));
            }
        }
        hits.sort_unstable();
        let mut inline_decls = Vec::new();
        if let Some(s) = inline {
            parse_decls(s, &mut inline_decls);
        }
        // (layer, specificity, order, position in its block, declaration)
        type Ranked = (u8, u32, u32, usize, (Rc<str>, Rc<str>));
        let mut normal: Styles = Vec::new();
        let mut important: Vec<Ranked> = Vec::new();
        // Highest priority first, so a cap keeps what wins.
        for (layer, spec, order, block) in hits.iter().rev() {
            if normal.len() + important.len() >= MAX_ELEMENT_DECLS {
                break;
            }
            for (k, d) in self.blocks[*block as usize].iter().enumerate().rev() {
                if d.important {
                    important.push((*layer, *spec, *order, k, (d.name.clone(), d.value.clone())));
                } else {
                    normal.push((d.name.clone(), d.value.clone()));
                }
            }
        }
        normal.reverse();
        for (k, d) in inline_decls.iter().enumerate() {
            if d.important {
                important.push((2, u32::MAX, u32::MAX, k, (d.name.clone(), d.value.clone())));
            } else {
                normal.push((d.name.clone(), d.value.clone()));
            }
        }
        important.sort_by_key(|a| (a.0, a.1, a.2, a.3));
        normal.extend(important.into_iter().map(|t| t.4));
        // Only each property's last declaration is ever read: drop the rest, so lookups scan
        // a few dozen entries at most.
        let mut seen: std::collections::HashSet<Rc<str>> = std::collections::HashSet::new();
        let mut out: Styles = normal.into_iter().rev().filter(|(k, _)| seen.insert(k.clone())).collect();
        out.reverse();
        out
    }
}

/// The winning value of `name`, or None (also for `inherit`, `initial` and `unset`: the
/// parent's value carries on).
pub fn get<'a>(st: &'a [(Rc<str>, Rc<str>)], name: &str) -> Option<&'a str> {
    st.iter().rev().find(|(k, _)| &**k == name).map(|(_, v)| &**v).filter(|v| !matches!(*v, "inherit" | "initial" | "unset" | "revert"))
}

/// Declarations in a `style=""` attribute or a rule's block, shorthands expanded.
fn parse_decls(block: &str, out: &mut Vec<Decl>) {
    let start = out.len();
    parse_decls_all(block, out);
    // Within a block one declaration per property counts: the last `!important` one, else
    // the last. Keep just that, in its place.
    let mut seen: HashMap<Rc<str>, bool> = HashMap::new();
    let mut keep = vec![false; out.len() - start];
    for (k, d) in out[start..].iter().enumerate().rev() {
        match seen.get(&d.name) {
            None => {
                seen.insert(d.name.clone(), d.important);
                keep[k] = true;
            }
            Some(false) if d.important => {
                // An earlier !important beats the later normal one kept so far.
                seen.insert(d.name.clone(), true);
                if let Some(j) = out[start..].iter().enumerate().skip(k + 1).find(|(j, x)| keep[*j] && x.name == d.name).map(|(j, _)| j) {
                    keep[j] = false;
                }
                keep[k] = true;
            }
            Some(_) => {}
        }
    }
    let mut k = 0;
    out.retain(|_| {
        let r = k < start || keep[k - start];
        k += 1;
        r
    });
}

fn parse_decls_all(block: &str, out: &mut Vec<Decl>) {
    for d in split_top(block, b';').into_iter().take(MAX_DECLS) {
        let Some((k, v)) = d.split_once(':') else { continue };
        let name = k.trim().to_ascii_lowercase();
        if name.is_empty() || name.bytes().any(|c| !(c.is_ascii_alphanumeric() || c == b'-')) {
            continue;
        }
        let mut v = v.trim();
        let mut important = false;
        if let Some(bang) = v.rfind('!')
            && v[bang + 1..].trim().eq_ignore_ascii_case("important")
        {
            important = true;
            v = v[..bang].trim();
        }
        if v.is_empty() {
            continue;
        }
        expand(&name, &lower_keep_urls(v), important, out);
    }
}

fn push(out: &mut Vec<Decl>, name: &str, value: &str, important: bool) {
    if known(name) {
        out.push(Decl { name: name.into(), value: value.into(), important });
    }
}

/// The properties the renderer reads: the rest (`mso-*`, `-webkit-*`, `-premailer-*`,
/// transitions…) are dropped as they're read, so lookups scan short lists. (Everything is kept
/// in this crate's unit tests, which use made-up names.)
fn known(name: &str) -> bool {
    cfg!(test)
        || matches!(
            name,
            "color" | "background-color" | "background-image" | "display" | "visibility" | "opacity" | "overflow" | "height" | "max-height" | "width" | "max-width" | "min-width"
                | "padding-top" | "padding-right" | "padding-bottom" | "padding-left" | "margin-top" | "margin-right" | "margin-bottom" | "margin-left"
                | "border-top-width" | "border-right-width" | "border-bottom-width" | "border-left-width" | "border-top-style" | "border-right-style"
                | "border-bottom-style" | "border-left-style" | "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color"
                | "border-radius" | "border-top-left-radius" | "border-collapse" | "border-spacing" | "font-family" | "font-size" | "font-style" | "font-weight"
                | "line-height" | "letter-spacing" | "text-align" | "text-decoration" | "text-decoration-line" | "text-transform" | "vertical-align" | "white-space"
                | "list-style" | "list-style-type" | "float" | "position" | "transform" | "clip-path" | "mix-blend-mode" | "filter" | "columns" | "column-count" | "content"
        )
}

const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];

/// `a`, `a b`, `a b c`, `a b c d` → top, right, bottom, left.
fn four(v: &str) -> Option<[String; 4]> {
    let t: Vec<String> = split_ws(v);
    Some(match t.as_slice() {
        [a] => [a.clone(), a.clone(), a.clone(), a.clone()],
        [a, b] => [a.clone(), b.clone(), a.clone(), b.clone()],
        [a, b, c] => [a.clone(), b.clone(), c.clone(), b.clone()],
        [a, b, c, d, ..] => [a.clone(), b.clone(), c.clone(), d.clone()],
        _ => return None,
    })
}

fn expand(name: &str, v: &str, imp: bool, out: &mut Vec<Decl>) {
    let wide = matches!(v, "inherit" | "initial" | "unset" | "revert");
    match name {
        "margin" | "padding" => {
            if wide {
                for s in SIDES {
                    push(out, &format!("{name}-{s}"), v, imp);
                }
            } else if let Some(vals) = four(v) {
                for (s, x) in SIDES.iter().zip(vals) {
                    push(out, &format!("{name}-{s}"), &x, imp);
                }
            }
        }
        "border" => {
            let (w, st, c) = border_parts(v);
            for s in SIDES {
                push(out, &format!("border-{s}-width"), &w, imp);
                push(out, &format!("border-{s}-style"), &st, imp);
                push(out, &format!("border-{s}-color"), &c, imp);
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let (w, st, c) = border_parts(v);
            push(out, &format!("{name}-width"), &w, imp);
            push(out, &format!("{name}-style"), &st, imp);
            push(out, &format!("{name}-color"), &c, imp);
        }
        "border-width" | "border-style" | "border-color" => {
            let what = &name[7..];
            if let Some(vals) = four(v) {
                for (s, x) in SIDES.iter().zip(vals) {
                    push(out, &format!("border-{s}-{what}"), &x, imp);
                }
            }
        }
        "background" => {
            let mut color = "transparent".to_owned();
            let mut image = "none".to_owned();
            for t in split_ws(v) {
                if t.starts_with("url(") || t.contains("gradient(") {
                    image = t;
                } else if is_color(&t) {
                    color = t;
                }
            }
            if wide {
                (color, image) = (v.to_owned(), v.to_owned());
            }
            push(out, "background-color", &color, imp);
            push(out, "background-image", &image, imp);
        }
        "font" => {
            // [style] [variant] [weight] size[/line-height] family
            let toks = split_ws(v);
            // The size has a unit or a `/`; a bare number before it is a weight.
            let is_size = |t: &str| {
                (t.starts_with(|c: char| c.is_ascii_digit() || c == '.') && t.parse::<u32>().is_err())
                    || matches!(t.split('/').next(), Some("small" | "medium" | "large" | "x-small" | "x-large" | "xx-large" | "smaller" | "larger"))
            };
            let Some(size_at) = toks.iter().position(|t| is_size(t)) else {
                return;
            };
            for t in &toks[..size_at] {
                match t.as_str() {
                    "italic" | "oblique" => push(out, "font-style", "italic", imp),
                    "bold" | "bolder" | "lighter" | "normal" => push(out, "font-weight", t, imp),
                    _ if t.parse::<u32>().is_ok() => push(out, "font-weight", t, imp),
                    _ => {}
                }
            }
            let mut size = toks[size_at].clone();
            let mut rest = toks[size_at + 1..].to_vec();
            if let Some((s, lh)) = size.clone().split_once('/') {
                size = s.to_owned();
                if lh.is_empty() {
                    if !rest.is_empty() {
                        push(out, "line-height", &rest.remove(0), imp);
                    }
                } else {
                    push(out, "line-height", lh, imp);
                }
            } else if rest.first().is_some_and(|t| t.starts_with('/')) {
                let t = rest.remove(0);
                let lh = t.trim_start_matches('/');
                if lh.is_empty() {
                    if !rest.is_empty() {
                        push(out, "line-height", &rest.remove(0), imp);
                    }
                } else {
                    push(out, "line-height", lh, imp);
                }
            }
            push(out, "font-size", &size, imp);
            if !rest.is_empty() {
                push(out, "font-family", &rest.join(" "), imp);
            }
        }
        _ => push(out, name, v, imp),
    }
}

/// `1px solid #ccc` (any order) → width, style, colour.
fn border_parts(v: &str) -> (String, String, String) {
    let mut w = "medium".to_owned();
    let mut st = "none".to_owned();
    let mut c = "currentcolor".to_owned();
    for t in split_ws(v) {
        if matches!(t.as_str(), "none" | "hidden" | "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset") {
            st = t;
        } else if t.starts_with(|ch: char| ch.is_ascii_digit() || ch == '.') || matches!(t.as_str(), "thin" | "medium" | "thick") {
            w = t;
        } else if is_color(&t) {
            c = t;
        }
    }
    (w, st, c)
}

fn is_color(t: &str) -> bool {
    t.starts_with('#') || t.starts_with("rgb") || t.starts_with("hsl") || super::parse_color(t).is_some()
}

/// Lowercases a value, except what's inside `url(…)` (paths are case-sensitive). One pass:
/// only ASCII bytes change, so the result stays valid UTF-8 and the same length.
fn lower_keep_urls(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_url = false;
    while i < b.len() {
        if in_url {
            out.push(b[i]);
            in_url = b[i] != b')';
            i += 1;
        } else if b.len() - i >= 4 && b[i..i + 4].eq_ignore_ascii_case(b"url(") {
            out.extend_from_slice(b"url(");
            in_url = true;
            i += 4;
        } else {
            out.push(b[i].to_ascii_lowercase());
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Splits on whitespace outside parentheses and quotes.
fn split_ws(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in v.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), _) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '(') => {
                depth += 1;
                cur.push(c);
            }
            (None, ')') => {
                depth -= 1;
                cur.push(c);
            }
            (None, c) if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, ',') if depth <= 0 => {
                // `font-family: a, b` stays one token per name; commas split nothing else here.
                cur.push(c);
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Splits on `sep` outside parentheses, brackets and quotes.
fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote = 0u8;
    for (i, &c) in b.iter().enumerate() {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = c,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if c == sep && depth <= 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The first of `stops` at or after `from` outside parentheses and quotes.
fn find_top(b: &[u8], from: usize, stops: &[u8]) -> Option<(usize, u8)> {
    let mut depth = 0i32;
    let mut quote = 0u8;
    for (i, &c) in b.iter().enumerate().skip(from) {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = c,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth <= 0 && stops.contains(&c) => return Some((i, c)),
            _ => {}
        }
    }
    None
}

/// The `}` closing the `{` at `open`.
fn matching_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote = 0u8;
    for (i, &c) in b.iter().enumerate().skip(open) {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = c,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        match rest[at + 2..].find("*/") {
            Some(end) => rest = &rest[at + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

// ── media queries ──

/// Whether a media query list applies to a screen `width` points wide in light mode.
pub fn media_matches_at(list: &str, width: f32) -> bool {
    let list = list.trim();
    list.is_empty() || split_top(list, b',').iter().any(|q| query_matches(q, width))
}

fn query_matches(q: &str, width: f32) -> bool {
    let q = q.trim().to_ascii_lowercase();
    let (negate, q) = match q.strip_prefix("not ") {
        Some(r) => (true, r.trim().to_owned()),
        None => (false, q.strip_prefix("only ").map(str::trim).unwrap_or(&q).to_owned()),
    };
    let mut ok = true;
    for part in q.split(" and ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let this = if let Some(f) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            feature_matches(f, width)
        } else {
            matches!(part, "all" | "screen")
        };
        ok &= this;
    }
    ok != negate
}

fn feature_matches(f: &str, width: f32) -> bool {
    let (name, value) = match f.split_once(':') {
        Some((n, v)) => (n.trim(), v.trim()),
        None => (f.trim(), ""),
    };
    let px = || super::length(value, 16.0);
    match name {
        "min-width" | "min-device-width" => px().is_some_and(|w| width >= w),
        "max-width" | "max-device-width" => px().is_some_and(|w| width <= w),
        "width" | "device-width" => px().is_some_and(|w| (width - w).abs() < 0.5),
        "prefers-color-scheme" => value == "light",
        "orientation" => value == "landscape",
        "hover" => value == "hover" || value.is_empty(),
        "pointer" => value == "fine" || value.is_empty(),
        "color" => true,
        "-webkit-min-device-pixel-ratio" | "min--moz-device-pixel-ratio" | "min-device-pixel-ratio" => value.parse::<f32>().is_ok_and(|r| r <= 1.0),
        "-webkit-max-device-pixel-ratio" | "max-device-pixel-ratio" => value.parse::<f32>().is_ok_and(|r| r >= 1.0),
        "min-resolution" => value.trim_end_matches("dppx").trim_end_matches("x").parse::<f32>().is_ok_and(|r| r <= 1.0),
        _ => false,
    }
}

// ── selectors ──

fn is_generated(sel: &str) -> bool {
    let l = sel.to_ascii_lowercase();
    l.contains(":before") || l.contains(":after")
}

/// Ok(None): a valid selector that can never match here (a pseudo-element or a dynamic
/// state). Err: one we can't read.
fn parse_selector(s: &str) -> Result<Option<Selector>, ()> {
    if s.len() > MAX_SELECTOR_LEN {
        return Err(());
    }
    let mut parts: Vec<Compound> = Vec::new();
    let mut combs: Vec<Comb> = Vec::new();
    let mut never = false;
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut pending: Option<Comb> = None;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            if !parts.is_empty() && pending.is_none() {
                pending = Some(Comb::Descendant);
            }
            i += 1;
            continue;
        }
        if matches!(c, '>' | '+' | '~') {
            if parts.is_empty() {
                return Err(());
            }
            pending = Some(match c {
                '>' => Comb::Child,
                '+' => Comb::Adjacent,
                _ => Comb::Sibling,
            });
            i += 1;
            continue;
        }
        let (comp, n, nv) = parse_compound(&b[i..], false)?;
        if n == 0 || parts.len() >= MAX_COMPOUNDS {
            return Err(());
        }
        never |= nv;
        i += n;
        if !parts.is_empty() {
            combs.push(pending.take().unwrap_or(Comb::Descendant));
        }
        pending = None;
        parts.push(comp);
    }
    if parts.is_empty() || pending.is_some() {
        return Err(());
    }
    if never {
        return Ok(None);
    }
    parts.reverse();
    combs.reverse();
    Ok(Some(Selector { parts, combs }))
}

fn ident(b: &[char], mut i: usize) -> (String, usize) {
    let mut s = String::new();
    while i < b.len() {
        let c = b[i];
        if c == '\\' && i + 1 < b.len() {
            s.push(b[i + 1]);
            i += 2;
        } else if c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
            s.push(c.to_ascii_lowercase());
            i += 1;
        } else {
            break;
        }
    }
    (s, i)
}

/// One compound selector at the start of `b`: (it, chars read, never matches). Inside a
/// `:not(…)` (`negated`) another `:not` is refused, so this never recurses more than once.
fn parse_compound(b: &[char], negated: bool) -> Result<(Compound, usize, bool), ()> {
    let mut c = Compound::default();
    let mut i = 0;
    let mut never = false;
    if i < b.len() && b[i] == '*' {
        i += 1;
    } else if i < b.len() && (b[i].is_alphabetic() || b[i] == '_') {
        let (t, n) = ident(b, i);
        c.tag = Some(t);
        i = n;
    }
    while i < b.len() {
        match b[i] {
            '.' => {
                let (cl, n) = ident(b, i + 1);
                if cl.is_empty() {
                    return Err(());
                }
                c.classes.push(cl);
                i = n;
            }
            '#' => {
                let (id, n) = ident(b, i + 1);
                if id.is_empty() {
                    return Err(());
                }
                c.id = Some(id);
                i = n;
            }
            '[' => {
                let end = b[i..].iter().position(|&x| x == ']').ok_or(())? + i;
                let inner: String = b[i + 1..end].iter().collect();
                c.attrs.push(parse_attr(&inner)?);
                i = end + 1;
            }
            ':' => {
                let element = b.get(i + 1) == Some(&':');
                let start = if element { i + 2 } else { i + 1 };
                let (name, mut n) = ident(b, start);
                let mut arg = String::new();
                if b.get(n) == Some(&'(') {
                    let mut depth = 0;
                    let mut j = n;
                    while j < b.len() {
                        match b[j] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    if j >= b.len() || j - n > MAX_PSEUDO_ARG {
                        return Err(());
                    }
                    arg = b[n + 1..j].iter().collect();
                    n = j + 1;
                }
                i = n;
                if element {
                    never = true;
                    continue;
                }
                match name.as_str() {
                    "first-child" => c.pseudos.push(Pseudo::FirstChild),
                    "last-child" => c.pseudos.push(Pseudo::LastChild),
                    "only-child" => c.pseudos.push(Pseudo::OnlyChild),
                    "root" => c.pseudos.push(Pseudo::Root),
                    "link" | "any-link" => c.pseudos.push(Pseudo::Link),
                    "nth-child" => {
                        let (a, bb) = parse_nth(&arg).ok_or(())?;
                        c.pseudos.push(Pseudo::Nth(a, bb));
                    }
                    "not" if !negated => {
                        let inner: Vec<char> = arg.trim().chars().collect();
                        let (comp, n, nv) = parse_compound(&inner, true)?;
                        if n != inner.len() || nv {
                            return Err(());
                        }
                        c.pseudos.push(Pseudo::Not(Box::new(comp)));
                    }
                    // Old single-colon pseudo-elements, dynamic and form states: never here.
                    "before" | "after" | "first-letter" | "first-line" | "hover" | "active" | "focus" | "focus-within" | "focus-visible"
                    | "visited" | "target" | "checked" | "disabled" | "enabled" | "empty" | "placeholder-shown" | "selection" => never = true,
                    _ => return Err(()),
                }
            }
            _ => break,
        }
    }
    Ok((c, i, never))
}

fn parse_attr(s: &str) -> Result<AttrSel, ()> {
    let s = s.trim();
    let ops = [("~=", AttrOp::Word), ("|=", AttrOp::Dash), ("^=", AttrOp::Prefix), ("$=", AttrOp::Suffix), ("*=", AttrOp::Contains), ("=", AttrOp::Eq)];
    for (tok, op) in ops {
        if let Some((name, value)) = s.split_once(tok) {
            let mut value = value.trim();
            let mut ci = false;
            if let Some(v) = value.strip_suffix(" i").or_else(|| value.strip_suffix(" I")) {
                value = v.trim();
                ci = true;
            } else if let Some(v) = value.strip_suffix(" s") {
                value = v.trim();
            }
            let value = value.trim_matches(|c| c == '"' || c == '\'');
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                return Err(());
            }
            let value = if ci { value.to_lowercase() } else { value.to_owned() };
            return Ok(AttrSel { name, op, value, ci });
        }
    }
    if s.is_empty() || s.contains(char::is_whitespace) {
        return Err(());
    }
    Ok(AttrSel { name: s.to_ascii_lowercase(), op: AttrOp::Exists, value: String::new(), ci: false })
}

/// `odd`, `even`, `3`, `2n+1`, `-n+3` → (a, b).
fn parse_nth(s: &str) -> Option<(i32, i32)> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some((a, b)) = s.split_once('n') {
        let a = match a {
            "" | "+" => 1,
            "-" => -1,
            _ => a.parse().ok()?,
        };
        let b = if b.is_empty() { 0 } else { b.parse().ok()? };
        Some((a, b))
    } else {
        Some((0, s.parse().ok()?))
    }
}

fn specificity(sel: &Selector) -> u32 {
    fn comp(c: &Compound) -> (u32, u32, u32) {
        let (mut a, mut b, mut t) = (c.id.is_some() as u32, (c.classes.len() + c.attrs.len()) as u32, c.tag.is_some() as u32);
        for p in &c.pseudos {
            if let Pseudo::Not(inner) = p {
                let (x, y, z) = comp(inner);
                a += x;
                b += y;
                t += z;
            } else {
                b += 1;
            }
        }
        (a, b, t)
    }
    let (mut a, mut b, mut c) = (0, 0, 0);
    for p in &sel.parts {
        let (x, y, z) = comp(p);
        a += x;
        b += y;
        c += z;
    }
    a.min(255) << 16 | b.min(255) << 8 | c.min(255)
}

fn compound_matches(c: &Compound, els: &[Elem], i: u32) -> bool {
    let e = &els[i as usize];
    if c.tag.as_ref().is_some_and(|t| *t != e.tag) {
        return false;
    }
    if c.id.as_ref().is_some_and(|id| e.id.as_ref() != Some(id)) {
        return false;
    }
    if !c.classes.iter().all(|cl| e.classes.contains(cl)) {
        return false;
    }
    for a in &c.attrs {
        let Some(v) = e.attr(&a.name) else { return false };
        let v = if a.ci { v.to_lowercase() } else { v.to_owned() };
        let ok = match a.op {
            AttrOp::Exists => true,
            AttrOp::Eq => v == a.value,
            AttrOp::Word => v.split_whitespace().any(|w| w == a.value),
            AttrOp::Dash => v == a.value || v.starts_with(&format!("{}-", a.value)),
            AttrOp::Prefix => !a.value.is_empty() && v.starts_with(&a.value),
            AttrOp::Suffix => !a.value.is_empty() && v.ends_with(&a.value),
            AttrOp::Contains => !a.value.is_empty() && v.contains(&a.value),
        };
        if !ok {
            return false;
        }
    }
    for p in &c.pseudos {
        let ok = match p {
            Pseudo::FirstChild => e.index == 1,
            Pseudo::LastChild => e.last,
            Pseudo::OnlyChild => e.index == 1 && e.last,
            Pseudo::Root => e.parent == NONE,
            Pseudo::Link => matches!(e.tag.as_str(), "a" | "area") && e.attr("href").is_some(),
            Pseudo::Nth(a, b) => {
                // index = a·k + b for some k ≥ 0. In i64 nothing here can overflow: index is a
                // u32, a and b are i32.
                let (a, n) = (*a as i64, e.index as i64 - *b as i64);
                if a == 0 { n == 0 } else { n % a == 0 && n / a >= 0 }
            }
            Pseudo::Not(inner) => !compound_matches(inner, els, i),
        };
        if !ok {
            return false;
        }
    }
    true
}

fn match_from(sel: &Selector, k: usize, els: &[Elem], i: u32, budget: &mut u32) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if !compound_matches(&sel.parts[k], els, i) {
        return false;
    }
    if k + 1 == sel.parts.len() {
        return true;
    }
    let e = &els[i as usize];
    match sel.combs[k] {
        Comb::Child => e.parent != NONE && match_from(sel, k + 1, els, e.parent, budget),
        Comb::Adjacent => e.prev != NONE && match_from(sel, k + 1, els, e.prev, budget),
        Comb::Descendant => {
            let mut p = e.parent;
            while p != NONE {
                if match_from(sel, k + 1, els, p, budget) {
                    return true;
                }
                if *budget == 0 {
                    return false;
                }
                p = els[p as usize].parent;
            }
            false
        }
        Comb::Sibling => {
            let mut p = e.prev;
            while p != NONE {
                if match_from(sel, k + 1, els, p, budget) {
                    return true;
                }
                if *budget == 0 {
                    return false;
                }
                p = els[p as usize].prev;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny tree: html > body > div.a#x > (p.b, p.c[data-k=v]) and the p's spans.
    fn tree() -> Vec<Elem> {
        let el = |tag: &str, id: Option<&str>, classes: &[&str], attrs: &[(&str, &str)], parent: u32, prev: u32, index: u32, last: bool| Elem {
            tag: tag.into(),
            id: id.map(Into::into),
            classes: classes.iter().map(|c| c.to_string()).collect(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            parent,
            prev,
            index,
            last,
            ancestors: [!0; 8],
        };
        vec![
            el("html", None, &[], &[], NONE, NONE, 1, true),
            el("body", None, &[], &[], 0, NONE, 1, true),
            el("div", Some("x"), &["a"], &[], 1, NONE, 1, true),
            el("p", None, &["b"], &[], 2, NONE, 1, false),
            el("p", None, &["c"], &[("data-k", "v"), ("style", "margin: 16px 0")], 2, 3, 2, true),
            el("span", None, &[], &[], 4, NONE, 1, true),
        ]
    }

    fn value(sheet: &mut Sheet, els: &[Elem], i: u32, inline: Option<&str>, prop: &str) -> Option<String> {
        get(&sheet.cascade(els, i, inline), prop).map(str::to_owned)
    }

    #[test]
    fn specificity_then_order_then_importance_then_inline() {
        let els = tree();
        let mut s = Sheet::new();
        s.add("p { color: red } .c { color: blue } p { color: green } #x p { color: black } .b { color: gray !important }", None);
        assert_eq!(value(&mut s, &els, 3, None, "color").as_deref(), Some("gray"), "!important beats specificity");
        assert_eq!(value(&mut s, &els, 4, None, "color").as_deref(), Some("black"), "id beats class beats type");
        assert_eq!(value(&mut s, &els, 4, Some("color: #123"), "color").as_deref(), Some("#123"), "inline beats any normal rule");
        let mut s = Sheet::new();
        s.add(".c { color: blue !important }", None);
        assert_eq!(value(&mut s, &els, 4, Some("color: #123"), "color").as_deref(), Some("blue"), "sheet !important beats inline");
        assert_eq!(value(&mut s, &els, 4, Some("color: #123 !important"), "color").as_deref(), Some("#123"), "inline !important beats it");
    }

    #[test]
    fn combinators_attributes_and_pseudos() {
        let els = tree();
        let mut s = Sheet::new();
        s.add(
            "div > p { a: 1 } body > p { b: 1 } .b + p { c: 1 } .b ~ span { d: 1 } html span { e: 1 } [data-k=v] { f: 1 }
             div[style*=\"margin: 16px 0\"] { g: 1 } p[style*=\"margin: 16px 0\"] { g: 2 } p:first-child { h: 1 } p:last-child { i: 1 }
             p:not(.b) { j: 1 } p:nth-child(2n) { k: 1 } :root { l: 1 } a:hover, .c { m: 1 } p::after { content: 'x' } td.columns .column:not([class*=large-offset]) { n: 1 }",
            None,
        );
        let has = |s: &mut Sheet, i: u32, p: &str| value(s, &els, i, None, p).is_some();
        assert!(has(&mut s, 4, "a") && !has(&mut s, 4, "b"));
        assert!(has(&mut s, 4, "c") && !has(&mut s, 3, "c"));
        assert!(!has(&mut s, 5, "d"), "span isn't a sibling of .b");
        assert!(has(&mut s, 5, "e"));
        assert!(has(&mut s, 4, "f") && !has(&mut s, 3, "f"));
        assert_eq!(value(&mut s, &els, 4, None, "g").as_deref(), Some("2"));
        assert!(has(&mut s, 3, "h") && !has(&mut s, 4, "h"));
        assert!(has(&mut s, 4, "i") && !has(&mut s, 3, "i"));
        assert!(has(&mut s, 4, "j") && !has(&mut s, 3, "j"));
        assert!(has(&mut s, 4, "k") && !has(&mut s, 3, "k"));
        assert!(has(&mut s, 0, "l") && !has(&mut s, 1, "l"));
        assert!(has(&mut s, 4, "m"), "a selector list keeps its readable selectors");
        assert_eq!(s.stats.generated, 1);
        assert_eq!(s.stats.bad_selectors, 0);
    }

    #[test]
    fn media_queries_are_desktop_and_light() {
        let els = tree();
        let mut s = Sheet::new();
        s.add(
            "@media only screen and (max-width: 600px) { p { a: mobile } }
             @media only screen and (min-width: 480px) { p { b: wide } }
             @media (prefers-color-scheme: dark) { p { c: dark } }
             @media print { p { d: print } }
             @media screen and (min-width: 480px), print { p { e: either } }
             @supports (display: grid) { p { f: 1 } }
             @font-face { font-family: X; src: url(x.woff) }
             @import url(https://fonts.example.com/css);
             p { g: 1 }",
            None,
        );
        assert_eq!(value(&mut s, &els, 3, None, "a"), None);
        assert_eq!(value(&mut s, &els, 3, None, "b").as_deref(), Some("wide"));
        assert_eq!(value(&mut s, &els, 3, None, "c"), None);
        assert_eq!(value(&mut s, &els, 3, None, "d"), None);
        assert_eq!(value(&mut s, &els, 3, None, "e").as_deref(), Some("either"));
        assert_eq!(value(&mut s, &els, 3, None, "f"), None);
        assert_eq!(value(&mut s, &els, 3, None, "g").as_deref(), Some("1"));
        let mut s = Sheet::new();
        s.add("p { a: 1 }", Some("print"));
        assert_eq!(value(&mut s, &els, 3, None, "a"), None, "<style media=print>");
    }

    #[test]
    fn phone_rules_are_kept_out_of_the_desktop_cascade_and_read_in_the_phones() {
        let els = tree();
        let mut s = Sheet::new();
        s.add(
            "@media screen and (max-width: 600px) { p { display: block; width: 100% } p::before { content: 'x' } }
             @media (max-width: 374px) { p { a: tiny } }
             @media (min-width: 480px) { p { b: wide } }
             @media print { p { e: 1 } }
             p { c: 1 }",
            None,
        );
        s.add("p { d: phone }", Some("(max-width: 480px)"));
        assert!(s.has_phone_rules());
        let desk = s.cascade(&els, 3, None);
        assert_eq!((get(&desk, "width"), get(&desk, "b"), get(&desk, "c"), get(&desk, "d")), (None, Some("wide"), Some("1"), None));
        let phone = s.cascade_phone(&els, 3, None);
        assert_eq!((get(&phone, "display"), get(&phone, "width"), get(&phone, "c"), get(&phone, "d")), (Some("block"), Some("100%"), Some("1"), Some("phone")));
        assert_eq!((get(&phone, "a"), get(&phone, "b"), get(&phone, "e")), (None, None, None), "not the smallest phones', the desktop's or print's");
        assert_eq!(s.stats.generated, 0, "a phone rule's generated content never draws here");
        let mut s = Sheet::new();
        s.add("@media (min-width: 480px) { p { b: wide } } @media print { p { e: 1 } }", None);
        assert!(!s.has_phone_rules());
    }

    #[test]
    fn shorthands_expand_in_order() {
        let els = tree();
        let mut s = Sheet::new();
        s.add("p { padding-top: 5px } .b { padding: 1px 2px 3px } .b { border: 2px solid #F00; border-left: none } .b { background: #FFF url('https://X.example/A.png') center / cover; font: italic bold 12px/30px Georgia, serif }", None);
        let st = s.cascade(&els, 3, Some("margin: 0 auto"));
        assert_eq!(get(&st, "padding-top"), Some("1px"), "a later shorthand beats an earlier longhand");
        assert_eq!(get(&st, "padding-left"), Some("2px"));
        assert_eq!(get(&st, "padding-bottom"), Some("3px"));
        assert_eq!(get(&st, "border-top-width"), Some("2px"));
        assert_eq!(get(&st, "border-top-color"), Some("#f00"));
        assert_eq!(get(&st, "border-left-style"), Some("none"));
        assert_eq!(get(&st, "background-color"), Some("#fff"));
        assert_eq!(get(&st, "background-image"), Some("url('https://X.example/A.png')"), "URLs keep their case");
        assert_eq!(get(&st, "font-size"), Some("12px"));
        assert_eq!(get(&st, "line-height"), Some("30px"));
        assert_eq!(get(&st, "font-weight"), Some("bold"));
        assert_eq!(get(&st, "font-style"), Some("italic"));
        assert_eq!(get(&st, "font-family"), Some("georgia, serif"));
        assert_eq!(get(&st, "margin-left"), Some("auto"));
    }

    #[test]
    fn outlook_sheets_and_junk_parse() {
        let els = tree();
        let mut s = Sheet::new();
        s.add("<!-- /* c */ p.b, li.b { margin: 0cm; font-size: 11.0pt } a:link, span.MsoHyperlink { color: #0563C1 } @page WordSection1 { size: 612pt 792pt } div.WordSection1 { page: WordSection1 } --> p { } ;;; }{ broken", None);
        assert_eq!(value(&mut s, &els, 3, None, "font-size").as_deref(), Some("11.0pt"));
        let mut s = Sheet::new();
        s.add("p > { a: 1 } .b { a: 2 } p:unknown-thing { a: 3 }", None);
        assert_eq!(value(&mut s, &els, 3, None, "a").as_deref(), Some("2"));
        assert_eq!(s.stats.bad_selectors, 2);
    }

    #[test]
    fn ancestor_filter_skips_impossible_rules() {
        // div.a > p: `.a p` can match, `.z p` can't and costs nothing.
        let mut a = [0u64; 8];
        bloom_add(&mut a, '.', "a");
        let els = vec![
            Elem { tag: "div".into(), id: None, classes: vec!["a".into()], attrs: vec![], parent: NONE, prev: NONE, index: 1, last: true, ancestors: [0; 8] },
            Elem { tag: "p".into(), id: None, classes: vec![], attrs: vec![], parent: 0, prev: NONE, index: 1, last: true, ancestors: a },
        ];
        let mut s = Sheet::new();
        for i in 0..1000 {
            s.add(&format!(".z{i} p {{ x: {i} }}"), None);
        }
        s.add(".a p { y: 1 }", None);
        let before = s.budget;
        let st = s.cascade(&els, 1, None);
        assert_eq!(get(&st, "y"), Some("1"));
        assert_eq!(get(&st, "x"), None);
        assert!(before - s.budget < 50, "spent {}", before - s.budget);
    }

    #[test]
    fn matching_is_bounded() {
        // A deep chain and a selector that backtracks through every ancestor at every step.
        let mut els = Vec::new();
        for i in 0..400u32 {
            els.push(Elem { tag: "div".into(), id: None, classes: vec![], attrs: vec![], parent: if i == 0 { NONE } else { i - 1 }, prev: NONE, index: 1, last: true, ancestors: [!0; 8] });
        }
        let mut s = Sheet::new();
        for _ in 0..50 {
            s.add(".x div div div div div div div div div { a: 1 }", None);
        }
        let t = std::time::Instant::now();
        for i in 0..els.len() as u32 {
            s.cascade(&els, i, None);
        }
        assert!(s.stats.exhausted, "the budget ran out");
        assert!(t.elapsed().as_secs_f64() < 2.0, "bounded: {:?}", t.elapsed());
    }

    #[test]
    fn duplicate_declarations_collapse_without_changing_the_cascade() {
        let els = tree();
        let mut s = Sheet::new();
        s.add(".b { color: red !important; color: blue; color: green } p { color: black !important } .b { padding: 1px; padding-top: 3px; padding: 2px }", None);
        let st = s.cascade(&els, 3, Some("color: pink; color: teal"));
        assert_eq!(get(&st, "color"), Some("red"), "!important in .b (more specific) beats p's");
        assert_eq!(get(&st, "padding-top"), Some("2px"));
        assert_eq!(st.iter().filter(|(k, _)| &**k == "color").count(), 1);
        let st = s.cascade(&els, 4, Some("color: pink; color: teal"));
        assert_eq!(get(&st, "color"), Some("black"), "p's !important beats inline normal");
    }

    #[test]
    fn nth_child_extremes_dont_overflow() {
        // Hostile `an+b` values at and past i32's ends must neither panic nor match wrongly.
        let els = tree();
        for arg in ["-n-2147483647", "-2147483648n-2147483648", "2147483647n+2147483647", "-n+2147483647", "-2147483648n", "-1n-1", "0n+0", "-0n-0", "2147483648", "n-2147483648"] {
            let mut s = Sheet::new();
            s.add(&format!("p:nth-child({arg}) {{ a: 1 }}"), None);
            for i in 0..els.len() as u32 {
                s.cascade(&els, i, None);
            }
        }
        let mut s = Sheet::new();
        s.add("p:nth-child(-n-2147483647) { a: 1 } p:nth-child(-n+1) { b: 1 } p:nth-child(-2147483648n+2) { c: 1 }", None);
        assert_eq!(value(&mut s, &els, 3, None, "a"), None);
        assert_eq!(value(&mut s, &els, 3, None, "b").as_deref(), Some("1"), "-n+1: the first");
        assert_eq!(value(&mut s, &els, 4, None, "b"), None);
        assert_eq!(value(&mut s, &els, 4, None, "c").as_deref(), Some("1"), "a·0+2: the second");
    }

    #[test]
    fn nested_not_is_refused_without_recursing() {
        // 24 KB of `:not(` overflowed a 2 MB stack and peaked near 235 MiB.
        let sel = format!("p{}a{}", ":not(".repeat(4000), ")".repeat(4000));
        let t = std::time::Instant::now();
        let mut s = Sheet::new();
        s.add(&format!("{sel} {{ a: 1 }} p:not(.b) {{ b: 1 }}"), None);
        assert!(t.elapsed().as_secs_f64() < 1.0, "{:?}", t.elapsed());
        assert_eq!(s.stats.bad_selectors, 1);
        let els = tree();
        assert_eq!(value(&mut s, &els, 4, None, "b").as_deref(), Some("1"), "a plain :not still works");
    }

    #[test]
    fn url_normalization_is_linear() {
        // 20,000 url()s in one value lowercased the rest of the value once per url(): 7.6 s.
        let v = "URL(https://A.example/X.png) Center ".repeat(20_000);
        let t = std::time::Instant::now();
        let out = lower_keep_urls(&v);
        assert!(t.elapsed().as_secs_f64() < 1.0, "{:?}", t.elapsed());
        assert!(out.starts_with("url(https://A.example/X.png) center url(https://A.example/X.png)"), "{}", &out[..80]);
        assert_eq!(out.len(), v.len());
        assert_eq!(lower_keep_urls("Bold URL( 'https://X/Ä.png' ) UrL(cid:A) NONE"), "bold url( 'https://X/Ä.png' ) url(cid:A) none");
        assert_eq!(lower_keep_urls("url(unclosed AND"), "url(unclosed AND");
    }
}
