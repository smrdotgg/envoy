//! Adding an account: Google sign-in, a Gmail app password, or any IMAP/SMTP server.

use rune::{Cursor, El, ScrollSpec, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, SegmentedControl, Spinner, TextInput};

use crate::app::{AddForm, AddMode, App, Screen};
use crate::icons;
use crate::theme::Theme;

fn labeled(t: &Theme, label: &str, input: El) -> El {
    let k = &t.k;
    col().w_full().gap(6.0).child(text(label.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted)).child(input)
}

fn input(edit: &TextEdit, placeholder: &str, key: &str, access: fn(&mut App) -> &mut TextEdit, t: &Theme) -> El {
    TextInput::new(edit).width(None).placeholder(placeholder).label(placeholder).on_edit(move |s: &mut App| access(s)).build(&t.k).key(key.to_owned())
}

fn link(t: &Theme, label: &str, url: &'static str, key: &str) -> El {
    let k = &t.k;
    text(label.to_owned())
        .key(key.to_owned())
        .font(k.ty.text_xsmall_medium)
        .color(t.accent)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |_: &mut App, tx: &mut Tx| {
            let _ = tx.open_url(url);
        })
}

fn step(t: &Theme, n: &str, body: El) -> El {
    let k = &t.k;
    row()
        .gap(10.0)
        .items_start()
        .w_full()
        .child(el().size(20.0, 20.0).radius(10.0).center().fill(k.c.layer_02).child(text(n.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted)))
        .child(col().grow(1.0).min_w(0.0).child(body))
}

fn note(t: &Theme, s: &str) -> El {
    text(s.to_owned()).font(t.k.ty.text_xsmall).color(t.k.c.text_muted).wrap().w_full()
}

pub fn form(s: &App, t: &Theme) -> El {
    let k = &t.k;
    let f = &s.add;
    let modes = ["Google sign-in", "Gmail app password", "Other email (IMAP)"];
    let current = match f.mode {
        AddMode::Google => 0,
        AddMode::AppPassword => 1,
        AddMode::Imap => 2,
    };
    let picker = SegmentedControl::new(&modes, Some(current))
        .label("How to sign in")
        .width(Some(520.0))
        .on_pick(|s: &mut App, i: usize, _: &mut Tx| {
            s.add.mode = match i {
                0 => AddMode::Google,
                1 => AddMode::AppPassword,
                _ => AddMode::Imap,
            };
            s.add.error = None;
        })
        .build(k)
        .key("add-mode");
    let mut body = col().w_full().gap(14.0);
    match f.mode {
        AddMode::Google => {
            body = body
                .child(labeled(t, "Gmail address (optional)", input(&f.email, "you@gmail.com", "field-add-email", |s| &mut s.add.email, t)))
                .child(labeled(t, "OAuth client ID", input(&f.client_id, "1234-abc.apps.googleusercontent.com", "field-add-client-id", |s| &mut s.add.client_id, t)))
                .child(labeled(t, "OAuth client secret", input(&f.client_secret, "GOCSPX-…", "field-add-client-secret", |s| &mut s.add.client_secret, t)))
                .child(
                    col()
                        .w_full()
                        .gap(8.0)
                        .padding(14.0)
                        .radius(10.0)
                        .fill(k.c.layer_01)
                        .child(text("Google sign-in needs your own OAuth client (one time, ~3 minutes):").font(k.ty.text_xsmall_medium).color(k.c.text_base).wrap())
                        .child(step(t, "1", col().gap(2.0).child(note(t, "Create a project and enable the Gmail API.")).child(link(t, "console.cloud.google.com/apis/library/gmail.googleapis.com", "https://console.cloud.google.com/apis/library/gmail.googleapis.com", "lnk-api"))))
                        .child(step(t, "2", col().gap(2.0).child(note(t, "OAuth consent screen: External, add yourself as a test user.")).child(link(t, "console.cloud.google.com/auth/audience", "https://console.cloud.google.com/auth/audience", "lnk-consent"))))
                        .child(step(t, "3", col().gap(2.0).child(note(t, "Clients → Create client → Desktop app. Paste its ID and secret above.")).child(link(t, "console.cloud.google.com/auth/clients", "https://console.cloud.google.com/auth/clients", "lnk-clients"))))
                        .child(note(t, "Or skip all this and use a Gmail app password instead.")),
                );
        }
        AddMode::AppPassword => {
            body = body
                .child(labeled(t, "Your name", input(&f.name, "Ada Lovelace", "field-add-name", |s| &mut s.add.name, t)))
                .child(labeled(t, "Gmail address", input(&f.email, "you@gmail.com", "field-add-email", |s| &mut s.add.email, t)))
                .child(labeled(
                    t,
                    "App password",
                    TextInput::new(&f.password)
                        .width(None)
                        .placeholder("xxxx xxxx xxxx xxxx")
                        .label("App password")
                        .secure()
                        .revealed(f.show_password)
                        .on_edit(|s: &mut App| &mut s.add.password)
                        .on_submit(|s: &mut App, _: &mut Tx| s.submit_add())
                        .build(k)
                        .key("field-add-password"),
                ))
                .child(
                    col()
                        .w_full()
                        .gap(8.0)
                        .padding(14.0)
                        .radius(10.0)
                        .fill(k.c.layer_01)
                        .child(step(t, "1", note(t, "Turn on 2-Step Verification for your Google account, if it isn't already.")))
                        .child(step(t, "2", col().gap(2.0).child(note(t, "Create an app password named \"Envoy\" and paste the 16 letters above.")).child(link(t, "myaccount.google.com/apppasswords", "https://myaccount.google.com/apppasswords", "lnk-apppw"))))
                        .child(note(t, "The password is stored in your system keychain, never in Envoy's files.")),
                );
        }
        AddMode::Imap => {
            let port_row = |host: El, port: El| row().w_full().gap(10.0).child(col().grow(1.0).min_w(0.0).child(host)).child(col().w(110.0).child(port));
            body = body
                .child(labeled(t, "Your name", input(&f.name, "Ada Lovelace", "field-add-name", |s| &mut s.add.name, t)))
                .child(labeled(
                    t,
                    "Email address",
                    TextInput::new(&f.email)
                        .width(None)
                        .placeholder("you@example.com")
                        .label("Email address")
                        .on_edit(|s: &mut App| &mut s.add.email)
                        .on_change(|s: &mut App, _: &mut Tx| {
                            if let Some((ih, ip, sh, sp)) = envoy_core::accounts::preset_for(&s.add.email.text) {
                                s.add.imap_host = TextEdit::new(ih);
                                s.add.imap_port = TextEdit::new(ip.to_string());
                                s.add.smtp_host = TextEdit::new(sh);
                                s.add.smtp_port = TextEdit::new(sp.to_string());
                            }
                        })
                        .build(k)
                        .key("field-add-email"),
                ))
                .child(labeled(
                    t,
                    "Password",
                    TextInput::new(&f.password).width(None).placeholder("Password or app password").label("Password").secure().on_edit(|s: &mut App| &mut s.add.password).build(k).key("field-add-password"),
                ))
                .child(port_row(
                    labeled(t, "IMAP server", input(&f.imap_host, "imap.example.com", "field-add-imap", |s| &mut s.add.imap_host, t)),
                    labeled(t, "Port", input(&f.imap_port, "993", "field-add-imap-port", |s| &mut s.add.imap_port, t)),
                ))
                .child(port_row(
                    labeled(t, "SMTP server", input(&f.smtp_host, "smtp.example.com", "field-add-smtp", |s| &mut s.add.smtp_host, t)),
                    labeled(t, "Port", input(&f.smtp_port, "465", "field-add-smtp-port", |s| &mut s.add.smtp_port, t)),
                ))
                .child(note(t, "Port 993 / 465 use TLS; 143 / 587 upgrade with STARTTLS."));
        }
    }
    if let Some(e) = &f.error {
        body = body.child(
            row()
                .key("add-error")
                .w_full()
                .gap(8.0)
                .padding(12.0)
                .radius(10.0)
                .fill(k.c.danger.bg)
                .items_start()
                .child(icon(&icons::ALERT, 15.0).color(k.c.danger.fg))
                .child(text(e.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().selectable(true).grow(1.0).min_w(0.0)),
        );
    }
    let label = match (f.mode, f.busy) {
        (AddMode::Google, true) => "Waiting for the browser…",
        (_, true) => "Checking…",
        (AddMode::Google, false) => "Sign in with Google",
        _ => "Add account",
    };
    let mut actions = row().w_full().gap(10.0).center_y().justify_end();
    if f.busy {
        actions = actions.child(Spinner::new().build(k));
    }
    if !s.accounts.is_empty() {
        actions = actions.child(
            Button::new("Cancel")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut App| {
                    s.add = AddForm::new();
                    s.screen = Screen::Mail;
                })
                .build(k)
                .key("add-cancel"),
        );
    }
    actions = actions.child(
        row()
            .key("add-submit")
            .role("button")
            .label(label)
            .focusable(!f.busy)
            .h(38.0)
            .px(20.0)
            .center()
            .radius(19.0)
            .fill(if f.busy { k.c.layer_03 } else { t.accent })
            .hover(rune::Color::WHITE.alpha(0.12))
            .press(rune::Color::BLACK.alpha(0.12))
            .cursor(Cursor::Pointer)
            .on_press(|s: &mut App| {
                if !s.add.busy {
                    s.submit_add()
                }
            })
            .child(text(label).font(k.ty.ui_medium).color(rune::Color::WHITE)),
    );
    col()
        .key("add-card")
        .w_full()
        .max_w(560.0)
        .gap(18.0)
        .padding(28.0)
        .radius(16.0)
        .fill(t.card)
        .border(1.0, k.c.border_muted)
        .child(picker)
        .child(body)
        .child(actions)
}

pub fn add_account(s: &App, t: &Theme, _layer: &mut Vec<El>) -> El {
    let k = &t.k;
    let welcome = s.accounts.is_empty();
    let head = col()
        .w_full()
        .max_w(560.0)
        .gap(6.0)
        .child(text(if welcome { "Welcome to Envoy" } else { "Add an account" }).font(rune::TextStyle::new(26.0, 650.0).lh(32.0)).color(k.c.text_base))
        .child(
            text(if welcome { "Fast, native mail that works offline. Connect an account to get started." } else { "Connect another mailbox. Its mail appears alongside your others." })
                .font(k.ty.text_small)
                .color(k.c.text_muted)
                .wrap(),
        );
    col()
        .key("add-scroll")
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(col().w_full().items_center().px(24.0).py(36.0).gap(22.0).child(head).child(form(s, t)))
}

