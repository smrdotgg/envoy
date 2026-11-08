//! `envoy-bench`: Envoy's latency budgets (docs/NORTH_STAR.md), measured against a large
//! synthetic mailbox, as a table of measured vs budget. Exits non-zero when a gated check is
//! over budget, so CI fails on a regression.
//!
//! ```text
//! envoy-bench gen   --home DIR [--messages 100000] [--seed 1]
//! envoy-bench store --home DIR                              search + open on the engine's code path
//! envoy-bench search|open --home DIR                        just one of them
//! envoy-bench app   --home DIR --bin target/dist/envoy      cold start, keypress→frame, open/search
//!                   [--runs 5] [--wall-margin 1]            end to end, idle memory (real window)
//!                   [--frame-margin 1]
//! envoy-bench size  --bin target/dist/envoy                 shipped binary size
//! envoy-bench all   --home DIR --bin target/dist/envoy      gen (if needed) + store + app + size
//! envoy-bench check --from OUT.json [--from ..] --only TEXT [--only ..]   gate earlier results
//!                   [--known-misses crates/envoy-bench/known-misses.toml]
//! envoy-bench misses --from OUT.json [--from ..]           known-misses.toml entries for every
//!                                                           check over budget in those runs
//!
//! common: --json OUT.json   --markdown OUT.md (appends; e.g. $GITHUB_STEP_SUMMARY)
//!         --measure-only    print and save; exit 0 unless a measurement is INVALID (gate later
//!                           with check)
//! ```
//!
//! `check` lets CI measure once and gate each budget in its own step. Every check passes its
//! gate unless known-misses.toml lists it for this OS, with a ceiling it mustn't reach (see
//! misses.rs). INVALID measurements (the app couldn't run or exited badly, a step timed out,
//! missing or miscounted samples) always fail.
//!
//! Margins multiply a gate (never the budget) for wall-clock checks that shared CI runners and
//! their software GPUs distort: `--frame-margin` for open end to end (whose store-side work is
//! gated hard by `store`), `--wall-margin` for cold start and keypress→frame (1 today: they miss
//! on real hardware). Search, open (store), memory and size are always gated at the budget. The
//! table always shows the true measurement, the budget and the gate.

mod app_bench;
mod mailbox;
mod mem;
mod misses;
mod report;
mod store_bench;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use report::{Check, Outcome};

struct Opts {
    cmd: String,
    home: Option<PathBuf>,
    bin: Option<PathBuf>,
    messages: usize,
    seed: u64,
    runs: usize,
    wall_margin: f64,
    frame_margin: f64,
    json: Option<PathBuf>,
    markdown: Option<PathBuf>,
    known_misses: Option<PathBuf>,
    measure_only: bool,
    from: Vec<PathBuf>,
    only: Vec<String>,
    label: String,
}

fn parse() -> Result<Opts> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let mut o = Opts {
        cmd,
        home: None,
        bin: None,
        messages: 100_000,
        seed: 1,
        runs: 5,
        wall_margin: 1.0,
        frame_margin: 1.0,
        json: None,
        markdown: None,
        known_misses: None,
        measure_only: false,
        from: Vec::new(),
        only: Vec::new(),
        label: std::env::consts::OS.to_string(),
    };
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--home" => o.home = Some(val()?.into()),
            "--bin" => o.bin = Some(val()?.into()),
            "--messages" => o.messages = val()?.parse()?,
            "--seed" => o.seed = val()?.parse()?,
            "--runs" => o.runs = val()?.parse::<usize>()?.max(1),
            "--wall-margin" => o.wall_margin = val()?.parse::<f64>()?.max(1.0),
            "--frame-margin" => o.frame_margin = val()?.parse::<f64>()?.max(1.0),
            "--json" => o.json = Some(val()?.into()),
            "--markdown" => o.markdown = Some(val()?.into()),
            "--known-misses" => o.known_misses = Some(val()?.into()),
            "--measure-only" => o.measure_only = true,
            "--from" => o.from.push(val()?.into()),
            "--only" => o.only.push(val()?),
            "--label" => o.label = val()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(o)
}

fn need<'a>(v: &'a Option<PathBuf>, flag: &str) -> Result<&'a PathBuf> {
    v.as_ref().with_context(|| format!("{flag} is required"))
}

fn gen_if_needed(o: &Opts, force: bool) -> Result<()> {
    let home = need(&o.home, "--home")?;
    if !force && home.join("mail.sqlite").exists() {
        println!("using the mailbox already in {}", home.display());
        return Ok(());
    }
    let t = Instant::now();
    let s = mailbox::generate(home, o.messages, o.seed)?;
    let size = std::fs::metadata(home.join("mail.sqlite")).map(|m| m.len()).unwrap_or(0);
    println!(
        "generated {} messages in {:.1} s: {} HTML, {} with attachments, {} replies, {} unread; {} raw, cache {}",
        s.messages,
        t.elapsed().as_secs_f64(),
        s.html,
        s.with_attachments,
        s.replies,
        s.unread,
        store_bench::human_size(s.bytes_raw as u64),
        store_bench::human_size(size)
    );
    Ok(())
}

fn size_check(o: &Opts, checks: &mut Vec<Check>) -> Result<()> {
    let bin = need(&o.bin, "--bin")?;
    let bytes = std::fs::metadata(bin).with_context(|| format!("no binary at {}", bin.display()))?.len();
    let mb = bytes as f64 / 1e6; // decimal MB: the stricter reading of the budget
    checks.push(Check::gated("binary", "size", mb, "MB", report::budget::BINARY_MB, 1.0, format!("{} ({bytes} bytes)", bin.display())));
    Ok(())
}

fn main() {
    let o = match parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("envoy-bench: {e:#}\n\nusage: envoy-bench gen|store|search|open|app|size|all|check|misses --home DIR --bin PATH (see the source header)");
            std::process::exit(2);
        }
    };
    let mut checks = Vec::new();
    let r: Result<()> = (|| {
        match o.cmd.as_str() {
            "gen" => return gen_if_needed(&o, true),
            "store" => store_bench::run(need(&o.home, "--home")?, &mut checks, true, true)?,
            "search" => store_bench::run(need(&o.home, "--home")?, &mut checks, true, false)?,
            "open" => store_bench::run(need(&o.home, "--home")?, &mut checks, false, true)?,
            "app" => app_bench::run(need(&o.home, "--home")?, need(&o.bin, "--bin")?, o.runs, o.wall_margin, o.frame_margin, &mut checks)?,
            "size" => size_check(&o, &mut checks)?,
            "check" => {
                anyhow::ensure!(!o.from.is_empty() && !o.only.is_empty(), "--from and --only are required");
                for from in &o.from {
                    let text = std::fs::read_to_string(from).with_context(|| format!("reading {}", from.display()))?;
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    checks.extend(report::from_json(&v["checks"]).into_iter().filter(|c| o.only.iter().any(|p| c.id().contains(p.as_str()))));
                }
                anyhow::ensure!(!checks.is_empty(), "no check in {:?} matches {:?}", o.from, o.only);
                if let Some(path) = &o.known_misses {
                    let list = misses::load(path)?;
                    for p in misses::apply(&mut checks, &list, std::env::consts::OS, &o.only) {
                        println!("PROMOTE: {p}");
                        if std::env::var_os("GITHUB_ACTIONS").is_some() {
                            println!("::warning title=Known budget miss now passes::{p}");
                        }
                    }
                }
            }
            "misses" => {
                anyhow::ensure!(!o.from.is_empty(), "--from is required");
                let mut runs = Vec::new();
                for from in &o.from {
                    let text = std::fs::read_to_string(from).with_context(|| format!("reading {}", from.display()))?;
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    let os = v["os"].as_str().with_context(|| format!("{} has no \"os\" (made by an older envoy-bench?)", from.display()))?;
                    runs.push((os.to_string(), report::from_json(&v["checks"])));
                }
                print!("{}", misses::suggest(&runs));
                return Ok(());
            }
            "all" => {
                gen_if_needed(&o, false)?;
                store_bench::run(need(&o.home, "--home")?, &mut checks, true, true)?;
                app_bench::run(need(&o.home, "--home")?, need(&o.bin, "--bin")?, o.runs, o.wall_margin, o.frame_margin, &mut checks)?;
                size_check(&o, &mut checks)?;
            }
            other => bail!("unknown command {other:?}"),
        }
        Ok(())
    })();
    if let Err(e) = &r {
        eprintln!("envoy-bench: {e:#}");
    }
    if checks.is_empty() {
        std::process::exit(if r.is_ok() { 0 } else { 2 });
    }
    let title = if o.cmd == "check" { format!("Envoy budgets: {} ({})", o.only.join(", "), o.label) } else { format!("Envoy budgets: {} ({})", o.cmd, o.label) };
    report::print_table(&title, &checks);
    if let Some(p) = &o.markdown {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(report::markdown(&title, &checks).as_bytes());
        }
    }
    if let Some(p) = &o.json {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&serde_json::json!({ "label": o.label, "os": std::env::consts::OS, "cmd": o.cmd, "checks": report::json(&checks) })).unwrap_or_default());
    }
    let of = |o: Outcome| checks.iter().filter(move |c| c.outcome == o);
    for c in of(Outcome::KnownMiss) {
        println!("KNOWN MISS: {} (under its ceiling)", c.id());
    }
    for c in of(Outcome::Fail) {
        println!("OVER BUDGET: {}", c.id());
    }
    for c in of(Outcome::Invalid) {
        println!("INVALID MEASUREMENT: {}: {}", c.id(), c.detail);
    }
    let invalid = of(Outcome::Invalid).next().is_some();
    let bad = r.is_err() || invalid || of(Outcome::Fail).next().is_some();
    if o.measure_only && r.is_ok() && !invalid {
        println!("\nmeasured (--measure-only: gated in separate steps)");
        std::process::exit(0);
    }
    println!("{}", if bad { "\nbudgets: FAILED" } else { "\nbudgets: ok" });
    std::process::exit(if bad { 1 } else { 0 });
}
