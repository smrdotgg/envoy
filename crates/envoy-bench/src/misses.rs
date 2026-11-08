//! `known-misses.toml`: the checks that miss their budget today, one entry per check per OS,
//! each with a ceiling. `check --known-misses FILE` rules on every measured check:
//!
//! - no entry: it must pass its gate (the budget, or budget × a documented frame margin);
//! - an entry, over its gate but under the ceiling: KNOWN MISS, doesn't fail the job;
//! - an entry, at or over the ceiling: FAIL (it got meaningfully worse);
//! - an entry, but it passed its gate: PASS with a "promote" warning annotation, so the entry
//!   gets removed. A warning, not a failure: several known misses sit close to their budget on
//!   noisy shared runners, so one passing run doesn't prove a fix, and failing on it would make
//!   CI flaky. Remove the entry once it passes reliably (and on the MacBook);
//! - INVALID measurements always fail; an entry never forgives a number that wasn't measured;
//! - an entry naming a check that wasn't measured (renamed, removed) is INVALID too, so the
//!   file can't rot.
//!
//! ```toml
//! [[miss]]
//! check = "search · archive: meeting"   # the check's id, exactly as the table prints it
//! os = "macos"                          # linux | macos | windows
//! ceiling = 680                         # same unit as the check; fails at or above
//! seen = "152–340 ms over 4 runs"       # what CI measured when the entry was written
//! ```
//!
//! `envoy-bench misses --from results.json ...` prints entries for every check over its gate in
//! any of the given runs, or for a store search/open timing at 80% of its budget or more (at the
//! budget on that runner, where run-to-run swings of ±20% would flake a hard gate), with
//! ceiling = worst seen × a factor: 1.05 for binary size (it only
//! moves with code), 1.25 for memory, 1.5 for timings. The worst seen already includes each
//! runner's slow days (the macOS runner's speed varied up to 3× between runs), so its ceilings
//! are wide; Linux and Windows give the tighter tripwires.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::report::{Check, Outcome};

pub const OSES: &[&str] = &["linux", "macos", "windows"];

/// `suggest` lists a store timing measured at or above this fraction of its budget: on the
/// Windows runner the same query's median spans about ±20% between runs (Inbox is:unread 33–46
/// ms), so a hard gate any closer than that flakes.
const NEAR: f64 = 0.8;

#[derive(Clone, Debug)]
pub struct Miss {
    pub check: String,
    pub os: String,
    pub ceiling: f64,
    pub seen: String,
}

pub fn load(path: &Path) -> Result<Vec<Miss>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<Miss>> {
    let doc: toml_edit::DocumentMut = text.parse()?;
    for (k, _) in doc.iter() {
        if k != "miss" {
            bail!("unknown top-level key {k:?} (only [[miss]] tables)");
        }
    }
    let Some(item) = doc.get("miss") else { return Ok(Vec::new()) };
    let tables = item.as_array_of_tables().context("`miss` must be [[miss]] tables")?;
    let mut out = Vec::new();
    let mut seen_keys = HashSet::new();
    for (i, t) in tables.iter().enumerate() {
        let n = i + 1;
        for (k, _) in t.iter() {
            if !matches!(k, "check" | "os" | "ceiling" | "seen") {
                bail!("entry {n}: unknown key {k:?}");
            }
        }
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_owned).with_context(|| format!("entry {n}: `{k}` must be a string"));
        let check = s("check")?;
        let os = s("os")?;
        let seen = s("seen")?;
        let ceiling = t
            .get("ceiling")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .with_context(|| format!("entry {n}: `ceiling` must be a number"))?;
        if !OSES.contains(&os.as_str()) {
            bail!("entry {n}: os {os:?} isn't one of {OSES:?}");
        }
        if !(ceiling.is_finite() && ceiling > 0.0) {
            bail!("entry {n}: ceiling must be a positive number");
        }
        if !seen_keys.insert((check.clone(), os.clone())) {
            bail!("entry {n}: {check:?} on {os} is listed twice");
        }
        out.push(Miss { check, os, ceiling, seen });
    }
    Ok(out)
}

/// Rules on `checks` (measured on `os`) with the entries for `os`. Entries for `os` whose check
/// matches one of `only` but wasn't measured become INVALID checks. Returns the promotions.
pub fn apply(checks: &mut Vec<Check>, misses: &[Miss], os: &str, only: &[String]) -> Vec<String> {
    let mut promote = Vec::new();
    // Only gated checks can be known misses; an entry for an info row is stale too.
    let ids: HashSet<String> = checks.iter().filter(|c| c.gate.is_some() || c.outcome == Outcome::Invalid).map(Check::id).collect();
    for c in checks.iter_mut() {
        let Some(m) = misses.iter().find(|m| m.os == os && m.check == c.id()) else { continue };
        let (Some(measured), Some(gate)) = (c.measured, c.gate) else { continue };
        if !matches!(c.outcome, Outcome::Pass | Outcome::Fail) {
            continue; // INVALID stays INVALID.
        }
        if m.ceiling <= gate {
            c.outcome = Outcome::Invalid;
            c.detail = format!("known-misses.toml ceiling {} is at or below the gate {gate}; fix the entry", m.ceiling);
            continue;
        }
        c.ceiling = Some(m.ceiling);
        if measured < gate {
            c.promote = true;
            c.detail = format!("{}; listed as a known miss on {os} but passed its gate: remove its known-misses.toml entry once it passes reliably", c.detail);
            promote.push(format!("{} passed its budget on {os} ({measured:.1} {} < {gate:.1}): remove its known-misses.toml entry once it passes reliably", c.id(), c.unit));
        } else if measured < m.ceiling {
            c.outcome = Outcome::KnownMiss;
            c.detail = format!("{}; known miss on {os} (CI saw {})", c.detail, m.seen);
        } else {
            c.outcome = Outcome::Fail;
            c.detail = format!("{}; OVER its known-miss ceiling on {os} (CI saw {}): it got worse", c.detail, m.seen);
        }
    }
    for m in misses.iter().filter(|m| m.os == os && only.iter().any(|p| m.check.contains(p.as_str()))) {
        if !ids.contains(&m.check) {
            let mut c = Check::invalid("known-misses.toml", m.check.clone(), None, "", format!("an entry for {os} names a check that wasn't measured (renamed or removed?)"));
            c.name = m.check.clone();
            checks.push(c);
        }
    }
    promote
}

fn factor(id: &str, unit: &str) -> f64 {
    if id.starts_with("binary") {
        1.05
    } else if unit == "MB" {
        1.25
    } else {
        1.5
    }
}

fn round_up(x: f64) -> f64 {
    let step = if x < 20.0 { 0.5 } else if x < 100.0 { 1.0 } else { 5.0 };
    (x / step).ceil() * step
}

/// known-misses.toml entries for every check over its gate in any of `runs`
/// (`(os, checks)` pairs, as measured).
pub fn suggest(runs: &[(String, Vec<Check>)]) -> String {
    let mut by: BTreeMap<(String, String), (Vec<f64>, usize, &'static str)> = BTreeMap::new();
    for (os, checks) in runs {
        for c in checks {
            let (Some(m), Some(_)) = (c.measured, c.gate) else { continue };
            let e = by.entry((os.clone(), c.id())).or_insert((Vec::new(), 0, c.unit));
            e.0.push(m);
            // Near the budget only counts for the store timings, gated at the budget: size and
            // memory don't swing run to run, and the app's wall-clock checks either miss or
            // already have a documented margin.
            let store_timing = matches!(c.group.as_str(), "search" | "open");
            if c.outcome == Outcome::Fail || (store_timing && c.gate.is_some_and(|g| m >= NEAR * g)) {
                e.1 += 1;
            }
        }
    }
    let mut s = String::new();
    for ((os, id), (v, over, unit)) in by {
        if over == 0 {
            continue;
        }
        let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = v.iter().copied().fold(0.0, f64::max);
        let ceiling = round_up(hi * factor(&id, unit));
        s.push_str(&format!(
            "[[miss]]\ncheck = {id:?}\nos = {os:?}\nceiling = {ceiling}\nseen = \"{lo:.1}–{hi:.1} {unit} over {} runs, over or within 20% of budget in {over}\"\n\n",
            v.len()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, measured: f64) -> Check {
        Check::gated("search", name, measured, "ms", 50.0, 1.0, "")
    }

    #[test]
    fn rulings() {
        let misses = parse(
            "[[miss]]\ncheck = \"search · slow\"\nos = \"linux\"\nceiling = 150\nseen = \"x\"\n\n\
             [[miss]]\ncheck = \"search · fixed\"\nos = \"linux\"\nceiling = 150\nseen = \"x\"\n\n\
             [[miss]]\ncheck = \"search · worse\"\nos = \"linux\"\nceiling = 150\nseen = \"x\"\n\n\
             [[miss]]\ncheck = \"search · gone\"\nos = \"linux\"\nceiling = 150\nseen = \"x\"\n\n\
             [[miss]]\ncheck = \"search · slow\"\nos = \"macos\"\nceiling = 90\nseen = \"x\"\n",
        )
        .unwrap();
        let mut checks = vec![c("slow", 100.0), c("fixed", 20.0), c("worse", 200.0), c("fine", 10.0), c("regressed", 60.0)];
        checks.push(Check::invalid("search", "timeout", Some(50.0), "ms", "timed out"));
        let promoted = apply(&mut checks, &misses, "linux", &["search · ".into()]);
        let outcome = |n: &str| checks.iter().find(|c| c.name == n || c.name.ends_with(n)).unwrap().outcome;
        assert_eq!(outcome("slow"), Outcome::KnownMiss);
        assert_eq!(outcome("fixed"), Outcome::Pass);
        assert_eq!(outcome("worse"), Outcome::Fail);
        assert_eq!(outcome("fine"), Outcome::Pass);
        assert_eq!(outcome("regressed"), Outcome::Fail, "a passing check without an entry gates hard");
        assert_eq!(outcome("timeout"), Outcome::Invalid);
        assert_eq!(outcome("search · gone"), Outcome::Invalid, "a stale entry is reported");
        assert_eq!(promoted.len(), 1);
        // The macOS entry doesn't apply on Linux, and a ceiling under the gate is refused.
        let mut mac = vec![c("slow", 100.0)];
        apply(&mut mac, &misses, "macos", &[]);
        assert_eq!(mac[0].outcome, Outcome::Fail);
        let low = parse("[[miss]]\ncheck = \"search · slow\"\nos = \"linux\"\nceiling = 40\nseen = \"x\"\n").unwrap();
        let mut l = vec![c("slow", 100.0)];
        apply(&mut l, &low, "linux", &[]);
        assert_eq!(l[0].outcome, Outcome::Invalid);
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(parse("[[miss]]\ncheck = \"a\"\nos = \"beos\"\nceiling = 1\nseen = \"x\"\n").is_err());
        assert!(parse("[[miss]]\ncheck = \"a\"\nos = \"linux\"\nceiling = 1\nseen = \"x\"\ntypo = 1\n").is_err());
        assert!(parse("[[miss]]\ncheck = \"a\"\nos = \"linux\"\nceiling = 1\n").is_err(), "seen is required");
        assert!(parse("[[miss]]\ncheck = \"a\"\nos = \"linux\"\nceiling = 1\nseen = \"x\"\n[[miss]]\ncheck = \"a\"\nos = \"linux\"\nceiling = 2\nseen = \"y\"\n").is_err());
        assert!(parse("other = 1\n").is_err());
    }

    #[test]
    fn the_checked_in_file_parses() {
        parse(include_str!("../known-misses.toml")).unwrap();
    }
}
