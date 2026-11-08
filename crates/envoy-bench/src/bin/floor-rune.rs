#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! The smallest Rune app, for `envoy-bench app`: a window the size of Envoy's drawing "Hello
//! World", started the way Rune asks (`prewarm_gpu` first thing in `main`). Its launch minus
//! the bare window's (floor-window.rs) is what Rune itself costs to start: the GPU device and
//! surface, the render pipeline, fonts, the first layout and draw. Its idle memory is Rune's
//! and the GPU driver's own footprint, which no app on Rune can go below.
//!
//! With `ENVOY_BENCH=REPORT.json` it writes the same report Envoy's probe writes (`main_ns`,
//! `first_frame_ns`, `first_list_ns`, one idle window) and quits; otherwise it stays open.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rune::{AppSpec, Cx, El, col, text};

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn view(_: &(), _: &mut Cx<()>) -> El {
    col().w_full().h_full().center().child(text("Hello World").font(rune::TextStyle::new(32.0, 600.0)))
}

fn main() {
    let main_ns = now_ns();
    rune::prewarm_gpu();
    let app = rune::App::new(AppSpec::new("Hello World", (1280.0, 820.0), || (), view));
    let Some(path) = std::env::var_os("ENVOY_BENCH").map(PathBuf::from) else {
        app.run();
        return;
    };
    let first: Arc<(Mutex<Option<u64>>, Condvar)> = Arc::default();
    let hook = first.clone();
    let app = app.on_frame_drawn(move |_: &rune::Runtime<()>| {
        let (lock, cv) = &*hook;
        if let Ok(mut f) = lock.lock()
            && f.is_none()
        {
            *f = Some(now_ns());
            cv.notify_all();
        }
    });
    let handle = app.handle();
    let _ = std::thread::Builder::new().name("floor-bench".into()).spawn(move || {
        let (lock, cv) = &*first;
        let ff = {
            let Ok(guard) = lock.lock() else { return };
            let (f, _) = cv.wait_timeout_while(guard, Duration::from_secs(60), |f| f.is_none()).unwrap_or_else(|e| e.into_inner());
            *f
        };
        let json = match ff {
            Some(ff) => {
                std::thread::sleep(Duration::from_secs(1));
                let start = now_ns();
                std::thread::sleep(Duration::from_secs(2));
                format!(
                    "{{\"main_ns\": {main_ns}, \"first_frame_ns\": {ff}, \"first_list_ns\": {ff}, \"idle\": [{{\"name\": \"startup\", \"start_ns\": {start}, \"end_ns\": {}}}]}}",
                    now_ns()
                )
            }
            None => "{\"error\": \"no frame within 60 s\"}".to_string(),
        };
        let _ = std::fs::write(&path, json);
        handle.quit();
    });
    app.run();
}
