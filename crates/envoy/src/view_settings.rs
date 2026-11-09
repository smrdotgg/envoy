//! Settings, generated from the schema: search, categories, "modified only", one row per
//! setting with the right control, and a link to `settings.json` for editing by hand.

use rune::{Chord, Color, Cursor, El, ScrollSpec, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Select, Switch, TextArea, TextInput};
use serde_json::{Value, json};

use envoy_core::accounts::AuthKind;
use envoy_core::engine::Command;
use envoy_core::paths;
use envoy_core::settings::{CATEGORIES, Kind, SCHEMA, SettingDef};

use crate::app::{AddForm, App, Screen};
use crate::icons;
use crate::theme::Theme;

fn category_icon(c: &str) -> &'static rune::Icon {
    match c {
        "Appearance" => &icons::PALETTE,
        "Inbox" => &icons::INBOX,
        "Reading" => &icons::BOOK,
        "Compose" => &icons::PENCIL,
        "Sync" => &icons::REFRESH,
        "Behavior" => &icons::SLIDERS,
        "Keyboard" => &icons::KEYBOARD,
        _ => &icons::SETTINGS,
    }
}

pub fn sidebar(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let ui = &s.settings_ui;
    let mut sb = Sidebar::new().label("Settings").inset(0.0).header_gap(2.0).header_padding(4.0, 10.0).row_gap(1.0);
    sb = sb.header(
        SidebarRow::new("Back to mail")
            .icon(&icons::BACK)
            .height(32.0)
            .on_select(|s: &mut App, _| s.screen = Screen::Mail)
            .build(k)
            .key("settings-back"),
    );
    sb = sb.child(
        SidebarRow::new("All settings")
            .icon(&icons::SETTINGS)
            .height(32.0)
            .selected(ui.category.is_none() && !ui.accounts)
            .on_select(|s: &mut App, _| {
                s.settings_ui.category = None;
                s.settings_ui.accounts = false;
            })
            .build(k)
            .key("cat-all"),
    );
    sb = sb.child(
        SidebarRow::new("Accounts")
            .icon(&icons::USER)
            .height(32.0)
            .detail(s.accounts.len().to_string())
            .selected(ui.accounts)
            .on_select(|s: &mut App, _| {
                s.settings_ui.accounts = true;
                s.settings_ui.category = None;
            })
            .build(k)
            .key("cat-accounts"),
    );
    for (i, c) in CATEGORIES.iter().enumerate() {
        let modified = SCHEMA.iter().filter(|d| d.category == *c && !s.settings.is_default(d.key)).count();
        let mut r = SidebarRow::new(*c).icon(category_icon(c)).height(32.0).selected(ui.category == Some(i) && !ui.accounts);
        if modified > 0 {
            r = r.detail(format!("{modified} changed"));
        }
        sb = sb.child(
            r.on_select(move |s: &mut App, _| {
                s.settings_ui.category = Some(i);
                s.settings_ui.accounts = false;
            })
            .build(k)
            .key(format!("cat-{c}")),
        );
    }
    sb = sb.footer(
        SidebarRow::new("Edit settings.json")
            .icon(&icons::FILE_CODE)
            .height(32.0)
            .on_select(|s: &mut App, _| open_settings_file(s))
            .build(k)
            .key("open-settings-json"),
    );
    sb.build(k)
}

fn open_settings_file(s: &mut App) {
    let path = paths::settings_file();
    if !path.exists() {
        let _ = paths::write_atomic(&path, b"{\n}\n");
    }
    let defaults = paths::config_dir().join("settings.defaults.jsonc");
    let _ = std::fs::write(&defaults, envoy_core::settings::defaults_reference());
    if let Err(e) = open::that_detached(&path) {
        s.toast(format!("Couldn't open {}: {e}", path.display()), true);
    } else {
        s.toast("Opened settings.json — saved changes apply instantly. Every option is listed in settings.defaults.jsonc next to it.", false);
    }
}

fn matches(d: &SettingDef, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let q = q.to_lowercase();
    q.split_whitespace().all(|w| {
        d.title.to_lowercase().contains(w) || d.description.to_lowercase().contains(w) || d.key.to_lowercase().contains(w) || d.category.to_lowercase().contains(w)
    })
}

fn number_text(v: &Value) -> String {
    match v.as_f64() {
        Some(n) if n.fract() == 0.0 => format!("{}", n as i64),
        Some(n) => format!("{n}"),
        None => String::new(),
    }
}

fn edit_access(key: &'static str, initial: String) -> impl for<'a> Fn(&'a mut App) -> &'a mut TextEdit + 'static {
    move |s: &mut App| s.settings_ui.edits.entry(key.to_owned()).or_insert_with(|| TextEdit::new(initial.clone()))
}

fn keybinding_problems(spec: &str) -> Vec<String> {
    spec.split(',').map(str::trim).filter(|p| !p.is_empty() && Chord::parse(p).is_err()).map(str::to_owned).collect()
}

fn control(s: &App, t: &Theme, d: &'static SettingDef, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let key = d.key;
    let v = s.settings.value(key);
    match d.kind {
        Kind::Bool => {
            let on = v.as_bool().unwrap_or(false);
            Switch::new(on).hide_label().label(d.title).on_toggle(move |s: &mut App| s.set_setting(key, json!(!on))).build(k).key(format!("ctl-{key}"))
        }
        Kind::Enum(options) => {
            let labels: Vec<&str> = options.iter().map(|(_, l)| *l).collect();
            let current = options.iter().find(|(o, _)| Some(*o) == v.as_str()).map(|(_, l)| *l);
            Select::new(format!("sel-{key}"), &labels)
                .current(current)
                .width(230.0)
                .label(d.title)
                .open(s.settings_ui.open_select.as_deref() == Some(key))
                .on_toggle(move |s: &mut App| {
                    s.settings_ui.open_select = if s.settings_ui.open_select.as_deref() == Some(key) { None } else { Some(key.to_owned()) };
                })
                .on_close(|s: &mut App| s.settings_ui.open_select = None)
                .on_choose(move |s: &mut App, label: &str, _: &mut Tx| {
                    s.settings_ui.open_select = None;
                    if let Some((value, _)) = options.iter().find(|(_, l)| *l == label) {
                        s.set_setting(key, json!(value));
                    }
                })
                .build(k, layer)
        }
        Kind::Number { min, max, step } => {
            let current = number_text(&v);
            let edit = s.settings_ui.edits.get(key).cloned().unwrap_or_else(|| TextEdit::new(current.clone()));
            let parsed = edit.text.trim().parse::<f64>().ok();
            let invalid = parsed.is_none_or(|n| n < min || n > max);
            let n = v.as_f64().unwrap_or(min);
            let stepper = move |delta: f64| {
                move |s: &mut App| {
                    let next = ((n + delta) / step).round() * step;
                    s.settings_ui.edits.remove(key);
                    s.set_setting(key, json!(next.clamp(min, max)));
                }
            };
            row()
                .key(format!("ctl-{key}"))
                .gap(4.0)
                .center_y()
                .child(
                    IconButton::new(&icons::MINUS)
                        .variant(IconButtonVariant::GhostMuted)
                        .control_size(ControlSize::Small)
                        .label("Decrease")
                        .disabled(n <= min)
                        .on_press(stepper(-step))
                        .build(k)
                        .key(format!("dec-{key}")),
                )
                .child(
                    TextInput::new(&edit)
                        .width(Some(110.0))
                        .label(d.title)
                        .invalid(invalid)
                        .on_edit(edit_access(key, current))
                        .on_change(move |s: &mut App, _: &mut Tx| {
                            let text = s.settings_ui.edits.get(key).map(|e| e.text.clone()).unwrap_or_default();
                            if let Ok(n) = text.trim().parse::<f64>()
                                && n >= min
                                && n <= max
                            {
                                s.set_setting(key, json!(n));
                            }
                        })
                        .build(k)
                        .key(format!("field-{key}")),
                )
                .child(
                    IconButton::new(&icons::PLUS)
                        .variant(IconButtonVariant::GhostMuted)
                        .control_size(ControlSize::Small)
                        .label("Increase")
                        .disabled(n >= max)
                        .on_press(stepper(step))
                        .build(k)
                        .key(format!("inc-{key}")),
                )
                .help(format!("{min} – {max}"))
        }
        Kind::Text { multiline } => {
            let current = v.as_str().unwrap_or_default().to_owned();
            let edit = s.settings_ui.edits.get(key).cloned().unwrap_or_else(|| TextEdit::new(current.clone()));
            let on_change = move |s: &mut App, _: &mut Tx| {
                let text = s.settings_ui.edits.get(key).map(|e| e.text.clone()).unwrap_or_default();
                s.set_setting(key, json!(text));
            };
            if multiline {
                TextArea::new(&edit).label(d.title).rows(4).width(Some(320.0)).on_edit(edit_access(key, current)).on_change(on_change).build(k).key(format!("field-{key}"))
            } else {
                TextInput::new(&edit).label(d.title).width(Some(320.0)).on_edit(edit_access(key, current)).on_change(on_change).build(k).key(format!("field-{key}"))
            }
        }
        Kind::List => {
            // One entry per line; saved as a JSON array.
            let current = v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            let edit = s.settings_ui.edits.get(key).cloned().unwrap_or_else(|| TextEdit::new(current.clone()));
            // Entries that can't be read are kept as typed but do nothing: say so.
            let problems = envoy_core::settings::list_problems(key, edit.text.lines());
            let area = TextArea::new(&edit)
                .label(d.title)
                .rows(4)
                .width(Some(320.0))
                .invalid(!problems.is_empty())
                .on_edit(edit_access(key, current))
                .on_change(move |s: &mut App, _: &mut Tx| {
                    let text = s.settings_ui.edits.get(key).map(|e| e.text.clone()).unwrap_or_default();
                    let items: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                    s.set_setting(key, json!(items));
                })
                .build(k)
                .key(format!("field-{key}"));
            if problems.is_empty() {
                area
            } else {
                col()
                    .key(format!("ctl-{key}"))
                    .gap(4.0)
                    .items_end()
                    .child(area)
                    .child(text(format!("Ignored, not an address or domain: {}", problems.join(", "))).font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().max_w(320.0))
            }
        }
        Kind::Keybinding => {
            let current = v.as_str().unwrap_or_default().to_owned();
            let edit = s.settings_ui.edits.get(key).cloned().unwrap_or_else(|| TextEdit::new(current.clone()));
            let problems = keybinding_problems(&edit.text);
            let label = crate::app::keybind(&current).chords().iter().map(|c| crate::app::keybind(&c.to_string()).label()).collect::<Vec<_>>().join("  ");
            col()
                .key(format!("ctl-{key}"))
                .gap(4.0)
                .items_end()
                .child(
                    TextInput::new(&edit)
                        .label(d.title)
                        .width(Some(230.0))
                        .invalid(!problems.is_empty())
                        .on_edit(edit_access(key, current))
                        .on_change(move |s: &mut App, _: &mut Tx| {
                            let text = s.settings_ui.edits.get(key).map(|e| e.text.clone()).unwrap_or_default();
                            if keybinding_problems(&text).is_empty() {
                                s.set_setting(key, json!(text));
                            }
                        })
                        .build(k)
                        .key(format!("field-{key}")),
                )
                .child(if problems.is_empty() {
                    text(label).font(k.ty.text_xsmall).color(k.c.text_faint)
                } else {
                    text(format!("Not a key: {}", problems.join(", "))).font(k.ty.text_xsmall).color(k.c.danger.fg)
                })
        }
    }
}

fn setting_row(s: &App, t: &Theme, d: &'static SettingDef, show_category: bool, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let modified = !s.settings.is_default(d.key);
    let key = d.key;
    let title = if show_category { format!("{}: {}", d.category, d.title) } else { d.title.to_owned() };
    let mut info = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(3.0)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(text(title).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(if modified {
                    el().key(format!("reset-{key}"))
                        .role("button")
                        .label("Reset to default")
                        .px(6.0)
                        .h(18.0)
                        .center()
                        .radius(5.0)
                        .fill(k.c.layer_02)
                        .hover(k.c.hover)
                        .cursor(Cursor::Pointer)
                        .on_press(move |s: &mut App| s.reset_setting(key))
                        .help("Reset to default")
                        .child(text("Reset").font(k.ty.text_xsmall_medium).color(k.c.text_muted))
                } else {
                    el()
                }),
        )
        .child(text(d.description).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full());
    info = info.child(text(d.key).font(rune::TextStyle::new(11.0, 400.0).mono()).color(k.c.text_faint).selectable(true));
    row()
        .key(format!("setting-{key}"))
        .w_full()
        .gap(24.0)
        .px(18.0)
        .py(14.0)
        .items_center()
        .radius(10.0)
        .fill(if modified { t.selected.alpha(0.45) } else { Color::TRANSPARENT })
        .border_sides([if modified { 3.0 } else { 0.0 }, 0.0, 0.0, 0.0], t.accent)
        .child(info)
        .child(col().items_end().child(control(s, t, d, layer)))
}

pub fn settings_main(s: &App, t: &Theme, layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let ui = &s.settings_ui;
    if ui.accounts {
        return accounts_page(s, t);
    }
    let q = ui.query.text.trim().to_owned();
    let mut list = col().key("settings-list").w_full().max_w(900.0).gap(2.0);
    let mut shown = 0;
    let mut last_cat = "";
    for d in SCHEMA {
        if let Some(i) = ui.category
            && q.is_empty()
            && CATEGORIES.get(i) != Some(&d.category)
        {
            continue;
        }
        if !matches(d, &q) || (ui.modified_only && s.settings.is_default(d.key)) {
            continue;
        }
        if d.category != last_cat && (ui.category.is_none() || !q.is_empty()) {
            list = list.child(
                row()
                    .key(format!("cat-head-{}", d.category))
                    .gap(8.0)
                    .pt(if shown == 0 { 4.0 } else { 20.0 })
                    .pb(6.0)
                    .px(18.0)
                    .center_y()
                    .child(icon(category_icon(d.category), 16.0).color(k.c.icon_muted))
                    .child(text(d.category).font(rune::TextStyle::new(15.0, 650.0)).color(k.c.text_base)),
            );
        }
        last_cat = d.category;
        list = list.child(setting_row(s, t, d, false, layer));
        shown += 1;
    }
    if shown == 0 {
        list = list.child(col().w_full().py(40.0).center().child(text("No settings match.").font(k.ty.text_small).color(k.c.text_faint)));
    }
    let mut problems = col().w_full().max_w(900.0).gap(4.0);
    for p in &s.settings.problems {
        problems = problems.child(
            row()
                .gap(8.0)
                .px(14.0)
                .py(8.0)
                .radius(8.0)
                .fill(k.c.warning.bg)
                .child(icon(&icons::ALERT, 14.0).color(k.c.warning.fg))
                .child(text(format!("settings.json — {p}")).font(k.ty.text_xsmall).color(k.c.warning.fg).wrap().grow(1.0)),
        );
    }
    let path = paths::settings_file();
    let top = col()
        .key("settings-top")
        .w_full()
        .max_w(900.0)
        .gap(10.0)
        .child(
            row()
                .gap(10.0)
                .center_y()
                .w_full()
                .child(
                    col().grow(1.0).min_w(0.0).child(
                        TextInput::new(&ui.query)
                            .placeholder("Search settings")
                            .leading_icon(&icons::SEARCH)
                            .clear_button()
                            .label("Search settings")
                            .on_edit(|s: &mut App| &mut s.settings_ui.query)
                            .build(k)
                            .key("field-settings-search"),
                    ),
                )
                .child(
                    Switch::new(ui.modified_only)
                        .label("Modified only")
                        .on_toggle(|s: &mut App| s.settings_ui.modified_only = !s.settings_ui.modified_only)
                        .build(k)
                        .key("modified-only"),
                ),
        )
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(text(format!("Saved to {}", path.display())).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().min_w(0.0).shrink().selectable(true))
                .child(
                    Button::new("Open settings.json")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .icon(&icons::FILE_CODE)
                        .on_press(open_settings_file)
                        .build(k)
                        .key("open-json"),
                ),
        )
        .child(problems);
    col().key("settings-scroll").w_full().h_full().min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(
        col().w_full().items_center().px(28.0).py(18.0).gap(14.0).child(top).child(list),
    )
}

fn accounts_page(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let mut list = col().key("accounts-list").w_full().max_w(760.0).gap(10.0);
    list = list.child(
        row()
            .w_full()
            .center_y()
            .child(text("Accounts").font(rune::TextStyle::new(18.0, 650.0)).color(k.c.text_base).grow(1.0))
            .child(
                Button::new("Add account")
                    .icon(&icons::PLUS)
                    .variant(ButtonVariant::Contrast)
                    .on_press(|s: &mut App| {
                        s.add = AddForm::new();
                        s.screen = Screen::AddAccount;
                    })
                    .build(k)
                    .key("add-account"),
            ),
    );
    if s.accounts.is_empty() {
        list = list.child(text("No accounts yet.").font(k.ty.text_small).color(k.c.text_faint));
    }
    for a in &s.accounts {
        let id = a.id.clone();
        let auth = match a.auth {
            AuthKind::GoogleOAuth => "Google sign-in",
            AuthKind::Password => "Password",
        };
        let status = match s.sync.get(&a.id) {
            Some(envoy_core::model::SyncState::Error(e)) | Some(envoy_core::model::SyncState::Offline(e)) => e.clone(),
            Some(envoy_core::model::SyncState::Syncing(w)) => format!("Syncing {w}"),
            Some(envoy_core::model::SyncState::Connecting) => "Connecting…".into(),
            _ => "Connected".into(),
        };
        list = list.child(
            row()
                .key(format!("acct-card-{}", a.id))
                .w_full()
                .gap(14.0)
                .px(18.0)
                .py(14.0)
                .radius(12.0)
                .border(1.0, k.c.border_muted)
                .center_y()
                .child(
                    el().size(36.0, 36.0)
                        .radius(18.0)
                        .center()
                        .fill(crate::theme::avatar_color(&a.email, t.dark))
                        .child(text(crate::fmt::initial(&a.email)).font(rune::TextStyle::new(15.0, 600.0)).color(Color::WHITE)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(text(if a.display_name.is_empty() { a.email.clone() } else { format!("{} <{}>", a.display_name, a.email) }).font(k.ty.text_small_medium).color(k.c.text_base).truncate())
                        .child(text(format!("{auth} · IMAP {}:{} · SMTP {}:{}", a.imap_host, a.imap_port, a.smtp_host, a.smtp_port)).font(k.ty.text_xsmall).color(k.c.text_faint).truncate())
                        .child(text(status).font(k.ty.text_xsmall).color(k.c.text_muted).truncate()),
                )
                .child(
                    Button::new("Remove")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut App| {
                            s.send(Command::RemoveAccount { id: id.clone() });
                            s.toast("Account removed from this computer", false);
                        })
                        .build(k)
                        .key(format!("remove-{}", a.id)),
                ),
        );
    }
    list = list.child(
        text(format!("Accounts are listed in {} (no passwords; those are in your system keychain).", paths::accounts_file().display()))
            .font(k.ty.text_xsmall)
            .color(k.c.text_faint)
            .wrap()
            .selectable(true),
    );
    col().key("accounts-scroll").w_full().h_full().min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(col().w_full().items_center().px(28.0).py(22.0).child(list))
}
