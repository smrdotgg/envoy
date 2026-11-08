//! The real app, in a real window, against the synthetic mailbox: `--runs` cold starts of the
//! given binary with `ENVOY_BENCH` set (see crates/envoy/src/bench.rs for what the app does
//! then), memory sampled from outside every 100 ms.
//!
//! - Cold start: from just before `spawn` (wall clock) to the end of the first frame that
//!   shows the message list read from the cache. Includes exec and dynamic loading, settings
//!   and accounts, opening SQLite, the first list query, window and GPU setup, layout and the
//!   first draws. The OS file cache is warm (the mailbox was just written or read): a truly cold
//!   disk needs root to simulate and isn't measured.
//!
//!   It is gated above the OS's own launch floor (docs/NORTH_STAR.md): every OS charges any
//!   app a fixed price to start a process and put a window on screen (exec and dynamic loading,
//!   macOS's and Windows Defender's per-launch checks, window server round trips, the first
//!   present), which no app can avoid. `floor-window` (src/bin) is the lightest window we can
//!   show on each OS: winit + softbuffer drawing "Hello World" with CPU pixels, no GPU and no
//!   Rune. Each run of Envoy is paired with a run of it, launched the same way and timed to its
//!   first presented frame the same way; the gate is Envoy's median minus the floor's median.
//!   `floor-rune`, the smallest Rune app, splits what's left into Rune's own start (GPU, fonts,
//!   first layout) and Envoy's. The total is always printed beside it.
//! - Keypress → frame: `j` handed to the UI thread → the first frame showing it.
//! - Open, end to end: the same press → the first frame showing the opened conversation.
//! - Search, end to end: a search edit → the first frame showing its results.
//! - Idle memory: the largest sample in the idle stretches the app names (after startup, and
//!   after the presses and searches), over all runs.
//!
//! A run that crashes, exits badly, times out or reports the wrong number of steps or samples
//! makes the affected checks INVALID, which always fails (see report::Outcome::Invalid).

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

/// What the app's probe does per run (crates/envoy/src/bench.rs: `KEYS`, `SEARCHES`); a run
/// that reports anything else is an invalid measurement.
const KEYS: usize = 25;
const SEARCHES: usize = 6;
const IDLE_WINDOWS: &[&str] = &["startup", "after use"];
/// Memory samples (every 100 ms) each 2 s idle window must hold to count.
const MIN_IDLE_SAMPLES: usize = 5;

struct Run {
    spawn_ns: u64,
    report: Value,
    samples: Vec<(u64, u64)>,
}

/// The launch floors built beside this executable (src/bin).
const FLOORS: [&str; 2] = ["floor-window", "floor-rune"];
/// The gated cold-start check: Envoy's launch above the OS's own floor.
const COLD: &str = "cold start → list on screen, above OS floor";

fn floor_bin(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding envoy-bench's own path")?;
    let bin = exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(bin.exists(), "no {} beside envoy-bench (build it: cargo build -p envoy-bench builds every launch floor)", bin.display());
    Ok(bin)
}

/// `tag` names the run's files (`bench-run-{tag}{k}.*`): "" for Envoy, else the floor's name.
fn one_run(home: &Path, bin: &Path, k: usize, tag: &str) -> Result<Run> {
    let report_path = home.join(format!("bench-run-{tag}{k}.json"));
    let _ = std::fs::remove_file(&report_path);
    let out = std::fs::File::create(home.join(format!("bench-run-{tag}{k}.out")))?;
    let err_path = home.join(format!("bench-run-{tag}{k}.err"));
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
    let report: Value = serde_json::from_str(&text).context("the app's report isn't valid JSON")?;
    if let Some(e) = report.get("error").and_then(Value::as_str) {
        anyhow::bail!("{e} (window error: {})\nstderr:\n{tail}", report["window_error"]);
    }
    // A report is only trusted from a run that ended cleanly.
    anyhow::ensure!(status.success(), "the app wrote a report but exited badly ({status}): {}\nstderr:\n{tail}", reason(&stderr));
    for key in ["main_ns", "first_frame_ns", "first_list_ns"] {
        let t = report[key].as_u64().with_context(|| format!("the app's report has no {key}"))?;
        anyhow::ensure!(t >= spawn_ns, "the app's {key} is before it was started (clock skew?)");
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

/// `field` from every entry of each run's `list`, or why the sample isn't valid: a run with the
/// wrong number of entries, or entries without the value (a step that timed out).
fn sample(runs: &[Run], list: &str, field: &str, per_run: usize, what: &str) -> Result<Vec<f64>, String> {
    let mut v = Vec::new();
    for (k, r) in runs.iter().enumerate() {
        let entries = r.report[list].as_array().map_or(&[][..], |a| a.as_slice());
        if entries.len() != per_run {
            return Err(format!("run {} reported {} {what}, expected {per_run}", k + 1, entries.len()));
        }
        let missing = entries.iter().filter(|x| x[field].as_f64().is_none()).count();
        if missing > 0 {
            return Err(format!("run {}: {missing} of {per_run} {what} never reached the screen (timed out after 3 s)", k + 1));
        }
        v.extend(entries.iter().filter_map(|x| x[field].as_f64()));
    }
    Ok(v)
}

pub fn run(home: &Path, bin: &Path, runs: usize, wall_margin: f64, frame_margin: f64, checks: &mut Vec<Check>) -> Result<()> {
    anyhow::ensure!(home.join("mail.sqlite").exists(), "no mailbox at {} (run `envoy-bench gen` first)", home.display());
    let bin: PathBuf = bin.canonicalize().with_context(|| format!("no binary at {}", bin.display()))?;
    // A floor that's missing or fails makes cold start INVALID (it can't be taken off), and
    // nothing else.
    let mut floor_problem: Option<String> = None;
    let floor_bins: Vec<PathBuf> = match FLOORS.iter().map(|n| floor_bin(n)).collect::<Result<Vec<_>>>() {
        Ok(b) => b,
        Err(e) => {
            floor_problem = Some(format!("{e:#}"));
            Vec::new()
        }
    };
    let mut floors: [Vec<Run>; 2] = Default::default();
    let mut done = Vec::new();
    for k in 0..runs {
        // Each floor runs just before each run of Envoy, so a runner whose speed drifts over the
        // job moves both alike.
        if floor_problem.is_none() {
            for (i, fb) in floor_bins.iter().enumerate() {
                match one_run(home, fb, k, &format!("{}-", FLOORS[i])) {
                    Ok(r) => {
                        println!("{} run {}/{runs}: first frame {:.1} ms after spawn", FLOORS[i], k + 1, ms(r.spawn_ns, r.report["first_frame_ns"].as_u64().unwrap_or(0)));
                        floors[i].push(r);
                    }
                    Err(e) => {
                        let why = format!("{e:#}");
                        println!("{} run {}/{runs} failed: {why}", FLOORS[i], k + 1);
                        floor_problem = Some(format!("{} run {} of {runs} failed: {}", FLOORS[i], k + 1, why.lines().next().unwrap_or_default()));
                        break;
                    }
                }
            }
        }
        match one_run(home, &bin, k, "") {
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
                    (COLD, budget::COLD_START_MS, "ms"),
                    ("keypress → frame", budget::KEY_TO_FRAME_MS, "ms"),
                    ("open message, end to end", budget::OPEN_MS, "ms"),
                    ("search, end to end", budget::SEARCH_MS, "ms"),
                    ("idle memory", budget::IDLE_MEMORY_MB, "MB"),
                ] {
                    checks.push(Check::invalid("app", name, Some(b), unit, format!("run {} of {runs} failed: {short}", k + 1)));
                }
                return Ok(());
            }
        }
    }

    let margin_note = |m: f64| if m > 1.0 { format!("; gate ×{m:.1} on this runner") } else { String::new() };

    // Cold start (one_run made sure every run has its timestamps).
    let cold: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["first_list_ns"].as_u64().unwrap_or(0))).collect();
    let main: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["main_ns"].as_u64().unwrap_or(0))).collect();
    let first: Vec<f64> = done.iter().map(|r| ms(r.spawn_ns, r.report["first_frame_ns"].as_u64().unwrap_or(0))).collect();
    let rows = done[0].report["first_list_rows"].as_u64().unwrap_or(0);
    let (c50, cmax) = p50_max(&cold);
    let spread = |v: &[f64]| (v.iter().copied().fold(f64::INFINITY, f64::min), p50_max(v).1);
    let cmin = spread(&cold).0;
    match &floor_problem {
        Some(why) => checks.push(Check::invalid("app", COLD, Some(budget::COLD_START_MS), "ms", format!("can't take the OS launch floor off: {why}"))),
        None => {
            let launch = |runs: &[Run]| -> Vec<f64> { runs.iter().map(|r| ms(r.spawn_ns, r.report["first_frame_ns"].as_u64().unwrap_or(0))).collect() };
            let (window, rune) = (launch(&floors[0]), launch(&floors[1]));
            let (a50, b50) = (p50_max(&window).0, p50_max(&rune).0);
            let ((amin, amax), (bmin, bmax)) = (spread(&window), spread(&rune));
            checks.push(Check::gated(
                "app",
                COLD,
                (c50 - a50).max(0.0),
                "ms",
                budget::COLD_START_MS,
                wall_margin,
                format!("Envoy's share: spawn → list frame {c50:.1} ms − the OS launch floor {a50:.1} ms (medians of {} runs each){}", cold.len(), margin_note(wall_margin)),
            ));
            checks.push(Check::info("app", "  total: spawn → list on screen", c50, "ms", format!("median of {} runs (min {cmin:.1}, max {cmax:.1}); {rows} rows", cold.len())));
            checks.push(Check::info(
                "app",
                "  OS launch floor",
                a50,
                "ms",
                format!("median (min {amin:.1}, max {amax:.1}): floor-window, a bare window drawing Hello World (winit + softbuffer, no GPU), spawn → first frame"),
            ));
            checks.push(Check::info(
                "app",
                "  Rune's own start",
                b50 - a50,
                "ms",
                format!("floor-rune, the smallest Rune app, {b50:.1} ms (min {bmin:.1}, max {bmax:.1}), minus the floor: GPU device, pipeline, fonts, first layout"),
            ));
            checks.push(Check::info("app", "  Envoy's own start", c50 - b50, "ms", "the total minus floor-rune: settings, the cache and its first list, Envoy's views"));
        }
    }
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

    // Keypress → frame.
    match sample(&done, "keys", "frame_ms", KEYS, "key presses") {
        Err(why) => checks.push(Check::invalid("app", "keypress → frame", Some(budget::KEY_TO_FRAME_MS), "ms", why)),
        Ok(key) => {
            let (k50, kmax) = p50_max(&key);
            checks.push(Check::gated(
                "app",
                "keypress → frame",
                quantile(&key, 0.95),
                "ms",
                budget::KEY_TO_FRAME_MS,
                wall_margin,
                format!("p95 of {} presses of j (p50 {k50:.1}, max {kmax:.1}){}", key.len(), margin_note(wall_margin)),
            ));
        }
    }
    if let (Ok(build), Ok(encode)) = (sample(&done, "keys", "frame_build_ms", KEYS, "frame costs"), sample(&done, "keys", "frame_encode_ms", KEYS, "frame costs")) {
        checks.push(Check::info(
            "app",
            "  that frame's build (CPU)",
            quantile(&build, 0.95),
            "ms",
            format!("p95 (p50 {:.1}): events, view, layout, paint; then encode+submit p50 {:.1} ms; the rest is waiting on the GPU, present and the event loop", p50_max(&build).0, p50_max(&encode).0),
        ));
    }

    // Open end to end: the frame margin applies (it passes on real hardware).
    match sample(&done, "keys", "done_ms", KEYS, "opened messages") {
        Err(why) => checks.push(Check::invalid("app", "open message, end to end", Some(budget::OPEN_MS), "ms", why)),
        Ok(open) => {
            let (o50, omax) = p50_max(&open);
            checks.push(Check::gated(
                "app",
                "open message, end to end",
                quantile(&open, 0.95),
                "ms",
                budget::OPEN_MS,
                frame_margin,
                format!("p95 of {}: key → frame with the conversation (p50 {o50:.1}, max {omax:.1}){}", open.len(), margin_note(frame_margin)),
            ));
        }
    }
    // Search end to end: no margin; it misses on real hardware today.
    match sample(&done, "searches", "done_ms", SEARCHES, "searches") {
        Err(why) => checks.push(Check::invalid("app", "search, end to end", Some(budget::SEARCH_MS), "ms", why)),
        Ok(search) => {
            let (s50, smax) = p50_max(&search);
            checks.push(Check::gated(
                "app",
                "search, end to end",
                quantile(&search, 0.95),
                "ms",
                budget::SEARCH_MS,
                1.0,
                format!("p95 of {}: edit → frame with results (p50 {s50:.1}, max {smax:.1})", search.len()),
            ));
        }
    }

    // Idle memory: every run must have both idle windows, each with enough samples.
    let mut details = Vec::new();
    let mut worst: Option<(f64, &str)> = None;
    let mut problem = None;
    for name in IDLE_WINDOWS {
        let mut peak = 0u64;
        for (k, r) in done.iter().enumerate() {
            let windows: Vec<&Value> = r.report["idle"].as_array().into_iter().flatten().filter(|w| w["name"] == *name).collect();
            if windows.len() != 1 {
                problem.get_or_insert(format!("run {} reported {} \"{name}\" idle windows, expected 1", k + 1, windows.len()));
                continue;
            }
            let (a, b) = (windows[0]["start_ns"].as_u64().unwrap_or(0), windows[0]["end_ns"].as_u64().unwrap_or(0));
            let inside: Vec<u64> = r.samples.iter().filter(|(t, _)| *t >= a && *t <= b).map(|s| s.1).collect();
            if inside.len() < MIN_IDLE_SAMPLES {
                problem.get_or_insert(format!(
                    "run {}: {} {} samples in the \"{name}\" idle window, need {MIN_IDLE_SAMPLES} (couldn't read the process's memory?)",
                    k + 1,
                    inside.len(),
                    mem::METRIC
                ));
                continue;
            }
            peak = peak.max(inside.into_iter().max().unwrap_or(0));
        }
        let mb = peak as f64 / 1e6;
        details.push(format!("{name} {mb:.0} MB"));
        if worst.is_none_or(|w| mb > w.0) {
            worst = Some((mb, name));
        }
    }
    match (problem, worst) {
        (Some(why), _) => checks.push(Check::invalid("app", "idle memory", Some(budget::IDLE_MEMORY_MB), "MB", why)),
        (None, None) => checks.push(Check::invalid("app", "idle memory", Some(budget::IDLE_MEMORY_MB), "MB", "no idle windows")),
        (None, Some((mb, name))) => {
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
    }
    // What the floors hold at idle: the bare window's is the OS's per-window cost, the Rune
    // app's is the least any app on Rune holds (its GPU device, surface and atlases included).
    if floor_problem.is_none() {
        for (i, label) in [(0, "  floor-window idle memory"), (1, "  floor-rune idle memory")] {
            let peaks: Vec<f64> = floors[i]
                .iter()
                .filter_map(|r| {
                    let w = r.report["idle"].as_array()?.first()?;
                    let (a, b) = (w["start_ns"].as_u64()?, w["end_ns"].as_u64()?);
                    r.samples.iter().filter(|(t, _)| *t >= a && *t <= b).map(|s| s.1).max()
                })
                .map(|b| b as f64 / 1e6)
                .collect();
            if peaks.len() == floors[i].len() && !peaks.is_empty() {
                let worst = peaks.iter().copied().fold(0.0, f64::max);
                let what = if i == 0 { "the bare window" } else { "the smallest Rune app: Rune, the GPU driver and a surface" };
                checks.push(Check::info("app", label, worst, "MB", format!("{} at idle, worst of {} runs: {what}", mem::METRIC, peaks.len())));
            }
        }
    }
    Ok(())
}
