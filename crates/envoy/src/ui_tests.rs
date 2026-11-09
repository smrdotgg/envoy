//! The app as its window runs it: Envoy's view, key handler and poll in a Rune runtime (no GPU),
//! driven by key presses and clicks, and read back as what was drawn where.

use rune::{Env, FileDialogKind, Key, KeyEvent, Mods, Runtime};

use crate::app::App;

/// The window's default size.
const SIZE: (f32, f32) = (1280.0, 820.0);

fn window(theme: &str) -> Runtime<App> {
    let mut app = App::new(true); // demo: settings.json is never written
    crate::demo::populate(&mut app);
    let _ = app.settings.set("appearance.theme", serde_json::json!(theme));
    let mut rt = Runtime::new(app, crate::view, SIZE, Env::new(theme == "dark", 2.0));
    rt.on_key = Some(crate::on_key);
    rt.poll = Some(crate::poll);
    settle(&mut rt);
    rt
}

/// Half a second of frames: what a press set off has finished moving.
fn settle(rt: &mut Runtime<App>) {
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
}

fn press(rt: &mut Runtime<App>, ev: KeyEvent) {
    rt.key(ev);
    settle(rt);
}

fn letter(c: &str) -> KeyEvent {
    KeyEvent { key: Key::Text(c.into()), mods: Mods::default() }
}

/// ⇧ and the shortcut modifier (⌘ on macOS, Ctrl elsewhere) with a letter, as the window gets it.
fn shift_mod(c: char) -> KeyEvent {
    let mac = cfg!(target_os = "macos");
    KeyEvent { key: Key::Command(c), mods: Mods { shift: true, cmd: true, alt: false, ctrl: !mac, logo: mac } }
}

fn click(rt: &mut Runtime<App>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("nothing keyed {key} on screen"));
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x, y);
    settle(rt);
}

fn inside(inner: [f32; 4], outer: [f32; 4]) -> bool {
    inner[0] >= outer[0] && inner[1] >= outer[1] && inner[0] + inner[2] <= outer[0] + outer[2] + 0.5 && inner[1] + inner[3] <= outer[1] + outer[3] + 0.5
}

#[test]
fn the_composers_footer_shows_an_attach_button_that_opens_the_picker() {
    for theme in ["light", "dark"] {
        let mut rt = window(theme);
        press(&mut rt, letter("c"));
        assert!(rt.state.compose.is_some(), "c opens a composer");
        let (attach, opacity) = rt.presented("compose-attach").expect("an Attach button");
        let (footer, _) = rt.presented("compose-footer").expect("the footer");
        let (send, _) = rt.presented("compose-send").expect("Send");
        assert!(attach[2] >= 20.0 && attach[3] >= 20.0, "{theme}: a button-sized target, not {attach:?}");
        assert!(opacity > 0.99, "{theme}: drawn, not faded ({opacity})");
        assert!(inside(attach, footer) && inside(attach, [0.0, 0.0, SIZE.0, SIZE.1]), "{theme}: in the footer, in the window: {attach:?} {footer:?}");
        assert!(attach[0] >= send[0] + send[2], "{theme}: after Send");
        // Nothing covers it: its centre is it (or its icon).
        let (x, y) = (attach[0] + attach[2] / 2.0, attach[1] + attach[3] / 2.0);
        let top = rt.element_at(x, y).expect("something at its centre");
        assert!(top.path().contains("compose-attach"), "{theme}: {} covers it", top.path());
        // Pressed, it asks for the system's picker, several files at once.
        click(&mut rt, "compose-attach");
        let (kind, options) = rt.file_dialog().expect("a file dialog");
        assert_eq!((kind, options.title.as_str()), (FileDialogKind::OpenMany, "Attach files"));
    }
}

#[test]
fn the_attach_shortcut_works_while_typing_in_the_body() {
    let mut rt = window("light");
    press(&mut rt, letter("c"));
    click(&mut rt, "field-compose-body");
    for c in ["h", "i"] {
        press(&mut rt, letter(c));
    }
    assert_eq!(rt.state.compose.as_ref().map(|c| c.body.text.as_str()), Some("hi"), "typed into the body");
    // keys.attach: mod+shift+a.
    press(&mut rt, shift_mod('a'));
    assert_eq!(rt.state.compose.as_ref().map(|c| c.body.text.as_str()), Some("hi"), "the field didn't take it");
    let (kind, _) = rt.file_dialog().expect("the shortcut opens the picker");
    assert_eq!(kind, FileDialogKind::OpenMany);
    // What's picked shows as chips.
    let dir = std::env::temp_dir().join(format!("envoy-ui-attach-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("notes.txt"), dir.join("plan.md"));
    std::fs::write(&a, b"hello").unwrap();
    std::fs::write(&b, b"# Plan").unwrap();
    rt.resolve_file_dialog_paths(vec![a, b]).unwrap();
    settle(&mut rt);
    let names: Vec<String> = rt.state.compose.as_ref().unwrap().files.iter().map(|f| f.filename.clone()).collect();
    assert_eq!(names, ["notes.txt", "plan.md"]);
    assert!(rt.presented("compose-files").is_some(), "shown as chips");
    let _ = std::fs::remove_dir_all(&dir);
}
