#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Envoy: a fast, native, configurable email client built on Rune.
//!
//! `envoy` opens the app. `envoy --demo` opens it with sample mail and no network.
//! Rune's headless tooling works too: `envoy --demo --snap out.png --scene open`.

mod app;
mod bench;
mod compose;
mod demo;
mod fmt;
mod html_view;
mod icons;
mod reader;
mod theme;
mod view_account;
mod view_mail;
mod view_settings;

use rune::{AppSpec, Chrome, Cx, El, KeyEvent, Tx, Waker, row};
use rune_kit::{Button, ButtonVariant, Dialog, Toast, ToastRegion};

use envoy_core::engine::{Command, Engine};
use envoy_core::model::{Role, View};

use app::{App, Screen};

fn is_demo() -> bool {
    std::env::var_os("ENVOY_DEMO").is_some() || std::env::args().any(|a| a == "--demo") || rune::extra_args().iter().any(|a| a == "demo" || a == "--demo")
}

thread_local! {
    /// The app as `main` started it, before Rune opened the window (`start_early`); `init`,
    /// which Rune calls on this thread, takes it.
    static EARLY: std::cell::RefCell<Option<App>> = const { std::cell::RefCell::new(None) };
}

/// The window's waker, once `setup` has it. The engine starts before the window exists: what it
/// sends before then waits in its channel for the first frame's `poll`.
static WAKER: std::sync::OnceLock<Waker> = std::sync::OnceLock::new();

/// Starts the engine and asks for the first list at the top of `main`, so the cache opens and
/// the list is read on the engine's thread while the OS and the GPU bring the window up: the
/// first frame shows the list when it's in by then (it usually is), instead of a frame later.
fn start_early() {
    if is_demo() {
        return;
    }
    let mut app = App::new(false);
    start_engine(&mut app);
    EARLY.with(|e| *e.borrow_mut() = Some(app));
}

fn start_engine(s: &mut App) {
    let wake = || {
        if let Some(w) = WAKER.get() {
            w();
        }
    };
    match Engine::start(wake, s.settings.clone()) {
        Ok(engine) => {
            s.engine = Some(engine);
            let view = s.view.clone();
            s.request_list();
            s.send(Command::Watch { view });
        }
        Err(e) => s.toast(format!("Couldn't open the mail cache: {e:#}"), true),
    }
}

fn init() -> App {
    if let Some(app) = EARLY.with(|e| e.borrow_mut().take()) {
        return app;
    }
    let demo = is_demo();
    let mut app = App::new(demo);
    if demo {
        demo::populate(&mut app);
    }
    app
}

fn setup(s: &mut App, waker: Waker) {
    s.image_cache = Some(rune::ImageCache::new(waker.clone()));
    if s.demo {
        return;
    }
    let _ = WAKER.set(waker);
    if s.engine.is_none() {
        start_engine(s);
    }
}

fn poll(s: &mut App, clock: f64, tx: &mut Tx) -> bool {
    let a = s.pump(tx);
    let b = s.tick(clock, tx);
    a || b
}

fn on_key(s: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if rune_kit::shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    s.on_key(ev, tx)
}

fn toasts(s: &App, t: &theme::Theme) -> Option<El> {
    if s.toasts.is_empty() {
        return None;
    }
    let k = &t.k;
    let mut region = ToastRegion::new();
    for toast in &s.toasts {
        let id = toast.id;
        let mut b = Toast::new(format!("toast-{id}"), toast.text.clone());
        if toast.error {
            b = b.icon(k, &icons::ALERT);
        }
        if toast.undo.is_some() {
            b = b.action(k, "Undo", true, move |s: &mut App, tx: &mut Tx| s.undo_toast(id, tx));
        }
        if let Some(path) = toast.open_path.clone() {
            b = b.action(k, "Show", false, move |_: &mut App, _: &mut Tx| {
                let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
                let _ = open::that_detached(dir);
            });
        }
        b = b.on_close(move |s: &mut App| s.toasts.retain(|t| t.id != id));
        region = region.toast(b.build(k));
    }
    // Bottom-left, clear of the composer at the bottom right.
    Some(region.build(k).pin(Some(20.0), None, None, Some(20.0)))
}

fn view(s: &App, cx: &mut Cx<App>) -> El {
    let t = theme::build(&s.settings, cx.system_dark());
    let k = &t.k;
    let mut layer: Vec<El> = Vec::new();
    let body = view_mail::mail_screen(s, &t, cx, &mut layer);
    if s.screen == Screen::Mail
        && let Some(c) = compose::composer(s, &t, &mut layer)
    {
        layer.push(c);
    }
    if let Some(ids) = &s.confirm_delete {
        let ids = ids.clone();
        Dialog::new("confirm-delete", "Move to Trash?")
            .open(true)
            .description("You can turn this question off in Settings → Behavior.")
            .child(
                row()
                    .gap(8.0)
                    .justify_end()
                    .w_full()
                    .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut App| s.confirm_delete = None).build(k).key("confirm-cancel"))
                    .child(
                        Button::new("Move to Trash")
                            .variant(ButtonVariant::Danger)
                            .on_press_tx(move |s: &mut App, tx: &mut Tx| s.delete_now(ids.clone(), tx))
                            .build(k)
                            .key("confirm-ok"),
                    ),
            )
            .on_close(|s: &mut App| s.confirm_delete = None)
            .build(k, &mut layer);
    }
    if let Some(tl) = toasts(s, &t) {
        layer.push(tl);
    }
    rune_kit::root(body.fill(t.page), layer)
}

fn scene_open(s: &mut App) {
    if let Some(id) = s.list.first().map(|m| m.id) {
        s.open = Some(id);
        s.thread = demo::thread_for(id);
        s.thread_for = Some(id);
        for m in &s.thread {
            s.expanded.insert(m.summary.id);
        }
    }
}

type Scene = (&'static str, fn(&mut App));

const SCENES: &[Scene] = &[
    ("open", scene_open),
    ("light", |s| {
        let _ = s.settings.set("appearance.theme", serde_json::json!("light"));
        scene_open(s);
    }),
    ("light-compose", |s| {
        let _ = s.settings.set("appearance.theme", serde_json::json!("light"));
        scene_open(s);
        s.compose_reply(None, false, false);
    }),
    ("dark", |s| {
        let _ = s.settings.set("appearance.theme", serde_json::json!("dark"));
        scene_open(s);
    }),
    ("compact", |s| {
        let _ = s.settings.set("appearance.density", serde_json::json!("compact"));
        scene_open(s);
    }),
    ("bottom", |s| {
        let _ = s.settings.set("inbox.readingPane", serde_json::json!("bottom"));
        scene_open(s);
    }),
    ("compose", |s| {
        scene_open(s);
        s.compose_reply(None, false, false);
    }),
    ("settings", |s| s.screen = Screen::Settings),
    ("settings-keys", |s| {
        s.screen = Screen::Settings;
        s.settings_ui.category = Some(6);
    }),
    ("accounts", |s| {
        s.screen = Screen::Settings;
        s.settings_ui.accounts = true;
    }),
    ("add", |s| s.screen = Screen::AddAccount),
    ("welcome", |s| {
        s.accounts.clear();
        s.mailboxes.clear();
        s.screen = Screen::AddAccount;
    }),
    ("sent", |s| {
        s.view = View::Role { account: Some("demo".into()), role: Role::Sent };
        s.list = demo::list_for(&s.view);
    }),
    ("drafts", scene_drafts),
    ("draft", |s| {
        scene_drafts(s);
        if let Some(id) = s.list.first().map(|m| m.id) {
            s.open_message(id);
        }
    }),
];

fn scene_drafts(s: &mut App) {
    s.view = View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() };
    s.list = demo::list_for(&s.view);
}

fn main() {
    bench::started();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("envoy_core=info")).init();
    rune::prewarm_gpu();
    start_early();
    let spec = AppSpec {
        min_size: Some((760.0, 480.0)),
        scenes: SCENES,
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("Envoy", (1280.0, 820.0), init, view)
    };
    let chrome = Chrome { full_size_content: true, traffic_lights: Some((16.0, 25.0)), drag_regions: true };
    // Closing writes the composer's unsaved edits before the process can exit.
    bench::attach(rune::App::new(spec).chrome(chrome).on_close(|s, _| s.on_close())).run();
}
