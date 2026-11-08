//! Checks against the budgets, printed as a table and optionally as Markdown and JSON.

use std::fmt::Write as _;

/// The budgets from docs/NORTH_STAR.md, in one place.
/// Sizes are decimal (1 MB = 10^6 bytes), the stricter reading.
pub mod budget {
    pub const COLD_START_MS: f64 = 100.0;
    pub const KEY_TO_FRAME_MS: f64 = 16.0;
    pub const OPEN_MS: f64 = 50.0;
    pub const SEARCH_MS: f64 = 50.0;
    pub const IDLE_MEMORY_MB: f64 = 150.0;
    pub const BINARY_MB: f64 = 30.0;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Pass,
    Fail,
    /// Measured and shown, not gated (a breakdown, or a number the method can't stand behind).
    Info,
    /// Couldn't be measured here (no window, unsupported platform). Fails the run unless
    /// `allow_skip`.
    Skipped,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub group: String,
    pub name: String,
    pub measured: Option<f64>,
    pub unit: &'static str,
    pub budget: Option<f64>,
    /// The threshold actually enforced: the budget, or budget × a documented margin.
    pub gate: Option<f64>,
    pub detail: String,
    pub outcome: Outcome,
}

impl Check {
    /// A gated check: passes when `measured < gate`.
    pub fn gated(group: impl Into<String>, name: impl Into<String>, measured: f64, unit: &'static str, budget: f64, margin: f64, detail: impl Into<String>) -> Check {
        let gate = budget * margin;
        Check {
            group: group.into(),
            name: name.into(),
            measured: Some(measured),
            unit,
            budget: Some(budget),
            gate: Some(gate),
            detail: detail.into(),
            outcome: if measured < gate { Outcome::Pass } else { Outcome::Fail },
        }
    }

    pub fn info(group: impl Into<String>, name: impl Into<String>, measured: f64, unit: &'static str, detail: impl Into<String>) -> Check {
        Check { group: group.into(), name: name.into(), measured: Some(measured), unit, budget: None, gate: None, detail: detail.into(), outcome: Outcome::Info }
    }

    /// "group · name", what `check --only` matches against.
    pub fn id(&self) -> String {
        format!("{} · {}", self.group, self.name.trim())
    }

    pub fn skipped(group: impl Into<String>, name: impl Into<String>, budget: Option<f64>, unit: &'static str, why: impl Into<String>) -> Check {
        Check { group: group.into(), name: name.into(), measured: None, unit, budget, gate: None, detail: why.into(), outcome: Outcome::Skipped }
    }
}

fn num(v: Option<f64>, unit: &str) -> String {
    match v {
        None => "—".into(),
        Some(v) if v >= 100.0 => format!("{v:.0} {unit}"),
        Some(v) if v >= 10.0 => format!("{v:.1} {unit}"),
        Some(v) => format!("{v:.2} {unit}"),
    }
}

fn result(c: &Check) -> &'static str {
    match c.outcome {
        Outcome::Pass => "PASS",
        Outcome::Fail => "FAIL",
        Outcome::Info => "info",
        Outcome::Skipped => "SKIP",
    }
}

fn gate_text(c: &Check) -> String {
    match (c.budget, c.gate) {
        (Some(b), Some(g)) if (g - b).abs() > 1e-9 => format!("{} (×{:.1})", num(Some(g), c.unit), g / b),
        (_, Some(g)) => num(Some(g), c.unit),
        _ => "—".into(),
    }
}

pub fn print_table(title: &str, checks: &[Check]) {
    let rows: Vec<[String; 6]> = checks
        .iter()
        .map(|c| [format!("{} · {}", c.group, c.name), num(c.measured, c.unit), num(c.budget, c.unit), gate_text(c), result(c).into(), c.detail.clone()])
        .collect();
    let head = ["check", "measured", "budget", "gate", "result", "detail"];
    let mut w = head.map(|h| h.chars().count());
    for r in &rows {
        for (i, cell) in r.iter().enumerate().take(5) {
            w[i] = w[i].max(cell.chars().count());
        }
    }
    let line = |cells: &[String; 6]| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i < 5 {
                let pad = w[i] - cell.chars().count();
                if i == 0 { s.push_str(cell); s.push_str(&" ".repeat(pad)); } else { s.push_str(&" ".repeat(pad)); s.push_str(cell); }
                s.push_str("  ");
            } else {
                s.push_str(cell);
            }
        }
        s
    };
    println!("\n== {title} ==");
    println!("{}", line(&head.map(String::from)));
    println!("{}", "-".repeat(w.iter().sum::<usize>() + 2 * 5 + 6));
    for r in &rows {
        println!("{}", line(r));
    }
}

pub fn markdown(title: &str, checks: &[Check]) -> String {
    let mut s = format!("### {title}\n\n| check | measured | budget | gate | result | detail |\n|---|--:|--:|--:|:-:|---|\n");
    for c in checks {
        let r = match c.outcome {
            Outcome::Pass => "✅ pass",
            Outcome::Fail => "❌ FAIL",
            Outcome::Info => "info",
            Outcome::Skipped => "⚠️ skip",
        };
        let _ = writeln!(s, "| {} · {} | {} | {} | {} | {} | {} |", c.group, c.name, num(c.measured, c.unit), num(c.budget, c.unit), gate_text(c), r, c.detail.replace('|', "\\|"));
    }
    s.push('\n');
    s
}

pub fn json(checks: &[Check]) -> serde_json::Value {
    serde_json::Value::Array(
        checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "group": c.group, "name": c.name, "measured": c.measured, "unit": c.unit,
                    "budget": c.budget, "gate": c.gate, "result": result(c), "detail": c.detail,
                })
            })
            .collect(),
    )
}

fn unit_of(s: &str) -> &'static str {
    match s {
        "ms" => "ms",
        "MB" => "MB",
        _ => "",
    }
}

/// Checks back from `json`'s output.
pub fn from_json(v: &serde_json::Value) -> Vec<Check> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|c| Check {
            group: c["group"].as_str().unwrap_or_default().to_string(),
            name: c["name"].as_str().unwrap_or_default().to_string(),
            measured: c["measured"].as_f64(),
            unit: unit_of(c["unit"].as_str().unwrap_or_default()),
            budget: c["budget"].as_f64(),
            gate: c["gate"].as_f64(),
            detail: c["detail"].as_str().unwrap_or_default().to_string(),
            outcome: match c["result"].as_str() {
                Some("PASS") => Outcome::Pass,
                Some("FAIL") => Outcome::Fail,
                Some("SKIP") => Outcome::Skipped,
                _ => Outcome::Info,
            },
        })
        .collect()
}

/// Median and maximum of a sample.
pub fn p50_max(v: &[f64]) -> (f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    if s.is_empty() {
        return (0.0, 0.0);
    }
    (s[(s.len() - 1) / 2], s[s.len() - 1])
}

/// The value at quantile `q` (nearest rank).
pub fn quantile(v: &[f64], q: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    if s.is_empty() {
        return 0.0;
    }
    s[((s.len() - 1) as f64 * q).round() as usize]
}
