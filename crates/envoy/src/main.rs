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
mod selection;
mod theme;
mod toast;
mod triage;
mod view_account;
mod view_mail;
mod view_settings;

use rune::{AppSpec, Chrome, Cx, El, KeyEvent, Tx, Waker, row};
use rune_kit::{Button, ButtonVariant, Dialog};

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
    if let Some(tl) = toast::region(s, &t) {
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

/// The open conversation full width with only its newest message expanded: with
/// `ENVOY_DEMO_HTML=mail.html` and a tall `--size`, a whole HTML email in one snapshot.
fn scene_html(s: &mut App, theme: &str) {
    let _ = s.settings.set("appearance.theme", serde_json::json!(theme));
    let _ = s.settings.set("inbox.readingPane", serde_json::json!("off"));
    scene_open(s);
    s.expanded.clear();
    if let Some(m) = s.thread.last() {
        s.expanded.insert(m.summary.id);
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
    ("forward", |s| {
        scene_open(s);
        s.compose_reply(None, false, true);
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
    ("html", |s| scene_html(s, "light")),
    ("html-dark", |s| scene_html(s, "dark")),
    ("sent", |s| {
        s.view = View::Role { account: Some("demo".into()), role: Role::Sent };
        s.list = demo::list_for(&s.view);
    }),
    ("select", scene_select),
    ("select-off", |s| {
        let _ = s.settings.set("inbox.readingPane", serde_json::json!("off"));
        scene_select(s);
    }),
    ("picker", |s| {
        scene_open(s);
        s.open_picker(app::Picker::Move);
    }),
    ("picker-select", |s| {
        scene_select(s);
        s.open_picker(app::Picker::Move);
    }),
    ("label", |s| {
        scene_open(s);
        s.open_picker(app::Picker::Label);
    }),
    ("toast", |s| {
        // The second conversation archived: its notice, with Undo, under the list and reader.
        scene_open(s);
        let id = s.list[1].id;
        s.archive(vec![id], &mut Tx::default());
        s.toast("Couldn't reach imap.gmail.com: the network is down", true);
    }),
    ("toast-compose", |s| {
        scene_open(s);
        s.compose_reply(None, false, false);
        let id = s.list[1].id;
        s.archive(vec![id], &mut Tx::default());
    }),
    ("counts", |s| {
        // Two unread conversations opened (and so read): the sidebar's Inbox count follows.
        let unread: Vec<_> = s.list.iter().filter(|m| m.thread_unread).map(|m| m.id).take(2).collect();
        for id in unread {
            s.open_message(id);
        }
    }),
    ("drafts", scene_drafts),
    ("draft", |s| {
        scene_drafts(s);
        if let Some(id) = s.list.first().map(|m| m.id) {
            s.open_message(id);
        }
    }),
    ("draft-notice", |s| {
        scene_drafts(s);
        if let Some(d) = s.list.first().and_then(|m| demo::draft_for(m.id)) {
            // A copy comes without the original's files.
            let copy = envoy_core::model::Draft { attachments: vec![], ..d };
            s.show_draft(0, copy, Default::default(), false, Some(envoy_core::model::DraftNotice::Copy("it's signed or encrypted".into())));
        }
    }),
];

/// The first conversation open, then it and the next two selected (x, shift+j, shift+j): the
/// reader shows the selection, the keyboard on the third.
fn scene_select(s: &mut App) {
    scene_open(s);
    let first = s.list[0].id;
    s.toggle_selected(first);
    let second = s.selection.extend(&s.list, first, 1).unwrap_or(first);
    s.selection.extend(&s.list, second, 1);
}

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
