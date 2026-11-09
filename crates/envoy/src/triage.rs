//! Acting on several messages at once: the selection bar and the panel the reader shows while
//! rows are selected, and the Move to… and Label… pickers.

use rune::{El, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Combobox, ControlSize, IconButton, IconButtonVariant};

use envoy_core::model::{MessageId, Role};

use crate::app::{App, Picker, key_label};
use crate::theme::Theme;
use crate::{fmt, icons};

/// An icon button with its keyboard shortcut in the tooltip.
fn tool(s: &App, t: &Theme, ic: &'static rune::Icon, label: &str, key: &str, keys: &str, f: impl Fn(&mut App, &mut Tx) + 'static) -> El {
    let mut b = IconButton::new(ic).variant(IconButtonVariant::GhostMuted).label(label).help(label).on_press_tx(f);
    let shortcut = key_label(&s.settings, keys);
    if !shortcut.is_empty() {
        b = b.shortcut(shortcut);
    }
    b.build(&t.k).key(key.to_owned())
}

/// Move to… or Label…: an icon button that opens a searchable list of `ids`' folders (or
/// labels), also opened with `v` (`l`). Enter or a click moves (labels) what's selected, or the
/// open message.
pub fn picker(s: &App, t: &Theme, which: Picker, ids: &[MessageId], layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let (key, ic, label, keys) = match which {
        Picker::Move => ("move-picker", &icons::FOLDER_MOVE, "Move to…", "keys.moveTo"),
        Picker::Label => ("label-picker", &icons::LABEL, "Label…", "keys.label"),
    };
    let state = match which {
        Picker::Move => &s.move_picker,
        Picker::Label => &s.label_picker,
    };
    // Only built while it's open: the closed picker is just its button.
    let (options, note) = if state.open { s.picker_options(which, ids) } else { (Vec::new(), None) };
    let mut trigger = IconButton::new(ic).variant(IconButtonVariant::GhostMuted).label(label).help(label).expanded(state.open);
    let shortcut = key_label(&s.settings, keys);
    if !shortcut.is_empty() {
        trigger = trigger.shortcut(shortcut);
    }
    let placeholder = match (&note, which) {
        (Some(note), _) => format!("{label} {}", note.to_lowercase()),
        (None, _) => label.to_owned(),
    };
    let empty = match which {
        Picker::Move => "No folder by that name",
        Picker::Label => "No label by that name",
    };
    Combobox::new(key, state, &options, move |s: &mut App| s.picker_state(which))
        .label(label)
        .trigger(trigger.build(k).key(format!("t-{key}")))
        .search_placeholder(placeholder)
        .empty_message(empty)
        .width(300.0)
        .max_height(320.0)
        .on_choose(move |s: &mut App, option: &str, tx: &mut Tx| s.pick(which, option, tx))
        .build(k, layer)
}

/// The bar over the selected rows: how many, and what to do with them. Every action here is
/// also a key, the same as for one message.
pub fn selection_bar(s: &App, t: &Theme, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let ids = s.targets();
    let rows: Vec<_> = s.list.iter().filter(|r| s.selection.contains(r.id)).collect();
    let any_unread = rows.iter().any(|r| r.thread_unread);
    let any_unstarred = rows.iter().any(|r| !r.flagged());
    let in_junk = s.all_in_junk(&ids);
    let drafts = s.view_role() == Some(Role::Drafts);
    let mut bar = row()
        .key("selection-bar")
        .role("toolbar")
        .label("Selected messages")
        .w_full()
        .h(40.0)
        .gap(2.0)
        .center_y()
        .child(tool(s, t, &icons::CLOSE, "Clear selection", "sel-clear", "keys.close", |s, _| s.selection.clear()))
        .child(text(format!("{} selected", ids.len())).font(k.ty.ui_medium).color(k.c.text_base).pl(4.0).pr(6.0));
    if ids.len() < s.list.len() {
        let n = s.list.len();
        bar = bar.child(
            Button::new(if s.list_complete { format!("Select all {n}") } else { "Select all".to_owned() })
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut App| s.selection.select_all(&s.list))
                .build(k)
                .key("sel-all"),
        );
    }
    bar = bar.child(el().grow(1.0));
    if !drafts {
        bar = bar.child(tool(s, t, &icons::ARCHIVE, "Archive", "sel-archive", "keys.archive", |s, tx| s.archive(s.targets(), tx)));
    }
    bar = bar
        .child(tool(s, t, &icons::TRASH, if drafts { "Discard drafts" } else { "Delete" }, "sel-delete", "keys.delete", |s, tx| s.delete(s.targets(), tx)));
    if drafts {
        return bar;
    }
    bar.child(tool(s, t, &icons::SPAM, if in_junk { "Not spam" } else { "Report spam" }, "sel-spam", "keys.spam", |s, tx| s.spam(s.targets(), tx)))
        .child(el().w(1.0).h(18.0).mx(6.0).fill(k.c.border_muted))
        .child(tool(
            s,
            t,
            if any_unread { &icons::MAIL_OPEN } else { &icons::MAIL_DOT },
            if any_unread { "Mark as read" } else { "Mark as unread" },
            "sel-read",
            "keys.markUnread",
            move |s, _| s.set_read(s.targets(), any_unread),
        ))
        .child(tool(
            s,
            t,
            if any_unstarred { &icons::STAR } else { &icons::STAR_FILLED },
            if any_unstarred { "Star" } else { "Unstar" },
            "sel-star",
            "keys.star",
            move |s, _| s.set_starred(s.targets(), any_unstarred),
        ))
        .child(picker(s, t, Picker::Move, &ids, layer))
        .children(s.all_gmail(&ids).then(|| picker(s, t, Picker::Label, &ids, layer)))
}

/// The reader while rows are selected: the selection bar, and what the keyboard does now.
pub fn selection_panel(s: &App, t: &Theme, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let n = s.selection.len();
    let noun = if s.conversations() && s.view_role() != Some(Role::Drafts) { "conversation" } else { "message" };
    let key = |action: &str, setting: &str| {
        let label = key_label(&s.settings, setting);
        (!label.is_empty()).then(|| format!("{label} {action}"))
    };
    let hints: Vec<String> = [
        key("archive", "keys.archive"),
        key("delete", "keys.delete"),
        key("move", "keys.moveTo"),
        key("read", "keys.markUnread"),
        key("star", "keys.star"),
        key("select", "keys.select"),
        key("clear", "keys.close"),
    ]
    .into_iter()
    .flatten()
    .collect();
    col()
        .key("selection-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(col().w_full().px(16.0).pt(10.0).child(selection_bar(s, t, layer)))
        .child(
            col()
                .key("selection-summary")
                .w_full()
                .grow(1.0)
                .center()
                .gap(10.0)
                .padding(24.0)
                .child(icon(&icons::LAYERS, 44.0).color(k.c.icon_faint))
                .child(text(format!("{} selected", fmt::count(n, noun))).font(k.ty.body_medium).color(k.c.text_muted))
                // Each key and what it does wraps as one.
                .child(
                    row()
                        .key("selection-keys")
                        .max_w(460.0)
                        .flex_wrap()
                        .justify_center()
                        .gap(14.0)
                        .children(hints.into_iter().map(|h| text(h).font(k.ty.text_small).color(k.c.text_faint))),
                ),
        )
}
