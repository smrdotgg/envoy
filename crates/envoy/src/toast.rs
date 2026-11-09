//! Notices ("Archived", "Message sent", errors): one compact line each, Undo inline beside the
//! text, at the bottom of the list and reader, clear of the sidebar and the composer.

use rune::{Color, El, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant};

use crate::app::{App, Screen, Toast};
use crate::icons;
use crate::theme::Theme;

/// The composer's width and its distance from the window's right edge (compose.rs), open and
/// minimized.
const COMPOSER: f32 = 540.0;
const COMPOSER_MINIMIZED: f32 = 320.0;
const COMPOSER_EDGE: f32 = 24.0;
/// Room left between a notice and the composer, and the window's bottom edge.
const GAP: f32 = 16.0;
const BOTTOM: f32 = 20.0;

/// Where notices are centred: `(left, right)` insets from the window's edges. Past the sidebar
/// (its mailboxes, Settings and the sync line stay in reach), and left of the composer when
/// one is showing.
pub fn insets(s: &App) -> (f32, f32) {
    let framed = if s.settings.bool("appearance.windowFrame") { 8.0 } else { 0.0 };
    let left = s.sidebar.shown_width() + framed;
    let right = match &s.compose {
        Some(c) if s.screen == Screen::Mail => COMPOSER_EDGE + if c.minimized { COMPOSER_MINIMIZED } else { COMPOSER } + GAP,
        _ => 0.0,
    };
    (left, right)
}

fn notice(t: &Theme, toast: &Toast) -> El {
    let k = &t.k;
    let id = toast.id;
    let mut line = row()
        .key(format!("toast-{id}"))
        .role("status")
        .label(toast.text.clone())
        .h(40.0)
        .pl(14.0)
        .pr(6.0)
        .gap(6.0)
        .center_y()
        .max_w(560.0)
        .radius(10.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_muted);
    line = k.elevation.floating.iter().fold(line, |e, sh| e.box_shadow(*sh));
    if toast.error {
        line = line.child(icon(&icons::ALERT, 16.0).color(k.c.danger.fg).mr(2.0));
    }
    line = line.child(text(toast.text.clone()).font(k.ty.ui).color(k.c.text_base).truncate().shrink().min_w(0.0).pr(6.0).a11y_hidden());
    if toast.undo.is_some() {
        line = line.child(
            Button::new("Undo")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label_with({
                    let accent = t.accent;
                    move |style, _| text("Undo").font(rune::TextStyle { weight: 600.0, ..style }).color(accent)
                })
                .on_press_tx(move |s: &mut App, tx: &mut Tx| s.undo_toast(id, tx))
                .build(k)
                .key(format!("toast-undo-{id}")),
        );
    }
    if let Some(path) = toast.open_path.clone() {
        line = line.child(
            Button::new("Show")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |_: &mut App| {
                    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
                    let _ = open::that_detached(dir);
                })
                .build(k)
                .key(format!("toast-show-{id}")),
        );
    }
    line.child(
        IconButton::new(&icons::CLOSE)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label("Dismiss")
            .on_press(move |s: &mut App| s.toasts.retain(|t| t.id != id))
            .build(k)
            .key(format!("toast-close-{id}")),
    )
    .transition(rune::Transition::move_edge(rune::Edge::Bottom))
    .motion(rune::Spring::new(0.28, 0.0))
}

/// The notices, newest at the bottom, for the window's overlay layer.
pub fn region(s: &App, t: &Theme) -> Option<El> {
    if s.toasts.is_empty() {
        return None;
    }
    let (left, right) = insets(s);
    let stack = col().key("toast-region").role("region").label("Notifications").gap(8.0).items_center().children(s.toasts.iter().map(|toast| notice(t, toast)));
    // Spans the area between the insets, centring the stack; only the notices take input.
    Some(row().key("toast-dock").pin(Some(left), None, Some(right), Some(BOTTOM)).justify_center().fill(Color::TRANSPARENT).child(stack))
}
