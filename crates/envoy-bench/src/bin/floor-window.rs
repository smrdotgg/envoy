#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! The OS's own launch floor, for `envoy-bench app`: the lightest native window we can open on
//! every platform (winit + softbuffer: CPU pixels handed to the window server, no GPU device,
//! no shaders, no fonts, no Rune) drawing "Hello World" in a built-in bitmap font. What it takes
//! from spawn to its first presented frame is what any app pays the OS to launch and show a
//! window: exec and dynamic loading, the system's per-launch checks (Gatekeeper and friends on
//! macOS, Defender on Windows), the window server round trips and the first present. Envoy's
//! cold start is gated above it (see crates/envoy-bench/src/app_bench.rs).
//!
//! With `ENVOY_BENCH=REPORT.json` it writes the same report Envoy's probe writes (`main_ns`,
//! `first_frame_ns`, `first_list_ns`, one idle window) and quits; otherwise it stays open.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// 5×7 glyphs, one row per byte, high bit on the left (bits 4..0).
const GLYPHS: &[(char, [u8; 7])] = &[
    ('H', [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]),
    ('e', [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110]),
    ('l', [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]),
    ('o', [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110]),
    ('W', [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010]),
    ('r', [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000]),
    ('d', [0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10011, 0b01101]),
    (' ', [0; 7]),
];

/// Fills `buf` (`w`×`h`, 0RGB) white and draws `text` centred, each font pixel `px` wide.
fn draw(buf: &mut [u32], w: usize, h: usize, text: &str, px: usize) {
    buf.fill(0x00FF_FFFF);
    let (cols, rows) = (text.chars().count() * 6, 7);
    let (x0, y0) = (w.saturating_sub(cols * px) / 2, h.saturating_sub(rows * px) / 2);
    for (i, c) in text.chars().enumerate() {
        let Some((_, g)) = GLYPHS.iter().find(|(gc, _)| *gc == c) else { continue };
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                for dy in 0..px {
                    let y = y0 + row * px + dy;
                    let x = x0 + (i * 6 + col) * px;
                    if y < h && x + px <= w {
                        buf[y * w + x..y * w + x + px].fill(0x0020_2428);
                    }
                }
            }
        }
    }
}

enum Phase {
    /// Waiting for the first frame.
    Launching,
    /// After the first frame, letting the process settle before memory is sampled.
    Settling(Instant),
    /// The idle window the harness samples memory in.
    Idle { start_ns: u64, until: Instant },
}

struct Floor {
    report: Option<PathBuf>,
    main_ns: u64,
    first_frame_ns: Option<u64>,
    phase: Phase,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl Floor {
    fn finish(&mut self, event_loop: &ActiveEventLoop, start_ns: u64) {
        if let Some(path) = &self.report {
            let ff = self.first_frame_ns.unwrap_or(0);
            let json = format!(
                "{{\"main_ns\": {}, \"first_frame_ns\": {ff}, \"first_list_ns\": {ff}, \"idle\": [{{\"name\": \"startup\", \"start_ns\": {start_ns}, \"end_ns\": {}}}]}}",
                self.main_ns,
                now_ns()
            );
            let _ = std::fs::write(path, json);
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for Floor {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("Hello World").with_inner_size(winit::dpi::LogicalSize::new(1280.0, 820.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("a window"));
        let context = softbuffer::Context::new(window.clone()).expect("a softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("a softbuffer surface"));
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
                let size = window.inner_size();
                let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
                if surface.resize(w, h).is_err() {
                    return;
                }
                let Ok(mut buf) = surface.buffer_mut() else { return };
                let px = (6.0 * window.scale_factor()).round().max(1.0) as usize;
                draw(&mut buf, w.get() as usize, h.get() as usize, "Hello World", px);
                if buf.present().is_ok() && self.first_frame_ns.is_none() {
                    self.first_frame_ns = Some(now_ns());
                    if self.report.is_some() {
                        let settle = Instant::now() + Duration::from_secs(1);
                        self.phase = Phase::Settling(settle);
                        event_loop.set_control_flow(ControlFlow::WaitUntil(settle));
                    }
                }
            }
            _ => {}
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        let now = Instant::now();
        match self.phase {
            Phase::Settling(at) if now >= at => {
                let until = now + Duration::from_secs(2);
                self.phase = Phase::Idle { start_ns: now_ns(), until };
                event_loop.set_control_flow(ControlFlow::WaitUntil(until));
            }
            Phase::Idle { start_ns, until } if now >= until => self.finish(event_loop, start_ns),
            _ => {}
        }
    }
}

fn main() {
    let main_ns = now_ns();
    let report = std::env::var_os("ENVOY_BENCH").map(PathBuf::from);
    let event_loop = EventLoop::new().expect("an event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = Floor { report, main_ns, first_frame_ns: None, phase: Phase::Launching, window: None, surface: None };
    event_loop.run_app(&mut app).expect("the event loop ran");
}
