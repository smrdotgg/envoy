//! The latency-budget probe that `envoy-bench app` drives (crates/envoy-bench). Inert unless
//! `ENVOY_BENCH` names a report file; then, in the real window, it:
//!
//! 1. notes when the first frame showing the local message list was drawn (cold start);
//! 2. presses `j` through the app's own key handler and times each press to the first frame
//!    that shows it (keypress → frame) and to the first frame showing the opened message's
//!    content (open, end to end);
//! 3. types searches into the search state as the search field does and times them to the
//!    frame showing the results;
//! 4. leaves idle stretches, named in the report, for the harness to sample memory in;
//!
//! then writes the report and quits. Times are taken on a driver thread just before the input
//! is handed to the UI thread, and in `on_frame_drawn`, after the frame was submitted and
//! presented, so they include the event-loop wake, the update, view, layout, paint and GPU
//! submission. They don't include the OS delivering a real key event, or scan-out.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use envoy_core::model::MessageId;
use rune::{Key, KeyEvent, Mods, TextEdit, Tx};
use serde_json::{Value, json};

use crate::app::{App, Screen};

static MAIN_NS: OnceLock<u64> = OnceLock::new();

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Call first thing in `main`: when the process got to Envoy's code.
pub fn started() {
    if std::env::var_os("ENVOY_BENCH").is_some() {
        MAIN_NS.get_or_init(now_ns);
    }
}

const KEYS: usize = 25;
const SEARCHES: &[&str] = &["invoice", "from:alice", "is:unread", "budget review", "has:attachment", ""];
const STEP_TIMEOUT: Duration = Duration::from_secs(3);

enum Want {
    Open(MessageId),
    List(u64),
}

struct Wait {
    t0: Instant,
    applied: bool,
    want: Option<Want>,
    frame_ms: Option<f64>,
    done_ms: Option<f64>,
    /// The first frame's own cost: building it (events, poll, view, layout, paint) and
    /// encoding and submitting it to the GPU, from `on_frame_encoded`.
    frame_cost: Option<(f64, f64)>,
    done_cost: Option<(f64, f64)>,
    rows: usize,
}

#[derive(Default)]
struct Probe {
    first_frame_ns: Option<u64>,
    frames_before_list: u32,
    first_list_ns: Option<u64>,
    first_list_rows: usize,
    first_list_cost: Option<(f64, f64)>,
    /// The latest drawn frame's (update_ms, encode_ms); `on_frame_encoded` runs just before
    /// `on_frame_drawn` for the same frame.
    last_cost: Option<(f64, f64)>,
    wait: Option<Wait>,
}

type Shared = Arc<(Mutex<Probe>, Condvar)>;

/// Hooks the probe into the app when `ENVOY_BENCH` is set; otherwise returns it untouched.
pub fn attach(app: rune::App<App>) -> rune::App<App> {
    let Some(path) = std::env::var_os("ENVOY_BENCH").map(PathBuf::from) else { return app };
    let shared: Shared = Arc::new((Mutex::new(Probe::default()), Condvar::new()));
    let costs = shared.clone();
    let app = app.on_frame_encoded(move |f: &rune::FrameEncoded| {
        if f.outcome() == rune::FrameOutcome::Drawn
            && let Ok(mut p) = costs.0.lock()
        {
            p.last_cost = f.update_ms().zip(f.encode_ms());
        }
    });
    let hook = shared.clone();
    let app = app.on_frame_drawn(move |rt: &rune::Runtime<App>| {
        let s = &rt.state;
        let now = Instant::now();
        let (lock, cv) = &*hook;
        let Ok(mut p) = lock.lock() else { return };
        let cost = p.last_cost.take();
        if p.first_frame_ns.is_none() {
            p.first_frame_ns = Some(now_ns());
        }
        if p.first_list_ns.is_none() {
            if s.screen == Screen::Mail && s.list_loaded && !s.list.is_empty() {
                p.first_list_ns = Some(now_ns());
                p.first_list_rows = s.list.len();
                p.first_list_cost = cost;
                cv.notify_all();
            } else {
                p.frames_before_list += 1;
            }
        }
        if let Some(w) = p.wait.as_mut()
            && w.applied
        {
            let ms = now.duration_since(w.t0).as_secs_f64() * 1000.0;
            if w.frame_ms.is_none() {
                w.frame_ms = Some(ms);
                w.frame_cost = cost;
            }
            let done = match w.want {
                Some(Want::Open(id)) => s.thread_for == Some(id) && !s.thread.is_empty() && s.thread.iter().all(|m| m.body.is_some()),
                Some(Want::List(req)) => s.list_req == req && s.list_loaded,
                None => false,
            };
            if done && w.done_ms.is_none() {
                w.done_ms = Some(ms);
                w.done_cost = cost;
                w.rows = match w.want {
                    Some(Want::Open(_)) => s.thread.len(),
                    _ => s.list.len(),
                };
                cv.notify_all();
            }
        }
    });
    let handle = app.handle();
    let driver = shared.clone();
    let _ = std::thread::Builder::new().name("envoy-bench".into()).spawn(move || {
        let report = drive(&driver, &handle);
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap_or_default());
        handle.quit();
    });
    app
}

/// Runs one input and waits for its frames: `apply` runs on the UI thread and returns what
/// the "done" frame must show.
fn step(shared: &Shared, handle: &rune::AppHandle<App>, apply: impl FnOnce(&mut App, &mut Tx) -> Option<Want> + Send + 'static) -> Value {
    let (lock, cv) = &**shared;
    let t0 = Instant::now();
    if let Ok(mut p) = lock.lock() {
        p.wait = Some(Wait { t0, applied: false, want: None, frame_ms: None, done_ms: None, frame_cost: None, done_cost: None, rows: 0 });
    }
    let probe = shared.clone();
    handle.update(move |s, tx| {
        let want = apply(s, tx);
        if let Ok(mut p) = probe.0.lock()
            && let Some(w) = p.wait.as_mut()
        {
            w.applied = true;
            w.want = want;
        }
    });
    let Ok(guard) = lock.lock() else { return Value::Null };
    let (mut p, _) = cv
        .wait_timeout_while(guard, STEP_TIMEOUT, |p| p.wait.as_ref().is_some_and(|w| w.done_ms.is_none()))
        .unwrap_or_else(|e| e.into_inner());
    let w = p.wait.take();
    drop(p);
    // Let follow-up frames (mark-as-read, reloads) settle before the next input.
    std::thread::sleep(Duration::from_millis(120));
    match w {
        Some(w) => json!({
            "frame_ms": w.frame_ms,
            "done_ms": w.done_ms,
            "frame_build_ms": w.frame_cost.map(|c| c.0),
            "frame_encode_ms": w.frame_cost.map(|c| c.1),
            "done_build_ms": w.done_cost.map(|c| c.0),
            "done_encode_ms": w.done_cost.map(|c| c.1),
            "rows": w.rows,
        }),
        None => Value::Null,
    }
}

fn idle(name: &str, settle: Duration, window: Duration) -> Value {
    std::thread::sleep(settle);
    let start = now_ns();
    std::thread::sleep(window);
    json!({ "name": name, "start_ns": start, "end_ns": now_ns() })
}

fn drive(shared: &Shared, handle: &rune::AppHandle<App>) -> Value {
    let (lock, cv) = &**shared;
    let first = {
        let Ok(guard) = lock.lock() else { return json!({ "error": "probe poisoned" }) };
        let (p, timeout) = cv.wait_timeout_while(guard, Duration::from_secs(60), |p| p.first_list_ns.is_none()).unwrap_or_else(|e| e.into_inner());
        if timeout.timed_out() {
            return json!({ "error": "no frame showed the message list within 60 s", "first_frame_ns": p.first_frame_ns, "window_error": handle.window_error() });
        }
        json!({
            "main_ns": MAIN_NS.get(),
            "first_frame_ns": p.first_frame_ns,
            "first_list_ns": p.first_list_ns,
            "first_list_rows": p.first_list_rows,
            "frames_before_list": p.frames_before_list,
            "first_list_build_ms": p.first_list_cost.map(|c| c.0),
            "first_list_encode_ms": p.first_list_cost.map(|c| c.1),
        })
    };
    let mut idles = vec![idle("startup", Duration::from_secs(1), Duration::from_secs(2))];
    let mut keys = Vec::new();
    for _ in 0..KEYS {
        keys.push(step(shared, handle, |s, tx| {
            let ev = KeyEvent { key: Key::Text("j".into()), mods: Mods::default() };
            crate::on_key(s, &ev, tx);
            s.open.map(Want::Open)
        }));
    }
    let mut searches = Vec::new();
    for q in SEARCHES {
        let typed = q.to_string();
        let mut r = step(shared, handle, move |s, _| {
            // What the search field's on_change does for each edit.
            s.search = TextEdit::new(typed.clone());
            s.applied_query = typed;
            s.request_list();
            // Unused while the list has rows (only the empty state reads it); here it marks
            // the moment the results for this request land.
            s.list_loaded = false;
            Some(Want::List(s.list_req))
        });
        r["query"] = json!(q);
        searches.push(r);
    }
    idles.push(idle("after use", Duration::from_secs(1), Duration::from_secs(2)));
    let mut out = first;
    out["keys"] = json!(keys);
    out["searches"] = json!(searches);
    out["idle"] = json!(idles);
    out
}
