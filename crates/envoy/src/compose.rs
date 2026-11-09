//! The composer: a floating window at the bottom right, as in Gmail.

use rune::{Cursor, El, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Placement, PopupItem, PopupList, Select, Spinner, TextInput};

use crate::app::{App, Compose, Recipients, key_label};
use envoy_core::model::{DraftAttachment, FileStatus};
use crate::{fmt, icons};
use crate::theme::Theme;

fn field_row(t: &Theme, label: &str, input: El, trailing: Option<El>) -> El {
    let k = &t.k;
    let mut r = row()
        .w_full()
        .px(16.0)
        .h(40.0)
        .gap(8.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
        .child(text(label.to_owned()).font(k.ty.text_small).color(k.c.text_faint).w(56.0))
        .child(col().grow(1.0).min_w(0.0).child(input));
    if let Some(e) = trailing {
        r = r.child(e);
    }
    r
}

fn plain_input(edit: &rune::TextEdit, placeholder: &str, key: &str, access: fn(&mut App) -> &mut rune::TextEdit, k: &rune_kit::Kit) -> El {
    TextInput::new(edit)
        .plain()
        .placeholder(placeholder)
        .label(placeholder)
        .on_edit(move |s: &mut App| access(s))
        .on_change(|s: &mut App, _: &mut Tx| s.compose_edited())
        .build(k)
        .key(key.to_owned())
}

fn c(s: &mut App) -> &mut Compose {
    s.compose.as_mut().expect("composer is open while its fields exist")
}

/// A To, Cc or Bcc field, as wide as its row (several recipients fit): typing in it suggests
/// people (app/suggest.rs), in a list under it that takes no focus, so the caret and every key
/// stay in the field.
fn recipients_input(cm: &Compose, f: Recipients, placeholder: &str, k: &rune_kit::Kit, layer: &mut Vec<El>) -> El {
    let key = format!("field-compose-{}", f.key());
    let shown = cm.suggestions(f);
    let mut input = TextInput::new(cm.recipients(f))
        .plain()
        .width(None)
        .placeholder(placeholder)
        .label(placeholder)
        .on_edit(move |s: &mut App| c(s).recipients_mut(f))
        .on_change(move |s: &mut App, _: &mut Tx| s.recipients_edited(f))
        .on_key(move |s: &mut App, ev, tx| s.suggestion_key(f, ev, tx));
    if let Some((items, active)) = shown {
        input = input.active_descendant(Some(format!("compose-suggest-option-{active}")));
        let emails: Vec<String> = items.iter().map(|a| a.email.clone()).collect();
        let rows = items.iter().enumerate().map(|(i, a)| {
            let name = a.name.trim();
            let row = PopupItem::new(i.to_string(), if name.is_empty() { a.email.as_str() } else { name });
            if name.is_empty() { row } else { row.description(&a.email) }
        });
        PopupList::new("compose-suggest", key.clone())
            .placement(Placement::BottomStart)
            .offset(6.0)
            .width(380.0)
            .label("Suggested recipients")
            .items(rows)
            .active(Some(active))
            .on_active(|s: &mut App, i| s.hover_suggestion(i))
            .on_choose(move |s: &mut App, id: &str, _: &mut Tx| {
                if let Some(email) = id.parse::<usize>().ok().and_then(|i| emails.get(i)) {
                    s.accept_suggestion(f, email);
                }
            })
            .build(k, layer);
    }
    input.build(k).key(key)
}

pub fn composer(s: &App, t: &Theme, layer: &mut Vec<El>) -> Option<El> {
    let k = &t.k;
    let cm = s.compose.as_ref()?;
    let title_bar = row()
        .key("compose-title")
        .w_full()
        .h(42.0)
        .pl(16.0)
        .pr(6.0)
        .gap(4.0)
        .center_y()
        .fill(k.c.layer_02)
        .cursor(Cursor::Pointer)
        .on_press(|s: &mut App| {
            if let Some(c) = &mut s.compose {
                c.minimized = !c.minimized;
            }
        })
        .child(text(if cm.title.is_empty() { "New message".to_owned() } else { cm.title.clone() }).font(k.ty.text_small_medium).color(k.c.text_base).truncate().grow(1.0).min_w(0.0))
        .child(
            IconButton::new(if cm.minimized { &icons::PLUS } else { &icons::MINUS })
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(if cm.minimized { "Expand" } else { "Minimize" })
                .on_press(|s: &mut App| {
                    if let Some(c) = &mut s.compose {
                        c.minimized = !c.minimized;
                    }
                })
                .build(k)
                .key("compose-min"),
        )
        .child(
            IconButton::new(&icons::CLOSE)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label("Discard draft")
                .help("Discard")
                .on_press_tx(|s: &mut App, tx: &mut Tx| s.discard_compose(tx))
                .build(k)
                .key("compose-close"),
        );
    let width = 540.0;
    if cm.minimized {
        let card = col().key("composer").w(320.0).radius(12.0).fill(t.card).box_shadow_list(&k.elevation.overlay).clip_rounded(12.0).child(title_bar);
        return Some(col().key("compose-dock").pin(None, None, Some(24.0), Some(0.0)).child(drop_target(card)));
    }

    let mut fields = col().w_full();
    if s.accounts.len() > 1 {
        let emails: Vec<String> = s.accounts.iter().map(|a| a.email.clone()).collect();
        let current = s.accounts.iter().find(|a| a.id == cm.account).map(|a| a.email.clone());
        let select = Select::new("compose-from", &emails)
            .current(current.as_deref())
            .open(cm.from_open)
            .label("From")
            .on_toggle(|s: &mut App| {
                if let Some(c) = &mut s.compose {
                    c.from_open = !c.from_open;
                }
            })
            .on_close(|s: &mut App| {
                if let Some(c) = &mut s.compose {
                    c.from_open = false;
                }
            })
            .on_choose(|s: &mut App, v: &str, _: &mut Tx| {
                let id = s.accounts.iter().find(|a| a.email == v).map(|a| a.id.clone());
                if let (Some(id), Some(c)) = (id, &mut s.compose) {
                    c.account = id;
                    c.from_open = false;
                }
                s.compose_edited();
            })
            .build(k, layer);
        fields = fields.child(field_row(t, "From", select, None));
    }
    let cc_toggle = if cm.show_cc {
        None
    } else {
        Some(
            el().key("show-cc")
                .role("button")
                .label("Add Cc and Bcc")
                .px(6.0)
                .h(24.0)
                .center()
                .radius(6.0)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut App| {
                    if let Some(c) = &mut s.compose {
                        c.show_cc = true;
                    }
                })
                .child(text("Cc Bcc").font(k.ty.text_xsmall_medium).color(k.c.text_muted)),
        )
    };
    fields = fields.child(field_row(t, "To", recipients_input(cm, Recipients::To, "Recipients", k, layer), cc_toggle));
    if cm.show_cc {
        fields = fields
            .child(field_row(t, "Cc", recipients_input(cm, Recipients::Cc, "Cc", k, layer), None))
            .child(field_row(t, "Bcc", recipients_input(cm, Recipients::Bcc, "Bcc", k, layer), None));
    }
    fields = fields.child(field_row(t, "Subject", plain_input(&cm.subject, "Subject", "field-compose-subject", |s| &mut c(s).subject, k), None));
    if let Some(notice) = &cm.notice {
        fields = fields.child(
            row()
                .key("compose-notice")
                .w_full()
                .px(16.0)
                .py(8.0)
                .gap(8.0)
                .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
                .child(icon(&icons::ALERT, 14.0).color(k.c.icon_muted))
                .child(text(notice.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().grow(1.0).min_w(0.0)),
        );
    }

    let ink = k.c.text_base;
    let body = col()
        .key("compose-body-wrap")
        .w_full()
        .min_h(280.0)
        .max_h(460.0)
        .px(16.0)
        .py(12.0)
        .scroll(rune::ScrollSpec { stick_bottom: false })
        .child(
            rune::text_input(&cm.body, rune::TextStyle::new(14.0, 400.0).lh(22.0))
                .key("field-compose-body")
                .label("Message")
                .w_full()
                .min_h(256.0)
                .color(ink)
                .placeholder("Write your message", k.c.text_faint)
                .caret(ink, k.selection())
                .on_edit(|s: &mut App| &mut c(s).body)
                .on_input_change(|s: &mut App, _: &mut Tx| s.compose_edited()),
        );

    // The draft's files: they go with it when it's saved and sent.
    let files = (!cm.files.is_empty()).then(|| {
        let (sizes, text) = cm.sizes();
        files_section(t, cm, s.compose_limit().over(&sizes, text))
    });

    let send_label = key_label(&s.settings, "keys.send");
    let mut footer = row()
        .key("compose-footer")
        .w_full()
        .px(14.0)
        .py(12.0)
        .gap(10.0)
        .center_y()
        .child(
            row()
                .key("compose-send")
                .role("button")
                .label("Send")
                .focusable(true)
                .h(36.0)
                .px(18.0)
                .gap(8.0)
                .center_y()
                .radius(18.0)
                .fill(t.accent)
                .hover(rune::Color::WHITE.alpha(0.12))
                .press(rune::Color::BLACK.alpha(0.12))
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut App| s.send_compose())
                .help("Send")
                .help_keys([send_label])
                .child(text("Send").font(k.ty.ui_medium).color(rune::Color::WHITE))
                .child(icon(&icons::SEND, 15.0).color(rune::Color::WHITE)),
        )
        .child(
            IconButton::new(&icons::PAPERCLIP)
                .variant(IconButtonVariant::GhostMuted)
                .label("Attach files")
                .on_press_tx(|s: &mut App, tx: &mut Tx| s.attach_dialog(tx))
                .build(k)
                .help("Attach files")
                .help_keys([key_label(&s.settings, "keys.attach")])
                .key("compose-attach"),
        );
    if let Some(e) = cm.error.as_ref().or(cm.save_error.as_ref()) {
        footer = footer.child(text(e.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).help(e.clone()).truncate().grow(1.0).min_w(0.0).key("compose-error"));
    } else if cm.shows_saved() {
        footer = footer.child(
            text("Draft saved")
                .font(k.ty.text_xsmall)
                .color(k.c.text_faint)
                .help("Saved on this computer, and to your Drafts folder on the server")
                .truncate()
                .grow(1.0)
                .min_w(0.0)
                .key("compose-saved"),
        );
    } else {
        footer = footer.child(el().grow(1.0));
    }
    footer = footer.child(
        Button::new("Discard")
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Regular)
            .icon(&icons::TRASH)
            .on_press_tx(|s: &mut App, tx: &mut Tx| s.discard_compose(tx))
            .build(k)
            .key("compose-discard"),
    );

    let card = col()
        .key("composer")
        .w(width)
        .max_h(720.0)
        .radius(12.0)
        .fill(t.card)
        .box_shadow_list(&k.elevation.overlay)
        .clip_rounded(12.0)
        .child(title_bar)
        .child(fields)
        .child(body);
    let mut card = match files {
        Some(f) => card.child(f),
        None => card,
    }
    .child(footer);
    if cm.drop_hover {
        card = card.child(
            col()
                .key("compose-drop")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .radius(12.0)
                .fill(t.card.alpha(0.92))
                .border(2.0, t.accent)
                .center()
                .gap(8.0)
                .child(icon(&icons::PAPERCLIP, 28.0).color(t.accent))
                .child(text("Drop files to attach").font(k.ty.ui_medium).color(k.c.text_base)),
        );
    }
    Some(col().key("compose-dock").pin(None, None, Some(24.0), Some(0.0)).child(drop_target(card)))
}

/// Files dragged from another app and dropped on the composer are attached to it.
fn drop_target(card: El) -> El {
    card.on_drop_files(|s: &mut App, files: &[std::path::PathBuf], _: &mut Tx| s.attach_paths(files.to_vec())).on_drop_hover(|s: &mut App, over: bool, _: &mut Tx| {
        if let Some(c) = &mut s.compose {
            c.drop_hover = over;
        }
    })
}

/// The composer's files, as chips with a remove button each, their total size, and a warning
/// when they add up to more than a message can carry.
fn files_section(t: &Theme, cm: &Compose, over_limit: Option<String>) -> El {
    let k = &t.k;
    let mut chips = row().key("compose-files").w_full().gap(8.0).flex_wrap();
    for (i, f) in cm.files.iter().enumerate() {
        chips = chips.child(file_chip(t, f, i));
    }
    let files = |n: usize| if n == 1 { "1 file".to_owned() } else { format!("{n} files") };
    let attached: Vec<&DraftAttachment> = cm.files.iter().filter(|f| f.status == FileStatus::Attached).collect();
    let attaching = cm.files.iter().filter(|f| f.status == FileStatus::Attaching).count();
    let size = fmt::size(attached.iter().map(|f| f.size).sum());
    let summary = match (attached.len(), attaching) {
        (0, 0) => None,
        (0, n) => Some(format!("Attaching {}…", files(n))),
        (a, 0) => Some(format!("{} · {size}", files(a))),
        (a, n) => Some(format!("{} · {size} · attaching {n} more…", files(a))),
    };
    let mut section = col().key("compose-files-section").w_full().px(14.0).pt(8.0).gap(6.0).child(chips);
    // Why a file isn't attached, in words, not only on hover.
    for f in &cm.files {
        if let FileStatus::Failed(why) = &f.status {
            section = section.child(text(why.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().w_full().key(format!("compose-file-why-{}", f.id)));
        }
    }
    if let Some(summary) = summary {
        section = section.child(text(summary).font(k.ty.text_xsmall).color(k.c.text_faint).key("compose-files-total"));
    }
    if let Some(why) = over_limit {
        section = section.child(
            row()
                .key("compose-over-limit")
                .role("alert")
                .label(why.clone())
                .w_full()
                .px(10.0)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .radius(8.0)
                .fill(k.c.danger.bg)
                .child(icon(&icons::ALERT, 14.0).color(k.c.danger.icon))
                .child(text(why).font(k.ty.text_xsmall_medium).color(k.c.danger.fg).wrap().grow(1.0).min_w(0.0)),
        );
    }
    section
}

/// One file, the `i`th: its name and size (or that it's on its way, or why it isn't attached),
/// and a button to take it off. The button is keyed by place, so once a file is taken off the
/// keyboard's focus stays put, on the next file's (its label says which).
fn file_chip(t: &Theme, f: &DraftAttachment, i: usize) -> El {
    let k = &t.k;
    let id = f.id;
    let name = f.filename.clone();
    let (lead, detail, detail_color, label) = match &f.status {
        FileStatus::Attached => (icon(&icons::PAPERCLIP, 13.0).color(k.c.icon_muted), fmt::size(f.size), k.c.text_faint, format!("Attached: {name}, {}", fmt::size(f.size))),
        FileStatus::Attaching => (Spinner::new().size(12.0).build(k), "Attaching…".to_owned(), k.c.text_faint, format!("Attaching {name}")),
        FileStatus::Failed(why) => (icon(&icons::ALERT, 13.0).color(k.c.danger.icon), "Not attached".to_owned(), k.c.danger.fg, format!("Not attached: {why}")),
    };
    let mut chip = row()
        .key(format!("compose-file-{id}"))
        .h(30.0)
        .pl(10.0)
        .pr(2.0)
        .gap(6.0)
        .center_y()
        .radius(8.0)
        .fill(k.c.layer_02)
        .label(label)
        .child(lead)
        .child(text(name.clone()).font(k.ty.text_xsmall_medium).color(k.c.text_base).truncate().max_w(200.0))
        .child(text(detail).font(k.ty.text_xsmall).color(detail_color))
        .child(
            IconButton::new(&icons::CLOSE)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(format!("Remove {name}"))
                .help("Remove")
                .on_press(move |s: &mut App| s.remove_file(id))
                .build(k)
                .key(format!("compose-file-remove-{i}")),
        );
    if let FileStatus::Failed(why) = &f.status {
        chip = chip.border(1.0, k.c.danger.border).help(why.clone());
    }
    chip
}

trait ShadowList {
    fn box_shadow_list(self, shadows: &[rune::Shadow]) -> Self;
}

impl ShadowList for El {
    fn box_shadow_list(self, shadows: &[rune::Shadow]) -> Self {
        shadows.iter().fold(self, |e, s| e.box_shadow(*s))
    }
}
