//! The mail screen: sidebar, toolbar and message list, laid out around the reader.

use std::borrow::Cow;

use rune::{Color, Cursor, Cx, El, GrowthSpec, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::shell::{PaneMotion, Sidebar, SidebarRow, SidebarSection, SidebarToggle, SplitView, Toolbar};
use rune_kit::{ControlSize, IconButton, IconButtonVariant, Spinner, TextInput};

use envoy_core::engine::Command;
use envoy_core::model::{Mailbox, Role, Summary, SyncState, View};

use crate::app::{App, Screen, row_key, unread_count};
use crate::theme::{Theme, avatar_color};
use crate::{fmt, icons, reader, view_settings};

pub fn role_icon(role: Role) -> &'static rune::Icon {
    match role {
        Role::Inbox => &icons::INBOX,
        Role::Flagged => &icons::STAR,
        Role::Drafts => &icons::DRAFT,
        Role::Sent => &icons::SEND,
        Role::Archive => &icons::ARCHIVE,
        Role::All => &icons::MAIL,
        Role::Junk => &icons::SPAM,
        Role::Trash => &icons::TRASH,
        Role::Important => &icons::IMPORTANT,
        Role::Other => &icons::LABEL,
    }
}

fn count_label(n: u32) -> String {
    if n == 0 { String::new() } else if n > 9999 { "9999+".into() } else { n.to_string() }
}

pub fn compose_button(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let fg = if t.dark { k.c.text_base } else { oklch_ink(t) };
    row()
        .key("compose-button")
        .role("button")
        .label("Compose")
        .focusable(true)
        .h(44.0)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .radius(14.0)
        .fill(t.selected)
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(|s: &mut App| s.compose_new())
        .child(icon(&icons::PENCIL, 18.0).color(fg))
        .child(text("Compose").font(k.ty.ui_medium).color(fg))
        .help("Compose")
        .help_keys([crate::app::key_label(&s.settings, "keys.compose")])
}

fn oklch_ink(t: &Theme) -> Color {
    let _ = t;
    rune::oklch(0.3, 0.06, 258.0)
}

fn mailbox_row(s: &App, t: &Theme, m: &Mailbox, label: Option<String>) -> El {
    let view = View::Mailbox { account: m.account.clone(), mailbox: m.name.clone() };
    let selected = s.screen == Screen::Mail && view_matches(&s.view, m);
    let count = if matches!(m.role, Role::Sent | Role::Drafts | Role::Trash | Role::All | Role::Archive) { 0 } else { m.unread };
    let name = label.unwrap_or_else(|| m.display.clone());
    let mut r = SidebarRow::new(name).icon(role_icon(m.role)).height(32.0).selected(selected).depth(if m.role == Role::Other { m.depth } else { 0 });
    if count > 0 {
        r = r.detail(count_label(count));
    }
    let v = view.clone();
    r.on_select(move |s: &mut App, _| s.go(v.clone())).build(&t.k).key(format!("mb:{}:{}", m.account, m.name))
}

fn view_matches(view: &View, m: &Mailbox) -> bool {
    match view {
        View::Mailbox { account, mailbox } => *account == m.account && *mailbox == m.name,
        View::Role { account: Some(a), role } => *a == m.account && *role == m.role,
        _ => false,
    }
}

fn sync_line(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let mut busy = None;
    let mut problem = None;
    for (acct, st) in &s.sync {
        let email = s.accounts.iter().find(|a| &a.id == acct).map(|a| a.email.clone()).unwrap_or_default();
        match st {
            SyncState::Connecting => busy = busy.or(Some("Connecting…".to_owned())),
            SyncState::Syncing(what) => busy = Some(format!("{what}…")),
            SyncState::Offline(e) => problem = Some((format!("Offline — {email}"), e.clone())),
            SyncState::Error(e) => problem = Some((format!("Sign-in problem — {email}"), e.clone())),
            SyncState::Idle => {}
        }
    }
    let (ic, label, help, color): (El, String, String, Color) = if let Some((l, e)) = problem {
        (icon(&icons::OFFLINE, 14.0).color(k.c.danger.fg), l, e, k.c.danger.fg)
    } else if let Some(b) = busy {
        (Spinner::new().build(k), b, String::new(), k.c.text_muted)
    } else if s.demo {
        (icon(&icons::BOLT, 14.0).color(k.c.text_faint), "Demo mode".into(), "Sample data; nothing is sent.".into(), k.c.text_faint)
    } else if s.accounts.is_empty() {
        (icon(&icons::USER, 14.0).color(k.c.text_faint), "No accounts".into(), String::new(), k.c.text_faint)
    } else {
        (icon(&icons::CHECK, 14.0).color(k.c.text_faint), "Up to date".into(), String::new(), k.c.text_faint)
    };
    let mut line = row()
        .key("sync-status")
        .gap(8.0)
        .px(10.0)
        .h(28.0)
        .center_y()
        .child(ic)
        .child(text(label).font(k.ty.text_xsmall).color(color).truncate().grow(1.0).min_w(0.0));
    if s.pending_ops > 0 {
        line = line.child(
            text(format!("{} waiting", s.pending_ops))
                .font(k.ty.text_xsmall)
                .color(k.c.text_faint)
                .help("Changes saved on this computer, waiting to reach the server"),
        );
    }
    if !help.is_empty() {
        line = line.help(help);
    }
    line
}

pub fn sidebar(s: &App, t: &Theme) -> El {
    if s.screen == Screen::Settings {
        return view_settings::sidebar(s, t);
    }
    let k = &t.k;
    let mut sb = Sidebar::new().label("Mailboxes").inset(0.0).header_gap(2.0).header_padding(4.0, 10.0).row_gap(1.0);
    if !s.accounts.is_empty() {
        sb = sb.header(col().pb(10.0).child(compose_button(s, t)));
    }
    let multi = s.accounts.len() > 1;
    if multi && s.settings.bool("inbox.unifiedInbox") {
        let unified = View::Role { account: None, role: Role::Inbox };
        let n = unread_count(s, None, Role::Inbox);
        let mut r = SidebarRow::new("All inboxes").icon(&icons::LAYERS).height(32.0).selected(s.screen == Screen::Mail && s.view == unified);
        if n > 0 {
            r = r.detail(count_label(n));
        }
        sb = sb.header(r.on_select(move |s: &mut App, _| s.go(unified.clone())).build(k).key("unified-inbox"));
        let starred = View::Starred { account: None };
        sb = sb.header(
            SidebarRow::new("All starred")
                .icon(&icons::STAR)
                .height(32.0)
                .selected(s.screen == Screen::Mail && s.view == starred)
                .on_select(move |s: &mut App, _| s.go(starred.clone()))
                .build(k)
                .key("unified-starred"),
        );
    }
    for a in &s.accounts {
        let boxes = s.mailboxes.get(&a.id).cloned().unwrap_or_default();
        let mut rows: Vec<El> = Vec::new();
        let roles: Vec<&Mailbox> = boxes.iter().filter(|m| m.role != Role::Other && m.selectable).collect();
        let labels: Vec<&Mailbox> = boxes.iter().filter(|m| m.role == Role::Other).collect();
        if roles.is_empty() {
            rows.push(
                row()
                    .px(10.0)
                    .h(32.0)
                    .gap(8.0)
                    .center_y()
                    .child(Spinner::new().build(k))
                    .child(text("Loading folders…").font(k.ty.text_small).color(k.c.text_faint))
                    .key(format!("loading:{}", a.id)),
            );
        }
        for m in roles {
            rows.push(mailbox_row(s, t, m, None));
        }
        let labels_key = format!("labels:{}", a.id);
        let labels_collapsed = !s.collapsed_sections.contains(&format!("open:{labels_key}"));
        if !labels.is_empty() {
            let lk = labels_key.clone();
            rows.push(
                row()
                    .key(format!("labels-toggle:{}", a.id))
                    .h(28.0)
                    .px(10.0)
                    .mt(6.0)
                    .gap(6.0)
                    .center_y()
                    .radius(6.0)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut App| {
                        let key = format!("open:{lk}");
                        if !s.collapsed_sections.remove(&key) {
                            s.collapsed_sections.insert(key);
                        }
                    })
                    .child(icon(if labels_collapsed { k.glyph.triangle_right } else { k.glyph.triangle_down }, 12.0).color(k.c.icon_faint))
                    .child(text(format!("Labels ({})", labels.len())).font(k.ty.text_xsmall_medium).color(k.c.text_faint)),
            );
            if !labels_collapsed {
                for m in labels {
                    if m.selectable {
                        rows.push(mailbox_row(s, t, m, None));
                    } else {
                        rows.push(row().px(10.0).h(26.0).center_y().child(text(m.display.clone()).font(k.ty.text_xsmall).color(k.c.text_faint)).key(format!("nosel:{}:{}", a.id, m.name)));
                    }
                }
            }
        }
        if multi {
            let collapsed = s.collapsed_sections.contains(&a.id);
            let id = a.id.clone();
            sb = sb.section(
                SidebarSection::new(a.email.clone())
                    .key(format!("acct:{}", a.id))
                    .collapsed(collapsed)
                    .on_toggle(move |s: &mut App, _| {
                        if !s.collapsed_sections.remove(&id) {
                            s.collapsed_sections.insert(id.clone());
                        }
                    })
                    .rows(rows),
            );
        } else {
            for r in rows {
                sb = sb.child(r);
            }
        }
    }
    let settings_row = SidebarRow::new("Settings")
        .icon(&icons::SETTINGS)
        .height(32.0)
        .on_select(|s: &mut App, _| s.screen = Screen::Settings)
        .build(k)
        .key("settings-row");
    sb = sb.footer(col().gap(2.0).child(sync_line(s, t)).child(settings_row));
    sb.build(k)
}

fn search_field(s: &App, t: &Theme) -> El {
    let k = &t.k;
    TextInput::new(&s.search)
        .placeholder("Search mail")
        .leading_icon(&icons::SEARCH)
        .clear_button()
        .label("Search mail")
        .width(Some(560.0))
        .on_edit(|s: &mut App| &mut s.search)
        .on_change(|s: &mut App, _tx: &mut Tx| {
            let q = s.search.text.trim().to_owned();
            if q != s.applied_query {
                s.applied_query = q;
                s.request_list();
            }
        })
        .on_submit(|s: &mut App, _tx: &mut Tx| {
            s.search.focused = false;
        })
        .build(k)
        .key("search-field")
        .help("Search words, or filter with from:  to:  subject:  is:unread  is:starred  has:attachment")
}

fn toolbar(s: &App, t: &Theme) -> Toolbar {
    let k = &t.k;
    let refresh = IconButton::new(&icons::REFRESH)
        .variant(IconButtonVariant::GhostMuted)
        .label("Check mail")
        .help("Check for new mail")
        .on_press(|s: &mut App| {
            s.send(Command::SyncNow);
        })
        .build(k)
        .key("refresh");
    let settings = IconButton::new(&icons::SETTINGS)
        .variant(IconButtonVariant::GhostMuted)
        .label("Settings")
        .help("Settings")
        .on_press(|s: &mut App| s.screen = Screen::Settings)
        .build(k)
        .key("toolbar-settings");
    Toolbar::new().center(search_field(s, t)).trailing(row().gap(2.0).child(refresh).child(settings))
}

// ── the list ──

fn avatar(name: &str, email: &str, t: &Theme, size: f32) -> El {
    el().size(size, size)
        .radius(size / 2.0)
        .center()
        .fill(avatar_color(email, t.dark))
        .child(text(fmt::initial(name)).font(rune::TextStyle::new(size * 0.42, 600.0)).color(Color::WHITE))
}

fn list_row(s: &App, i: usize, t: &Theme) -> El {
    let k = &t.k;
    let m: &Summary = &s.list[i];
    let id = m.id;
    let selected = s.open == Some(id);
    let unread = m.thread_unread;
    let compact = t.compact;
    let preview_on = s.settings.bool("inbox.showPreview");
    let avatars = s.settings.bool("inbox.showAvatars");
    let role = s.view_role();
    let who = if matches!(role, Some(Role::Sent | Role::Drafts)) {
        match m.to.first() {
            None if role == Some(Role::Drafts) => "Draft".to_owned(),
            first => format!("To: {}", first.map(|a| a.display().to_owned()).unwrap_or_default()),
        }
    } else if m.thread_count > 1 && !m.participants.is_empty() {
        m.participants.clone()
    } else {
        m.from.display().to_owned()
    };
    let name_style = if unread { rune::TextStyle { weight: 650.0, ..k.ty.text_small } } else { k.ty.text_small };
    let name_color = if unread { k.c.text_base } else { k.c.text_muted };
    let name_row_base = || {
        let mut r = row().gap(6.0).center_y().min_w(0.0).child(text(who.clone()).font(name_style).color(name_color).truncate().min_w(0.0).shrink());
        if m.thread_count > 1 {
            r = r.child(text(m.thread_count.to_string()).font(k.ty.text_xsmall).color(k.c.text_faint));
        }
        r
    };
    let name_row = name_row_base().grow(1.0);
    let date = text(fmt::list_date(m.date, &s.settings.str("inbox.dateFormat")))
        .font(if unread { rune::TextStyle { weight: 600.0, ..k.ty.text_xsmall } } else { k.ty.text_xsmall })
        .color(if unread { t.accent } else { k.c.text_faint })
        .group_opacity(1.0, 0.0);
    let subject = if m.subject.is_empty() { "(no subject)".to_owned() } else { m.subject.clone() };
    let subject_style = if unread { rune::TextStyle { weight: 600.0, ..k.ty.text_small } } else { k.ty.text_small };

    // Quick actions, shown while the row is hovered.
    let act = |ic: &'static rune::Icon, label: &str, key: &str, f: fn(&mut App, i64, &mut Tx)| {
        IconButton::new(ic)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(label)
            .help(label)
            .on_press_tx(move |s: &mut App, tx: &mut Tx| f(s, id, tx))
            .build(k)
            .key(format!("{key}-{id}"))
    };
    let actions = row()
        .key(format!("acts-{id}"))
        .gap(0.0)
        .px(2.0)
        .radius(8.0)
        .fill(if selected { t.selected } else { t.card })
        .pin(None, Some(if compact { 3.0 } else { 7.0 }), Some(8.0), None)
        .group_opacity(0.0, 1.0);
    // A draft is only discarded (with Undo): it isn't archived, and it's never unread.
    let actions = if role == Some(Role::Drafts) || id < 0 {
        actions.child(act(&icons::TRASH, "Discard draft", "d", |s, id, tx| s.delete(vec![id], tx)))
    } else {
        actions
            .child(act(&icons::ARCHIVE, "Archive", "a", |s, id, tx| s.archive(vec![id], tx)))
            .child(act(&icons::TRASH, "Delete", "d", |s, id, tx| s.delete(vec![id], tx)))
            .child(act(if unread { &icons::MAIL_OPEN } else { &icons::MAIL_DOT }, if unread { "Mark as read" } else { "Mark as unread" }, "r", |s, id, _| s.toggle_read(id)))
    };

    let star_icon = if m.flagged() { &icons::STAR_FILLED } else { &icons::STAR };
    let star = el()
        .key(format!("star-{id}"))
        .role("button")
        .label(if m.flagged() { "Unstar" } else { "Star" })
        .size(22.0, 22.0)
        .center()
        .radius(6.0)
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut App| s.toggle_star(id))
        .child(icon(star_icon, 15.0).color(if m.flagged() { t.star } else { k.c.icon_faint }))
        .group_opacity(if m.flagged() { 1.0 } else { 0.0 }, 1.0);

    let dot = el().size(7.0, 7.0).radius(3.5).fill(if unread { t.unread } else { Color::TRANSPARENT });
    let attach = if m.has_attachments { Some(icon(&icons::PAPERCLIP, 13.0).color(k.c.icon_faint)) } else { None };

    let body: El = if compact {
        let mut line = row()
            .gap(10.0)
            .center_y()
            .grow(1.0)
            .min_w(0.0)
            .child(name_row_base().w(190.0).min_w(190.0).max_w(190.0))
            .child({
                // One text run, so the subject keeps its room and the preview takes what's left.
                let mut content = subject.clone();
                let mut spans = vec![rune::Span::new(0, content.len()).weight(subject_style.weight).color(k.c.text_base)];
                if preview_on && !m.preview.is_empty() {
                    let start = content.len();
                    content.push_str(" — ");
                    content.push_str(&m.preview);
                    spans.push(rune::Span::new(start, content.len()).color(k.c.text_faint));
                }
                rune::rich(content, spans).font(k.ty.text_small).truncate().grow(1.0).min_w(0.0)
            }            );
        if let Some(a) = attach {
            line = line.child(a);
        }
        line.child(star).child(date)
    } else {
        let mut top = row().gap(8.0).center_y().w_full().child(name_row);
        if let Some(a) = attach {
            top = top.child(a);
        }
        top = top.child(date);
        let mut c = col()
            .gap(2.0)
            .grow(1.0)
            .min_w(0.0)
            .child(top)
            .child(row().gap(6.0).center_y().w_full().child(text(subject).font(subject_style).color(k.c.text_base).truncate().grow(1.0).min_w(0.0)).child(star));
        if preview_on {
            c = c.child(text(if m.preview.is_empty() { " ".to_owned() } else { m.preview.clone() }).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().w_full());
        }
        c
    };

    let mut r = row()
        .key(row_key(id))
        .role("row")
        .label(format!("{}{}, {}", if unread { "Unread, " } else { "" }, m.from.display(), m.subject))
        .w_full()
        .gap(10.0)
        .px(if compact { 10.0 } else { 12.0 })
        .py(if compact { 7.0 } else { 10.0 })
        .center_y()
        .radius(10.0)
        .hover_group()
        .fill(if selected { t.selected } else { Color::TRANSPARENT })
        .hover(if selected { Color::TRANSPARENT } else { k.c.hover })
        .press(k.c.pressed)
        .on_press(move |s: &mut App| s.open_message(id))
        .on_double_press(move |s: &mut App| {
            if s.settings.str("inbox.readingPane") == "off" {
                s.open_message(id);
            }
        })
        .child(dot);
    if avatars && !compact {
        r = r.child(avatar(m.from.display(), &m.from.email, t, 34.0));
    }
    r.child(body).child(actions)
}

/// Messages the server says the current view has (from the folder counts), and its name.
fn server_total(s: &App) -> Option<(u32, String)> {
    let boxes: Vec<&Mailbox> = match &s.view {
        View::Mailbox { account, mailbox } => s.mailboxes.get(account).into_iter().flatten().filter(|m| &m.name == mailbox).collect(),
        View::Role { account, role } => s
            .mailboxes
            .iter()
            .filter(|(a, _)| account.as_ref().is_none_or(|x| x == *a))
            .flat_map(|(_, m)| m.iter())
            .filter(|m| m.role == *role)
            .collect(),
        View::Starred { .. } => Vec::new(),
    };
    let total: u32 = boxes.iter().map(|m| m.total).sum();
    let name = boxes.first().map(|m| m.display.clone()).unwrap_or_default();
    (total > 0).then_some((total, name))
}

fn empty_list(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let downloading = s.applied_query.is_empty() && s.list_loaded && !s.demo;
    let pending = if downloading { server_total(s) } else { None };
    let syncing = s.sync.values().any(|st| matches!(st, SyncState::Connecting | SyncState::Syncing(_)));
    let (title, sub): (String, String) = if !s.list_loaded {
        ("Loading…".into(), String::new())
    } else if let Some((n, name)) = &pending {
        let noun = if *n == 1 { "message" } else { "messages" };
        (format!("Downloading {name}…"), format!("{n} {noun} on the server. The newest appear here first."))
    } else if syncing && s.applied_query.is_empty() {
        ("Syncing your mail…".into(), "Messages appear here as they download.".into())
    } else if !s.applied_query.is_empty() {
        ("No results".into(), "Only mail stored on this computer is searched. Try fewer words.".into())
    } else if matches!(&s.view, View::Role { role: Role::Inbox, .. }) {
        ("All caught up".into(), "Nothing in your inbox.".into())
    } else {
        ("Nothing here".into(), String::new())
    };
    let busy = pending.is_some() || (syncing && s.applied_query.is_empty());
    col()
        .key("empty-list")
        .w_full()
        .h_full()
        .center()
        .gap(8.0)
        .padding(24.0)
        .child(if busy { Spinner::new().build(k) } else { icon(&icons::INBOX, 36.0).color(k.c.icon_faint) })
        .child(text(title).font(k.ty.body_medium).color(k.c.text_muted))
        .child(text(sub).font(k.ty.text_small).color(k.c.text_faint).wrap())
}

fn list_header(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let unread: u32 = s.list.iter().filter(|m| m.thread_unread).count() as u32;
    let noun = if s.conversations() { "conversations" } else { "messages" };
    // While the rest of a long list is on its way (a frame or two), its counts are "at least".
    let more = if s.list_complete { "" } else { "+" };
    let n = s.list.len();
    let sub = if !s.applied_query.is_empty() {
        format!("{n}{more} results for \u{201c}{}\u{201d}", s.applied_query)
    } else if unread > 0 {
        format!("{unread}{more} unread · {n}{more} {noun}")
    } else {
        format!("{n}{more} {noun}")
    };
    row()
        .key("list-header")
        .w_full()
        .px(16.0)
        .pt(12.0)
        .pb(8.0)
        .gap(10.0)
        .items_end()
        .child(text(s.view_title()).font(rune::TextStyle::new(18.0, 650.0).lh(24.0)).color(k.c.text_base))
        .child(text(sub).font(k.ty.text_xsmall).color(k.c.text_faint).pb(2.0).truncate().min_w(0.0).grow(1.0))
}

/// Everything outside a row's own message that its look depends on, hashed once a frame.
fn rows_env(s: &App, t: &Theme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (t.compact, t.dark, s.settings.str("appearance.accent"), s.settings.bool("inbox.showPreview"), s.settings.bool("inbox.showAvatars")).hash(&mut h);
    // A Sent or Drafts list (a role view or the mailbox itself) shows recipients, and a draft
    // only Discard: list_row reads the view's role.
    let role = s.view_role();
    (role == Some(Role::Sent), role == Some(Role::Drafts), s.settings.str("inbox.dateFormat"), chrono::Local::now().date_naive()).hash(&mut h);
    h.finish()
}

/// A row's version for `memo`: what `list_row` shows of its message, whether it's selected,
/// and `env`. A press of j changes two rows' versions, so only those two are built again.
fn row_version(m: &Summary, selected: bool, env: u64) -> impl std::hash::Hash + '_ {
    let to = m.to.first().map(|a| (a.name.as_str(), a.email.as_str()));
    (
        (m.id, m.flags, m.thread_unread, m.thread_count, m.date, m.has_attachments, selected, env),
        (m.participants.as_str(), m.from.name.as_str(), m.from.email.as_str(), to, m.subject.as_str(), m.preview.as_str()),
    )
}

/// Identifies the rows' keys, in order, so the list's keys are re-read only when they change.
fn keys_version(list: &[Summary]) -> u64 {
    list.iter().fold(0xcbf2_9ce4_8422_2325u64 ^ list.len() as u64, |h, m| (h ^ m.id as u64).wrapping_mul(0x0100_0000_01b3))
}

pub fn message_list(s: &App, t: &Theme) -> El {
    let est = if t.compact { 36.0 } else if s.settings.bool("inbox.showPreview") { 78.0 } else { 60.0 };
    let body = if s.list.is_empty() {
        empty_list(s, t)
    } else {
        let theme = t.clone();
        let env = rows_env(s, t);
        col().key("list-scroll").scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0).grow(1.0).min_h(0.0).w_full().child(
            col()
                .key("rows")
                .w_full()
                .px(6.0)
                .growth(GrowthSpec::default())
                .virtual_rows(
                    s.list.len(),
                    est,
                    |s: &App, i| Cow::Owned(row_key(s.list[i].id)),
                    move |s: &App, i| {
                        let m = &s.list[i];
                        rune::memo(row_key(m.id), row_version(m, s.open == Some(m.id), env), || list_row(s, i, &theme))
                    },
                )
                .virtual_rows_version(keys_version(&s.list)),
        )
    };
    col().key("list-pane").w_full().h_full().min_h(0.0).child(list_header(s, t)).child(body)
}

fn drag_handle(t: &Theme, vertical: bool) -> El {
    let k = &t.k;
    let line = if vertical { el().w(1.0).h_full().fill(k.c.border_muted) } else { el().h(1.0).w_full().fill(k.c.border_muted) };
    let handle = if vertical { el().key("list-resize").w(9.0).h_full().cursor(Cursor::ResizeColumn) } else { el().key("list-resize").h(9.0).w_full().cursor(Cursor::ResizeRow) };
    handle
        .center()
        
        .on_drag(move |s: &mut App, d: &mut rune::Drag, tx: &mut Tx| {
            let start = *s.list_drag_start.get_or_insert(if vertical { s.list_width } else { s.list_height });
            if vertical {
                s.list_width = (start + d.dx).clamp(280.0, 900.0);
            } else {
                s.list_height = (start + d.dy).clamp(160.0, 900.0);
            }
            tx.instant();
            if d.phase == rune::Phase::Up {
                s.list_drag_start = None;
                let (key, v) = if vertical { ("inbox.listWidth", s.list_width) } else { ("inbox.listHeight", s.list_height) };
                s.set_setting(key, serde_json::json!(v.round()));
            }
        })
        .child(line)
}

pub fn mail_main(s: &App, t: &Theme, cx: &mut Cx<App>) -> El {
    let pane = s.settings.str("inbox.readingPane");
    let has_open = s.open.is_some();
    match pane.as_str() {
        "bottom" => col()
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(col().w_full().h(s.list_height).min_h(0.0).child(message_list(s, t)))
            .child(drag_handle(t, false))
            .child(col().w_full().grow(1.0).min_h(0.0).child(reader::reader(s, t, cx))),
        "off" => {
            if has_open {
                col().w_full().h_full().min_h(0.0).child(reader::reader(s, t, cx))
            } else {
                col().w_full().h_full().min_h(0.0).child(message_list(s, t))
            }
        }
        _ => row()
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(col().w(s.list_width).h_full().min_h(0.0).child(message_list(s, t)))
            .child(drag_handle(t, true))
            .child(col().grow(1.0).h_full().min_w(0.0).min_h(0.0).child(reader::reader(s, t, cx))),
    }
}

pub fn mail_screen(s: &App, t: &Theme, cx: &mut Cx<App>, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let main = match s.screen {
        Screen::Settings => view_settings::settings_main(s, t, layer),
        Screen::AddAccount => crate::view_account::add_account(s, t, layer),
        Screen::Mail => mail_main(s, t, cx),
    };
    let framed = s.settings.bool("appearance.windowFrame");
    let tb = if s.screen == Screen::Mail { toolbar(s, t) } else { Toolbar::new().title(if s.screen == Screen::Settings { "Settings" } else { "Add account" }) };
    SplitView::new(main)
        .sidebar(&s.sidebar, |s: &mut App| &mut s.sidebar, sidebar(s, t))
        .toolbar(tb)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(if framed { 8.0 } else { 0.0 })
        .toolbar_in_panel()
        .sidebar_padding(10.0, 8.0)
        .sidebar_toggle(SidebarToggle::new().size(ControlSize::Large).past_lights(16.0).edge(14.0).gap(4.0).in_header())
        .pane_motion(PaneMotion::new().spring(rune::Spring::new(0.3, 0.0)).scale_with_width(480.0, 0.15, 0.3).fade_tail(0.15))
        .reduced_motion(cx.is_reduced_motion())
        .build(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_built_again_when_the_view_changes_how_they_show() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        let t = crate::theme::build(&app.settings, true);
        app.view = View::Starred { account: None };
        let starred = rows_env(&app, &t);
        // A draft row shows its recipient and only Discard; a sent one its recipient.
        app.view = View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Drafts".into() };
        let drafts = rows_env(&app, &t);
        app.view = View::Mailbox { account: "demo".into(), mailbox: "[Gmail]/Sent Mail".into() };
        let sent = rows_env(&app, &t);
        assert!(drafts != starred && sent != starred && drafts != sent);
    }
}
