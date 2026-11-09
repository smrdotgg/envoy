//! The reading pane: an opened conversation, its messages as cards, and the reply bar.

use rune::{Color, Cursor, Cx, El, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Markdown, MarkdownStyle, Spinner};

use envoy_core::engine::{Command, MoveTarget};
use envoy_core::html::Verdict;
use envoy_core::model::{MessageId, Role, ThreadMessage, View};

use crate::app::{App, Picker};
use crate::theme::{Theme, avatar_color};
use crate::{fmt, icons};

fn placeholder(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let unread: usize = s.list.iter().filter(|m| m.thread_unread).count();
    let line = if unread > 0 { format!("{unread} unread") } else { "Nothing selected".to_owned() };
    col()
        .key("reader-empty")
        .w_full()
        .h_full()
        .center()
        .gap(10.0)
        .child(icon(&icons::MAIL, 44.0).color(k.c.icon_faint))
        .child(text(line).font(k.ty.body_medium).color(k.c.text_muted))
        .child(text("Pick a conversation, or press J and K to move through the list.").font(k.ty.text_small).color(k.c.text_faint))
}

fn tool(t: &Theme, ic: &'static rune::Icon, label: &str, key: &str, f: impl Fn(&mut App, &mut Tx) + 'static) -> El {
    IconButton::new(ic)
        .variant(IconButtonVariant::GhostMuted)
        .label(label)
        .help(label)
        .on_press_tx(f)
        .build(&t.k)
        .key(key.to_owned())
}

fn header(s: &App, t: &Theme, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let Some(first) = s.thread.first() else { return el() };
    let last = s.thread.last().map(|m| &m.summary).unwrap_or(&first.summary);
    let subject = if first.summary.subject.is_empty() { "(no subject)".to_owned() } else { fmt::base_subject(&last.subject).to_owned() };
    let open = s.open;
    let starred = s.thread.iter().any(|m| m.summary.flagged());
    let last_id = last.id;
    let in_trash = s.mailboxes.get(&last.account).and_then(|m| m.iter().find(|m| m.name == last.mailbox)).is_some_and(|m| m.role == Role::Trash);
    let in_junk = s.mailboxes.get(&last.account).and_then(|m| m.iter().find(|m| m.name == last.mailbox)).is_some_and(|m| m.role == Role::Junk);
    let mailbox_label = s.mailboxes.get(&last.account).and_then(|m| m.iter().find(|m| m.name == last.mailbox)).map(|m| m.display.clone()).unwrap_or_default();

    let mut tools = row().key("reader-tools").gap(2.0).center_y();
    if s.settings.str("inbox.readingPane") == "off" {
        tools = tools.child(tool(t, &icons::BACK, "Back to list", "t-back", |s, _| s.close_message()));
    }
    tools = tools
        .child(tool(t, &icons::ARCHIVE, "Archive", "t-archive", move |s, tx| {
            if let Some(id) = open {
                s.archive(vec![id], tx)
            }
        }))
        .child(tool(t, &icons::TRASH, if in_trash { "Delete forever" } else { "Delete" }, "t-delete", move |s, tx| {
            if let Some(id) = open {
                s.delete(vec![id], tx)
            }
        }))
        .child(tool(t, &icons::SPAM, if in_junk { "Not spam" } else { "Report spam" }, "t-spam", move |s, tx| {
            if let Some(id) = open {
                s.move_to(vec![id], if in_junk { MoveTarget::Inbox } else { MoveTarget::Junk }, tx)
            }
        }));
    let targets: Vec<MessageId> = open.into_iter().collect();
    tools = tools.child(crate::triage::picker(s, t, Picker::Move, &targets, layer));
    if s.all_gmail(&targets) {
        tools = tools.child(crate::triage::picker(s, t, Picker::Label, &targets, layer));
    }
    tools = tools
        .child(el().w(1.0).h(18.0).mx(6.0).fill(k.c.border_muted))
        .child(tool(t, &icons::MAIL_DOT, "Mark as unread", "t-unread", move |s, _| {
            if let Some(id) = open {
                s.toggle_read(id)
            }
        }))
        .child(
            IconButton::new(if starred { &icons::STAR_FILLED } else { &icons::STAR })
                .variant(IconButtonVariant::GhostMuted)
                .label(if starred { "Unstar" } else { "Star" })
                .help(if starred { "Unstar" } else { "Star" })
                .on_press(move |s: &mut App| s.toggle_star(last_id))
                .build(k)
                .key("t-star"),
        )
        .child(tool(t, &icons::EXTERNAL, "Open original (.eml) in your mail viewer", "t-original", move |s, _| s.send(Command::OpenOriginal { id: last_id })));

    let mut title_row = row().gap(10.0).items_start().w_full().child(
        text(subject).font(rune::TextStyle::new(21.0, 600.0).lh(28.0)).color(k.c.text_base).wrap().selectable(true).grow(1.0).min_w(0.0),
    );
    if !mailbox_label.is_empty() {
        title_row = title_row.child(
            text(mailbox_label).font(k.ty.text_xsmall_medium).color(k.c.text_muted).px(8.0).py(2.0).radius(6.0).fill(k.c.layer_02).mt(4.0),
        );
    }
    col().key("reader-header").w_full().gap(10.0).px(24.0).pt(14.0).pb(6.0).child(tools).child(title_row)
}

fn avatar(name: &str, email: &str, t: &Theme) -> El {
    el().size(38.0, 38.0)
        .radius(19.0)
        .center()
        
        .fill(avatar_color(email, t.dark))
        .child(text(fmt::initial(name)).font(rune::TextStyle::new(16.0, 600.0)).color(Color::WHITE))
}

/// What a message body's width is short of the cards column's: the column's padding, the
/// card's, and the avatar's indent (`reader`, `message_card`). Borders take no room.
const BODY_INSET: f32 = 2.0 * 20.0 + 2.0 * 20.0 + 50.0;

/// The steps, in points, a message's width is read in: dragging the divider re-fits it (its
/// elements rebuild, never its tree) when a step is crossed, not on every frame.
const WIDTH_STEP: f32 = 8.0;

/// The width the reader gives message bodies, read from the cards column in steps of
/// [`WIDTH_STEP`] (rounded down, so what's fitted to it fits); None before it's laid out. The
/// view re-runs only when a step is crossed. The cards column is there before a message
/// opens from another, so opening one lays it out once, at the right width.
fn body_width() -> Option<f32> {
    rune::memo::layout_with("cards", |r| ((r[2] - BODY_INSET) / WIDTH_STEP).floor() as i32).map(|n| (n as f32 * WIDTH_STEP).max(0.0))
}

fn body_view(s: &App, t: &Theme, m: &ThreadMessage) -> El {
    let k = &t.k;
    let id = m.summary.id;
    let max_w = s.settings.f32("reading.maxWidth");
    let Some(b) = &m.body else {
        return row()
            .key(format!("loading-{id}"))
            .gap(8.0)
            .py(12.0)
            .center_y()
            .child(Spinner::new().build(k))
            .child(text("Downloading message…").font(k.ty.text_small).color(k.c.text_faint));
    };
    let prefer_plain = s.settings.bool("reading.preferPlainText");
    let show_quoted = s.settings.bool("reading.showQuoted") || s.show_quoted.contains(&id);
    let copy_links = s.settings.bool("behavior.copyLinks");
    let doc = b.doc.as_ref().filter(|d| !d.nodes.is_empty() && (!prefer_plain || b.text.as_ref().is_none_or(|t| t.trim().is_empty())));
    if let Some(doc) = doc {
        let remote = s.remote_images(m);
        let cx = crate::html_view::Ctx {
            t,
            images: &s.images,
            adapt: s.settings.str("reading.htmlColors") != "original",
            load_images: remote,
            copy_links: s.settings.bool("behavior.copyLinks"),
            max_w,
            width: body_width().unwrap_or(max_w + 32.0),
        };
        let ready = doc.images.iter().filter(|u| matches!(s.images.get(*u), Some(crate::app::ImageState::Ready(_)))).count();
        let version = (t.dark, cx.adapt, cx.load_images, cx.copy_links, max_w as u32, cx.width as u32, ready, std::sync::Arc::as_ptr(doc) as usize);
        let rendered = rune::memo(format!("html-{id}"), version, || crate::html_view::render(&cx, doc));
        // It may shrink below what's in it: what's in it is fitted to the width it gets.
        let mut out = col().key(format!("body-{id}")).w_full().shrink().max_w(max_w + 32.0).gap(8.0);
        let has_remote = doc.images.iter().any(|u| u.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("http")));
        if has_remote && !remote && s.settings.str("reading.loadImages") == "ask" {
            out = out.child(images_bar(t, id, &m.summary.from.email));
        }
        if doc.fidelity.verdict() == Verdict::Poor && !s.layout_note_dismissed.contains(&id) {
            out = out.child(layout_note(t, id, remote));
        }
        out = out.child(rendered);
        out = attachments(out, t, b, id);
        return out;
    }
    let use_md = b.markdown.as_ref().is_some_and(|md| !md.trim().is_empty()) && (!prefer_plain || b.text.as_ref().is_none_or(|t| t.trim().is_empty()));
    let mut out = col().key(format!("body-{id}")).w_full().max_w(max_w).gap(8.0);
    if use_md {
        let md = b.markdown.clone().unwrap_or_default();
        let style = MarkdownStyle::from_kit(k).body_font(rune::TextStyle::new(14.0, 400.0).lh(22.0)).link(t.accent);
        out = out.child(
            Markdown::new(md)
                .selectable(true)
                .on_link_tx(move |_s: &mut App, url: &str, tx: &mut Tx| {
                    if copy_links {
                        tx.copy(url.to_owned());
                    } else {
                        let _ = tx.open_url(url);
                    }
                })
                .render(&style),
        );
    } else {
        let raw = b.text.clone().unwrap_or_default();
        let (shown, had_quote) = if show_quoted { (raw.clone(), false) } else { fmt::strip_quoted(&raw) };
        let shown = if shown.trim().is_empty() { raw.clone() } else { shown };
        out = out.child(text(shown.trim_end().to_owned()).font(rune::TextStyle::new(14.0, 400.0).lh(22.0)).color(k.c.text_base).wrap().selectable(true).w_full());
        if had_quote {
            out = out.child(
                el().key(format!("quoted-{id}"))
                    .role("button")
                    .label("Show quoted text")
                    .px(10.0)
                    .h(20.0)
                    .center()
                    .radius(10.0)
                    .fill(k.c.layer_02)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut App| {
                        s.show_quoted.insert(id);
                    })
                    .help("Show quoted text")
                    .child(text("•••").font(k.ty.text_xsmall_medium).color(k.c.text_muted)),
            );
        }
    }
    attachments(out, t, b, id)
}

/// A quiet line over mail whose remote images are hidden (`reading.loadImages` "ask" and a
/// sender not in `reading.imagesFrom`): load them for this message, or always from the sender.
/// On one line while it fits; in a narrower reader the actions wrap onto a second line, and a
/// long address ends in "…" (the button still says it whole to assistive technology).
fn images_bar(t: &Theme, id: i64, sender: &str) -> El {
    let k = &t.k;
    let label = row()
        .gap(8.0)
        .h(20.0)
        .center_y()
        .grow(1.0)
        .min_w(0.0)
        .child(icon(&icons::IMAGE, 14.0).color(k.c.icon_faint))
        .child(text("Images hidden").font(k.ty.text_small).color(k.c.text_muted).truncate());
    let mut actions = row().gap(2.0).center_y().shrink().child(
        Button::new("Load images")
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut App| s.load_images(id))
            .build(k)
            .key(format!("images-load-{id}")),
    );
    if !sender.trim().is_empty() {
        let sender = sender.trim().to_lowercase();
        let label = format!("Always load from {sender}");
        let shown = label.clone();
        actions = actions.child(
            Button::new(label)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label_with(move |font, ink| text(shown).font(font).color(ink).truncate())
                .on_press(move |s: &mut App| s.allow_images_from(&sender))
                .build(k)
                .shrink()
                .key(format!("images-always-{id}")),
        );
    }
    row()
        .key(format!("images-bar-{id}"))
        .w_full()
        .flex_wrap()
        .gaps(0.0, 8.0)
        .pl(10.0)
        .pr(4.0)
        .py(2.0)
        .center_y()
        .radius(8.0)
        .fill(k.c.layer_01)
        .child(label)
        .child(actions)
}

/// A quiet line over mail the native renderer can't lay out as designed (positioned layers,
/// CSS grid, drawings, embedded media: `html::Verdict::Poor`), offering it in the browser,
/// sanitized: nothing in it runs, and remote pictures load only if they would here
/// (`remote`). Opening it is the user's choice. Dismissed per message.
fn layout_note(t: &Theme, id: i64, remote: bool) -> El {
    let k = &t.k;
    row()
        .key(format!("layout-note-{id}"))
        .w_full()
        .gap(8.0)
        .pl(10.0)
        .pr(4.0)
        .py(2.0)
        .center_y()
        .radius(8.0)
        .fill(k.c.layer_01)
        .child(icon(&icons::LAYERS, 14.0).color(k.c.icon_faint))
        .child(text("Some of this message's layout can't be shown here.").font(k.ty.text_small).color(k.c.text_muted).wrap().grow(1.0).min_w(0.0))
        .child(
            Button::new("Open in browser")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut App| s.send(Command::OpenInBrowser { id, remote_images: remote }))
                .build(k)
                .key(format!("layout-note-open-{id}")),
        )
        .child(
            IconButton::new(&icons::CLOSE)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label("Dismiss")
                .help("Dismiss")
                .on_press(move |s: &mut App| {
                    s.layout_note_dismissed.insert(id);
                })
                .build(k)
                .key(format!("layout-note-close-{id}")),
        )
}

fn attachments(mut out: El, t: &Theme, b: &envoy_core::model::Body, id: i64) -> El {
    let k = &t.k;
    if !b.attachments.is_empty() {
        let mut chips = row().key(format!("atts-{id}")).gap(8.0).flex_wrap().pt(8.0);
        for a in &b.attachments {
            let index = a.index;
            chips = chips.child(
                row()
                    .key(format!("att-{id}-{index}"))
                    .role("button")
                    .label(format!("Save {}", a.filename))
                    .gap(10.0)
                    .px(12.0)
                    .py(8.0)
                    .center_y()
                    .radius(10.0)
                    .border(1.0, k.c.border_base)
                    .hover(k.c.hover)
                    .press(k.c.pressed)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut App| s.send(Command::SaveAttachment { id, index }))
                    .help("Save to Downloads and open")
                    .child(icon(&icons::PAPERCLIP, 16.0).color(k.c.icon_muted))
                    .child(
                        col()
                            .gap(0.0)
                            .child(text(a.filename.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().max_w(240.0))
                            .child(text(fmt::size(a.size)).font(k.ty.text_xsmall).color(k.c.text_faint)),
                    )
                    .child(icon(&icons::DOWNLOAD, 14.0).color(k.c.icon_faint)),
            );
        }
        out = out.child(chips);
    }
    out
}

fn message_card(s: &App, t: &Theme, m: &ThreadMessage, is_last: bool) -> El {
    let k = &t.k;
    let sm = &m.summary;
    let id = sm.id;
    let expanded = s.expanded.contains(&id) || s.thread.len() == 1;
    let date = fmt::full_date(sm.date);
    let to_line = {
        let mut names: Vec<String> = sm.to.iter().map(|a| a.display().to_owned()).collect();
        if let Some(b) = &m.body {
            names.extend(b.cc.iter().map(|a| a.display().to_owned()));
        }
        if names.len() > 4 {
            let more = names.len() - 3;
            names.truncate(3);
            names.push(format!("{more} more"));
        }
        if names.is_empty() { String::new() } else { format!("to {}", names.join(", ")) }
    };
    let toggle = move |s: &mut App| {
        if !s.expanded.remove(&id) {
            s.expanded.insert(id);
        }
    };
    let head = row()
        .key(format!("head-{id}"))
        .w_full()
        .gap(12.0)
        .items_start()
        .cursor(Cursor::Pointer)
        .on_press(toggle)
        .child(avatar(sm.from.display(), &sm.from.email, t))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .w_full()
                        .child(text(sm.from.display().to_owned()).font(rune::TextStyle { weight: 600.0, ..k.ty.text_small }).color(k.c.text_base).truncate().min_w(0.0))
                        .child(if expanded && !sm.from.name.is_empty() {
                            text(format!("<{}>", sm.from.email)).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().min_w(0.0).shrink()
                        } else {
                            el()
                        })
                        .child(el().grow(1.0))
                        .child(text(date).font(k.ty.text_xsmall).color(k.c.text_faint)),
                )
                .child(if expanded {
                    text(to_line).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().w_full()
                } else {
                    text(if sm.preview.is_empty() { " ".to_owned() } else { sm.preview.clone() }).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().w_full()
                }),
        );
    let mut card = col()
        .key(format!("msg-{id}"))
        .w_full()
        .gap(14.0)
        .px(20.0)
        .py(if expanded { 18.0 } else { 12.0 })
        .radius(14.0)
        .fill(t.card)
        .border(1.0, k.c.border_muted)
        .child(head);
    if expanded {
        card = card.child(row().w_full().pl(50.0).child(body_view(s, t, m)));
        let reply_btn = |label: &str, ic: &'static rune::Icon, key: &str, f: fn(&mut App, i64)| {
            Button::new(label)
                .icon(ic)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press(move |s: &mut App| f(s, id))
                .build(k)
                .key(format!("{key}-{id}"))
        };
        if is_last {
            card = card.child(
                row()
                    .key(format!("replybar-{id}"))
                    .pl(50.0)
                    .pt(4.0)
                    .gap(8.0)
                    .child(reply_btn("Reply", &icons::REPLY, "reply", |s, id| s.compose_reply(Some(id), false, false)))
                    .child(reply_btn("Reply all", &icons::REPLY_ALL, "replyall", |s, id| s.compose_reply(Some(id), true, false)))
                    .child(reply_btn("Forward", &icons::FORWARD, "forward", |s, id| s.compose_reply(Some(id), false, true))),
            );
        }
    } else {
        card = card.hover(k.c.hover);
    }
    card
}

pub fn reader(s: &App, t: &Theme, _cx: &mut Cx<App>, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    if !s.selection.is_empty() {
        return crate::triage::selection_panel(s, t, layer);
    }
    if s.open.is_none() {
        return placeholder(s, t);
    }
    if s.thread.is_empty() {
        return col().key("reader-loading").w_full().h_full().center().child(Spinner::new().build(k));
    }
    let n = s.thread.len();
    let mut cards = col().key("cards").w_full().gap(10.0).px(20.0).pb(28.0).pt(4.0);
    // Long conversations fold their middle, as Gmail does.
    let fold = n > 4 && !s.expanded.contains(&-1);
    for (i, m) in s.thread.iter().enumerate() {
        if fold && i > 0 && i < n - 2 && !s.expanded.contains(&m.summary.id) {
            if i == 1 {
                let hidden = n - 3;
                cards = cards.child(
                    row()
                        .key("fold")
                        .w_full()
                        .h(30.0)
                        .center()
                        .radius(10.0)
                        .border(1.0, k.c.border_muted)
                        .hover(k.c.hover)
                        .cursor(Cursor::Pointer)
                        .on_press(|s: &mut App| {
                            s.expanded.insert(-1);
                        })
                        .child(text(format!("{hidden} earlier messages")).font(k.ty.text_xsmall_medium).color(k.c.text_muted)),
                );
            }
            continue;
        }
        cards = cards.child(message_card(s, t, m, i + 1 == n));
    }
    let is_sent_view = matches!(&s.view, View::Role { role: Role::Sent, .. });
    let _ = is_sent_view;
    col().key("reader").w_full().h_full().min_h(0.0).child(header(s, t, layer)).child(
        col().key("reader-scroll").scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0).grow(1.0).min_h(0.0).w_full().child(cards),
    )
}

#[cfg(test)]
mod tests {
    use rune::{Cx, El, Env, Runtime, col};

    use crate::app::App;

    fn bar_at(width: f32) -> Vec<rune::ElementInfo> {
        fn view(w: &f32, _: &mut Cx<f32>) -> El {
            let t = crate::theme::build(&envoy_core::settings::Settings::default(), false);
            col().key("pane").w(*w).child(super::images_bar(&t, 7, "Maya.Chen-Reviews@northwind-design.example"))
        }
        let mut rt = Runtime::new(width, view, (width, 200.0), Env::new(false, 1.0));
        rt.frame(0.0);
        rt.elements()
    }

    #[test]
    fn the_images_bar_wraps_instead_of_overflowing() {
        for width in [640.0, 360.0, 240.0] {
            let els = bar_at(width);
            let find = |s: &str| els.iter().find(|e| e.text.as_deref().is_some_and(|t| t.starts_with(s))).unwrap_or_else(|| panic!("{s:?} at {width}")).rect;
            for e in &els {
                assert!(e.rect[0] >= -0.5 && e.rect[0] + e.rect[2] <= width + 0.5, "at {width} pt {:?} spans {:?}", e.text.as_deref().unwrap_or(&e.path), e.rect);
            }
            let (hidden, load, always) = (find("Images hidden"), find("Load images"), find("Always load from"));
            let overlap = |a: [f32; 4], b: [f32; 4]| a[0] < b[0] + b[2] - 0.5 && b[0] < a[0] + a[2] - 0.5 && a[1] < b[1] + b[3] - 0.5 && b[1] < a[1] + a[3] - 0.5;
            assert!(!overlap(hidden, load) && !overlap(hidden, always) && !overlap(load, always), "at {width} pt the bar's parts overlap: {hidden:?} {load:?} {always:?}");
            let middle = |r: [f32; 4]| r[1] + r[3] / 2.0;
            if width >= 640.0 {
                assert!((middle(load) - middle(hidden)).abs() < 2.0, "one line at {width} pt: {hidden:?} {load:?}");
            } else {
                assert!(load[1] >= hidden[1] + hidden[3], "the actions move to a second line at {width} pt: {hidden:?} {load:?}");
            }
        }
    }

    #[test]
    fn message_bodies_are_fitted_to_the_reader_they_are_in() {
        // The width the reader hands the renderer is the body's own, as laid out, so mail that
        // asks for 760 points of minimum width in a narrow reader still ends at its edge.
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../envoy-core/tests/fixtures/html/foundation-hero.html")).unwrap();
        for window in [1120.0, 1280.0, 1700.0] {
            let mut app = App::new(true);
            crate::demo::populate(&mut app);
            crate::scene_open(&mut app);
            let m = app.thread.last_mut().unwrap();
            m.body.as_mut().unwrap().doc = Some(std::sync::Arc::new(envoy_core::html::parse(&src)));
            let id = m.summary.id;
            let mut rt = Runtime::new(app, crate::view, (window, 1000.0), Env::new(false, 1.0));
            rt.frame(0.0);
            rt.frame(0.0);
            let els = rt.elements();
            let rect = |key: &str| els.iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} at {window}")).rect;
            let (cards, body) = (rect("cards"), rect(&format!("body-{id}")));
            let max_w = crate::app::App::new(true).settings.f32("reading.maxWidth");
            assert!((cards[2] - super::BODY_INSET).min(max_w + 32.0) - body[2] < 0.5 && body[2] <= cards[2] - super::BODY_INSET + 0.5, "at {window}: cards {cards:?}, body {body:?}");
            let right = body[0] + body[2];
            let html = format!("html-{id}");
            for e in els.iter().filter(|e| e.path.split('/').any(|p| p == html)) {
                assert!(e.rect[0] + e.rect[2] <= right + 0.5, "at {window}: {:?} spans {:?}, past the body's edge {right}", e.text.as_deref().unwrap_or(&e.path), e.rect);
            }
        }
    }
}
