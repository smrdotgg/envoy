//! The real app, in a real window, against the synthetic mailbox: `--runs` cold starts of the
//! given binary with `ENVOY_BENCH` set (see crates/envoy/src/bench.rs for what the app does
//! then), memory sampled from outside every 100 ms.
//!
//! - Cold start: from just before `spawn` (wall clock) to the end of the first frame that
//!   shows the message list read from the cache. Includes exec and dynamic loading, settings
//!   and accounts, opening SQLite, the first list query, window and GPU setup, layout and the
//!   first draws. The OS file cache is warm (the mailbox was just written or read): a truly cold
//!   disk needs root to simulate and isn't measured.
//! - Keypress → frame: `j` handed to the UI thread → the first frame showing it.
//! - Open, end to end: the same press → the first frame showing the opened conversation.
//! - Search, end to end: a search edit → the first frame showing its results.
//! - Idle memory: the largest sample in the idle stretches the app names (after startup, and
//!   after the presses and searches), over all runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mem;
use crate::report::{Check, budget, p50_max, quantile};

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

struct Run {
    spawn_ns: u64,
    report: Value,
    samples: Vec<(u64, u64)>,
}

fn one_run(home: &Path, bin: &Path, k: usize) -> Result<Run> {
    let report_path = home.join(format!("bench-run-{k}.json"));
    let _ = std::fs::remove_file(&report_path);
    let out = std::fs::File::create(home.join(format!("bench-run-{k}.out")))?;
    let err_path = home.join(format!("bench-run-{k}.err"));
    let err = std::fs::File::create(&err_path)?;
    let mut cmd = Command::new(bin);
    cmd.env("ENVOY_HOME", home)
        .env("ENVOY_SECRETS", "file")
        .env("ENVOY_BENCH", &report_path)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let spawn_ns = now_ns();
    let mut child = cmd.spawn().with_context(|| format!("starting {}", bin.display()))?;
    let pid = child.id();
    let started = Instant::now();
    let mut samples = Vec::new();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if let Some(b) = mem::of(pid) {
            samples.push((now_ns(), b));
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            anyhow::bail!("{} didn't finish within 120 s (killed)", bin.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let tail: String = stderr.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let text = std::fs::read_to_string(&report_path).with_context(|| format!("the app wrote no report ({status}): {}\nstderr:\n{tail}", reason(&stderr)))?;
    let report: Value = serde_json::from_str(&text)?;
    if let Some(e) = report.get("error").and_then(Value::as_str) {
        anyhow::bail!("{e} (window error: {})\nstderr:\n{tail}", report["window_error"]);
    }
    Ok(Run { spawn_ns, report, samples })
}

/// The line that says why a crashed run crashed: a panic's message, else the last line.
fn reason(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    if let Some(i) = lines.iter().rposition(|l| l.contains("panicked at")) {
        return lines.get(i + 1).unwrap_or(&lines[i]).trim().to_string();
    }
    lines.last().map_or_else(|| "no output".into(), |l| l.trim().to_string())
}

fn ms(a: u64, b: u64) -> f64 {
    (b.saturating_sub(a)) as f64 / 1e6
}

fn floats(runs: &[Run], list: &str, field: &str) -> (Vec<f64>, usize) {
    let mut v = Vec::new();
    let mut missing = 0;
    for r in runs {
        for x in r.report[list].as_array().into_iter().flatten() {
            match x[field].as_f64() {
                Some(f) => v.push(f),
                None => missing += 1,
            }
        }
    }
    (v, missing)
}

pub fn run(home: &Path, bin: &Path, runs: usize, wall_margin: f64, frame_margin: f64, checks: &mut Vec<Check>) -> Result<()> {
    anyhow::ensure!(home.join("mail.sqlite").exists(), "no mailbox at {} (run `envoy-bench gen` first)", home.display());
    let bin: PathBuf = bin.canonicalize().with_context(|| format!("no binary at {}", bin.display()))?;
    let mut done = Vec::new();
    for k in 0..runs {
        match one_run(home, &bin, k) {
            Ok(r) => {
                let cold = ms(r.spawn_ns, r.report["first_list_ns"].as_u64().unwrap_or(0));
                println!("app run {}/{runs}: first list frame {cold:.1} ms after spawn", k + 1);
                done.push(r);
            }
            Err(e) => {
                let why = format!("{e:#}");
                println!("app run {}/{runs} failed: {why}", k + 1);
                let short = why.lines().next().unwrap_or_default().to_string();
                for (name, b, unit) in [
                    ("cold start → list on screen", budget::COLD_START_MS, "ms"),
                    ("keypress → frame", budget::KEY_TO_FRAME_MS, "ms"),
                    ("open message, end to end", budget::OPEN_MS, "ms"),
                    ("search, end to end", budget::SEARCH_MS, "ms"),
                    ("idle memory", budget::IDLE_MEMORY_MB, "MB"),
                ] {
                    checks.push(Check::skipped("app", name, Some(b), unit, format!("couldn't run the app: {short}")));
                }
                return Ok(());
            }
        }
    }

    let margin_note = |m: f64| if m > 1.0 { format!("; gate ×{m:.1} on this runner") } else { String::new() };

    // Cold start.
    let cold: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["first_list_ns"].as_u64().unwrap_or(0))).collect();
    let main: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["main_ns"].as_u64().unwrap_or(0))).collect();
    let first: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["first_frame_ns"].as_u64().unwrap_or(0))).collect();
    let rows = done[0].report["first_list_rows"].as_u64().unwrap_or(0);
    let (c50, cmax) = p50_max(&cold);
    let cmin = cold.iter().copied().fold(f64::INFINITY, f64::min);
    checks.push(Check::gated(
        "app",
        "cold start → list on screen",
        c50,
        "ms",
        budget::COLD_START_MS,
        wall_margin,
        format!("median of {} runs (min {cmin:.1}, max {cmax:.1}); {rows} rows{}", cold.len(), margin_note(wall_margin)),
    ));
    checks.push(Check::info("app", "  spawn → main()", p50_max(&main).0, "ms", "median: exec, loader, Rust runtime"));
    checks.push(Check::info("app", "  spawn → first frame", p50_max(&first).0, "ms", "median: window, GPU, first layout (list maybe still loading)"));
    let list_wait: Vec<f64> = done.iter().map(|r| ms(r.report["first_frame_ns"].as_u64().unwrap_or(0), r.report["first_list_ns"].as_u64().unwrap_or(0))).collect();
    let list_build: Vec<f64> = done.iter().filter_map(|r| r.report["first_list_build_ms"].as_f64()).collect();
    let list_encode: Vec<f64> = done.iter().filter_map(|r| r.report["first_list_encode_ms"].as_f64()).collect();
    checks.push(Check::info(
        "app",
        "  first frame → list frame",
        p50_max(&list_wait).0,
        "ms",
        format!("median: the cache's list arriving and drawn; that frame built in {:.1} ms, encoded in {:.1} ms", p50_max(&list_build).0, p50_max(&list_encode).0),
    ));

    // Keypress → frame, and open end to end.
    let (key, key_missing) = floats(&done, "keys", "frame_ms");
    if key.is_empty() {
        checks.push(Check::skipped("app", "keypress → frame", Some(budget::KEY_TO_FRAME_MS), "ms", "no key press produced a frame"));
    } else {
        let (k50, kmax) = p50_max(&key);
        let k95 = quantile(&key, 0.95);
        checks.push(Check::gated(
            "app",
            "keypress → frame",
            k95,
            "ms",
            budget::KEY_TO_FRAME_MS,
            wall_margin,
            format!("p95 of {} presses of j (p50 {k50:.1}, max {kmax:.1}){}{}", key.len(), if key_missing > 0 { format!(", {key_missing} without a frame") } else { String::new() }, margin_note(wall_margin)),
        ));
    }
    let (build, _) = floats(&done, "keys", "frame_build_ms");
    let (encode, _) = floats(&done, "keys", "frame_encode_ms");
    if !build.is_empty() {
        checks.push(Check::info(
            "app",
            "  that frame's build (CPU)",
            quantile(&build, 0.95),
            "ms",
            format!("p95 (p50 {:.1}): events, view, layout, paint; then encode+submit p50 {:.1} ms; the rest is waiting on the GPU, present and the event loop", p50_max(&build).0, p50_max(&encode).0),
        ));
    }
    let (open, open_missing) = floats(&done, "keys", "done_ms");
    if open.is_empty() {
        checks.push(Check::skipped("app", "open message, end to end", Some(budget::OPEN_MS), "ms", "no opened message reached the screen"));
    } else {
        let (o50, omax) = p50_max(&open);
        let o95 = quantile(&open, 0.95);
        let mut c = Check::gated(
            "app",
            "open message, end to end",
            o95,
            "ms",
            budget::OPEN_MS,
            frame_margin,
            format!("p95 of {}: key → frame with the conversation (p50 {o50:.1}, max {omax:.1}){}{}", open.len(), if open_missing > 0 { format!(", {open_missing} timed out") } else { String::new() }, margin_note(frame_margin)),
        );
        if open_missing > 0 {
            c.outcome = crate::report::Outcome::Fail;
        }
        checks.push(c);
    }
    let (search, search_missing) = floats(&done, "searches", "done_ms");
    if search.is_empty() {
        checks.push(Check::skipped("app", "search, end to end", Some(budget::SEARCH_MS), "ms", "no search result reached the screen"));
    } else {
        let (s50, smax) = p50_max(&search);
        let s95 = quantile(&search, 0.95);
        let mut c = Check::gated(
            "app",
            "search, end to end",
            s95,
            "ms",
            budget::SEARCH_MS,
            frame_margin,
            format!("p95 of {}: edit → frame with results (p50 {s50:.1}, max {smax:.1}){}{}", search.len(), if search_missing > 0 { format!(", {search_missing} timed out") } else { String::new() }, margin_note(frame_margin)),
        );
        if search_missing > 0 {
            c.outcome = crate::report::Outcome::Fail;
        }
        checks.push(c);
    }

    // Idle memory.
    let mut worst: Option<(f64, String)> = None;
    let mut details = Vec::new();
    for name in ["startup", "after use"] {
        let mut peak = 0u64;
        for r in &done {
            for w in r.report["idle"].as_array().into_iter().flatten().filter(|w| w["name"] == name) {
                let (a, b) = (w["start_ns"].as_u64().unwrap_or(0), w["end_ns"].as_u64().unwrap_or(0));
                for &(t, bytes) in &r.samples {
                    if t >= a && t <= b {
                        peak = peak.max(bytes);
                    }
                }
            }
        }
        if peak > 0 {
            let mb = peak as f64 / 1e6;
            details.push(format!("{name} {mb:.0} MB"));
            if worst.as_ref().is_none_or(|w| mb > w.0) {
                worst = Some((mb, name.to_string()));
            }
        }
    }
    match worst {
        Some((mb, name)) => {
            let peak_all = done.iter().flat_map(|r| r.samples.iter().map(|s| s.1)).max().unwrap_or(0) as f64 / 1e6;
            checks.push(Check::gated(
                "app",
                "idle memory",
                mb,
                "MB",
                budget::IDLE_MEMORY_MB,
                1.0,
                format!("{} at idle, worst ({name}) over {} runs: {}; peak while busy {peak_all:.0} MB", mem::METRIC, done.len(), details.join(", ")),
            ));
        }
        None => checks.push(Check::skipped("app", "idle memory", Some(budget::IDLE_MEMORY_MB), "MB", format!("couldn't read the process's {} here", mem::METRIC))),
    }
    Ok(())
}
