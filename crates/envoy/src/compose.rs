//! The composer: a floating window at the bottom right, as in Gmail.

use rune::{Cursor, El, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Select, TextInput};

use crate::app::{App, Compose, key_label};
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
        return Some(col().key("compose-dock").pin(None, None, Some(24.0), Some(0.0)).child(card));
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
    fields = fields.child(field_row(t, "To", plain_input(&cm.to, "Recipients", "field-compose-to", |s| &mut c(s).to, k), cc_toggle));
    if cm.show_cc {
        fields = fields
            .child(field_row(t, "Cc", plain_input(&cm.cc, "Cc", "field-compose-cc", |s| &mut c(s).cc, k), None))
            .child(field_row(t, "Bcc", plain_input(&cm.bcc, "Bcc", "field-compose-bcc", |s| &mut c(s).bcc, k), None));
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
    let files = (!cm.attachments.is_empty()).then(|| {
        let mut chips = row().key("compose-files").w_full().px(14.0).pt(8.0).gap(8.0).flex_wrap();
        for a in &cm.attachments {
            chips = chips.child(
                row()
                    .key(format!("compose-file-{}", a.id))
                    .h(30.0)
                    .px(10.0)
                    .gap(6.0)
                    .center_y()
                    .radius(8.0)
                    .fill(k.c.layer_02)
                    .label(format!("Attached: {}, {}", a.filename, fmt::size(a.size)))
                    .child(icon(&icons::PAPERCLIP, 13.0).color(k.c.icon_muted))
                    .child(text(a.filename.clone()).font(k.ty.text_xsmall_medium).color(k.c.text_base).truncate().max_w(220.0))
                    .child(text(fmt::size(a.size)).font(k.ty.text_xsmall).color(k.c.text_faint)),
            );
        }
        chips
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
        );
    if let Some(e) = &cm.error {
        footer = footer.child(text(e.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).truncate().grow(1.0).min_w(0.0));
    } else if cm.saved && !cm.dirty {
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
    let card = match files {
        Some(f) => card.child(f),
        None => card,
    }
    .child(footer);
    Some(col().key("compose-dock").pin(None, None, Some(24.0), Some(0.0)).child(card))
}

trait ShadowList {
    fn box_shadow_list(self, shadows: &[rune::Shadow]) -> Self;
}

impl ShadowList for El {
    fn box_shadow_list(self, shadows: &[rune::Shadow]) -> Self {
        shadows.iter().fold(self, |e, s| e.box_shadow(*s))
    }
}
