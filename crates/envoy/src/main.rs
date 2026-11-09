#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Envoy: a fast, native, configurable email client built on Rune.
//!
//! `envoy` opens the app. `envoy --demo` opens it with sample mail and no network.
//! Rune's headless tooling works too, in debug builds: `envoy --demo --snap out.png --scene open`.
//! Release builds leave it out (Rune's `headless` feature is off).

mod app;
mod bench;
mod categories;
mod compose;
mod demo;
#[cfg(test)]
mod demo_tests;
mod fmt;
mod html_view;
mod icons;
#[cfg(test)]
mod live_tests;
mod reader;
mod selection;
mod theme;
mod toast;
mod triage;
#[cfg(test)]
mod ui_tests;
mod view_account;
mod view_mail;
mod view_settings;

use rune::{AppSpec, Chrome, Cx, El, KeyEvent, Tx, Waker, row};
use rune_kit::{Button, ButtonVariant, Dialog};

use envoy_core::engine::{Command, Engine};
use envoy_core::model::{Role, View};

use app::{App, Screen};

/// The folder every file the tests touch goes in (settings.json, the cache, drafts kept at
/// close, exports): this test run's own, emptied first, so no test reads the settings of the
/// machine it runs on or writes anything of its user's. [`App::new`] sets it, before anything
/// asks where a file goes.
#[cfg(test)]
pub(crate) fn test_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("envoy-app-tests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(envoy_core::paths::use_home(home.clone()), "something asked where Envoy's files go before the tests' folder was set");
        home
    })
}

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
/// Demo mode first puts its files in a folder of their own, and goes offline for good.
fn start_early() {
    let demo = is_demo();
    if demo {
        if !envoy_core::demo::isolate() {
            log::warn!("demo mode's files couldn't be kept apart from yours");
        }
        envoy_core::net::go_offline();
    }
    let mut app = App::new(demo);
    start_engine(&mut app);
    EARLY.with(|e| *e.borrow_mut() = Some(app));
}

/// Starts the engine (on the demo's in-process server in demo mode), and asks for the list.
fn start_engine(s: &mut App) {
    let wake = || {
        if let Some(w) = WAKER.get() {
            w();
        }
    };
    let started = if s.demo { demo::start(s, wake) } else { Engine::start(wake, s.settings.clone()).map(|e| s.engine = Some(e)).map_err(|e| format!("{e:#}")) };
    match started {
        Ok(()) => {
            let view = s.view.clone();
            s.request_list();
            s.send(Command::Watch { view });
        }
        Err(e) => s.toast(format!("Couldn't open the mail cache: {e}"), true),
    }
}

fn init() -> App {
    if let Some(app) = EARLY.with(|e| e.borrow_mut().take()) {
        return app;
    }
    App::new(is_demo())
}

fn setup(s: &mut App, waker: Waker) {
    s.image_cache = Some(rune::ImageCache::new(waker.clone()));
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
    if s.screen == Screen::Mail {
        // What the composer opens over itself (suggestions, the From list) goes after it: an
        // overlay follows the element it's anchored to.
        let mut over = Vec::new();
        if let Some(c) = compose::composer(s, &t, &mut over) {
            layer.push(c);
        }
        layer.extend(over);
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

/// The first conversation of the inbox (its Important tab) open, every message of it shown.
/// Scenes run on demo mode's engine: each waits for it ([`App::settle`]) before it's drawn.
fn scene_open(s: &mut App) {
    s.settle();
    if let Some(id) = s.list.first().map(|m| m.id) {
        s.open_message(id);
        s.settle();
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

/// The demo account's `role` (Sent, Snoozed…), as its sidebar row opens it.
fn scene_view(s: &mut App, role: Role) {
    s.settle();
    s.go(View::Role { account: Some(demo::ACCOUNT.into()), role });
    s.settle();
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
        s.settle();
    }),
    ("compose-suggest", |s| {
        // Recipient suggestions for "l" in a new message's To field.
        scene_open(s);
        s.compose_new();
        if let Some(c) = &mut s.compose {
            c.to.insert("l");
        }
        s.recipients_edited(app::Recipients::To);
        s.settle_until(|s| s.compose.as_ref().is_some_and(|c| !c.suggest.items.is_empty()));
    }),
    ("settings", |s| {
        s.settle();
        s.screen = Screen::Settings;
    }),
    ("settings-keys", |s| {
        s.settle();
        s.screen = Screen::Settings;
        s.settings_ui.category = Some(6);
    }),
    ("accounts", |s| {
        s.settle();
        s.screen = Screen::Settings;
        s.settings_ui.accounts = true;
    }),
    ("add", |s| {
        s.settle();
        s.screen = Screen::AddAccount;
    }),
    ("welcome", |s| {
        s.settle();
        s.accounts.clear();
        s.mailboxes.clear();
        s.screen = Screen::AddAccount;
    }),
    ("html", |s| scene_html(s, "light")),
    ("html-dark", |s| scene_html(s, "dark")),
    ("sent", |s| scene_view(s, Role::Sent)),
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
    ("snooze", |s| {
        scene_open(s);
        s.open_snooze_picker();
    }),
    ("snooze-typed", |s| {
        scene_open(s);
        s.open_snooze_picker();
        s.snooze_picker.search = rune::TextEdit::new("fri 9am");
    }),
    ("snoozed", |s| scene_view(s, Role::Snoozed)),
    ("woken", |s| {
        // Back from a snooze: a message whose time came while Envoy was closed, woken on the
        // next check, atop the inbox (its own tab of it), marked, until it's dealt with.
        s.settle();
        if let Some(server) = s.demo_server.clone() {
            demo::snoozed_till(&server, chrono::Utc::now().timestamp(), 600);
        }
        s.send(Command::SyncNow);
        s.settle_until(|s| s.list.first().is_some_and(|m| m.flags & envoy_core::model::flags::WOKEN != 0));
    }),
    ("toast", |s| {
        // The second conversation archived: its notice, with Undo, under the list and reader.
        scene_open(s);
        let id = s.list[1].id;
        s.archive(vec![id], &mut Tx::default());
        s.settle();
        s.toast("Couldn't reach imap.gmail.com: the network is down", true);
    }),
    ("toast-compose", |s| {
        scene_open(s);
        s.compose_reply(None, false, false);
        let id = s.list[1].id;
        s.archive(vec![id], &mut Tx::default());
        s.settle();
    }),
    ("counts", |s| {
        // Two unread conversations opened (and so read): the sidebar's Inbox count follows.
        s.settle();
        let unread: Vec<_> = s.list.iter().filter(|m| m.thread_unread).map(|m| m.id).take(2).collect();
        for id in unread {
            s.open_message(id);
            s.settle();
        }
    }),
    ("drafts", scene_drafts),
    // The split inbox: the Important tab, the first conversation open, every tab's count.
    ("split", scene_open),
    ("split-newsletters", |s| {
        s.settle();
        s.show_category(envoy_core::model::Category::Newsletters);
        scene_open(s);
    }),
    // A newsletter moved to Important (alt+1): off the tab, a rule for its sender, Undo.
    ("split-moved", |s| {
        s.settle();
        s.show_category(envoy_core::model::Category::Newsletters);
        scene_open(s);
        let ids = s.targets();
        s.move_to_category(ids, envoy_core::model::Category::Important, &mut Tx::default());
        s.settle();
    }),
    // A search looks in every tab; each result says which it's in.
    ("split-search", |s| {
        s.settle();
        s.applied_query = "the".into();
        s.search = rune::TextEdit::new("the");
        s.request_list();
        s.settle();
    }),
    ("split-narrow", |s| {
        s.list_width = 330.0;
        scene_open(s);
    }),
    // The reader dragged narrow, about 300 points at the default size: Reply, Reply all and
    // Forward as icons, the subject's chips below it.
    ("reader-narrow", |s| {
        s.list_width = 715.0;
        scene_open(s);
    }),
    ("split-empty", |s| {
        s.settle();
        s.show_category(envoy_core::model::Category::Other);
        s.settle();
        let ids: Vec<_> = s.list.iter().map(|m| m.id).collect();
        s.archive(ids, &mut Tx::default());
        s.settle();
        s.toasts.clear();
    }),
    ("split-off", |s| {
        let _ = s.settings.set("inbox.split", serde_json::json!(false));
        s.settle();
        s.request_list();
        scene_open(s);
    }),
    ("settings-rules", |s| {
        s.settle();
        let _ = s.settings.set("inbox.rules", serde_json::json!([{ "from": "*@github.com", "category": "notifications" }, { "list": "rust-lang.org", "category": "newsletters" }, { "from": "maya@northwind.dev", "category": "important" }]));
        s.screen = Screen::Settings;
        s.settings_ui.category = Some(1);
    }),
    ("draft", |s| {
        scene_drafts(s);
        if let Some(id) = s.list.first().map(|m| m.id) {
            s.open_message(id);
            s.settle_until(|s| s.compose.is_some());
        }
    }),
    ("draft-notice", |s| {
        scene_drafts(s);
        // A draft Envoy can't edit in place opens as a copy, without the original's files.
        s.show_draft(0, demo::draft(), Default::default(), false, Some(envoy_core::model::DraftNotice::Copy("it's signed or encrypted".into())));
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
    s.settle();
    s.go(View::Mailbox { account: demo::ACCOUNT.into(), mailbox: demo::DRAFTS.into() });
    s.settle();
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
    // Closing writes the composer's unsaved edits before the process can exit; demo mode's
    // folder then goes.
    bench::attach(rune::App::new(spec).chrome(chrome).on_close(|s, _| {
        s.on_close();
        if s.demo {
            envoy_core::demo::clean_up();
        }
    }))
    .run();
}
