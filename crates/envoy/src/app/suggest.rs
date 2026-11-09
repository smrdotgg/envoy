//! Recipient suggestions in the composer's To, Cc and Bcc fields: who you write to, from your
//! own mail on this computer (`envoy_core::contacts`).
//!
//! Typing in a field looks up the entry at the caret (what's between the commas) off the UI
//! thread ([`Command::Contacts`]); meanwhile the suggestions shown narrow to the ones still
//! matching, so a keystroke never waits. An answer to anything but the newest lookup is
//! dropped. While suggestions show, ↑ and ↓ move through them, Return or Tab takes one (as
//! `Name <address>, `, ready for the next), and Esc hides them until the entry changes; with
//! none showing, the field keeps every key.

use rune::{Key, KeyEvent, TextEdit, Tx};

use envoy_core::contacts;
use envoy_core::engine::Command;
use envoy_core::model::Address;

use super::{App, Compose};

/// How many suggestions show at most.
pub const SHOWN: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipients {
    To,
    Cc,
    Bcc,
}

impl Recipients {
    pub fn key(self) -> &'static str {
        match self {
            Recipients::To => "to",
            Recipients::Cc => "cc",
            Recipients::Bcc => "bcc",
        }
    }
}

/// The suggestions for the entry being typed in one of the fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Suggest {
    /// The field, and the entry (trimmed) they're for.
    pub field: Option<Recipients>,
    pub typed: String,
    /// The newest lookup asked: an answer to an older one is stale.
    pub req: u64,
    pub items: Vec<Address>,
    /// The one Return takes.
    pub active: usize,
    /// Esc hid them, until the entry changes.
    pub dismissed: bool,
}

impl Compose {
    pub fn recipients(&self, f: Recipients) -> &TextEdit {
        match f {
            Recipients::To => &self.to,
            Recipients::Cc => &self.cc,
            Recipients::Bcc => &self.bcc,
        }
    }

    pub fn recipients_mut(&mut self, f: Recipients) -> &mut TextEdit {
        match f {
            Recipients::To => &mut self.to,
            Recipients::Cc => &mut self.cc,
            Recipients::Bcc => &mut self.bcc,
        }
    }

    /// The entry at a field's caret, trimmed.
    fn typed(&self, f: Recipients) -> String {
        let edit = self.recipients(f);
        edit.text[contacts::entry_at(&edit.text, edit.focus)].trim().to_owned()
    }

    /// What field `f` shows under it, and which is active: only while it has the focus and
    /// its caret is in the entry they were looked up for.
    pub fn suggestions(&self, f: Recipients) -> Option<(&[Address], usize)> {
        let s = &self.suggest;
        let edit = self.recipients(f);
        let shown = s.field == Some(f) && edit.focused && !s.dismissed && !s.items.is_empty() && !s.typed.is_empty() && self.typed(f) == s.typed;
        shown.then(|| (&s.items[..], s.active.min(s.items.len() - 1)))
    }

    /// The addresses in the fields (lowercase), but the entry being typed in `f`: someone
    /// already in the message isn't suggested again.
    fn present(&self, f: Recipients) -> Vec<String> {
        let mut out = Vec::new();
        for field in [Recipients::To, Recipients::Cc, Recipients::Bcc] {
            let edit = self.recipients(field);
            let typing = (field == f).then(|| contacts::entry_at(&edit.text, edit.focus));
            let mut start = 0;
            while start <= edit.text.len() {
                let entry = contacts::entry_at(&edit.text, start);
                if Some(&entry) != typing.as_ref() {
                    let e = &edit.text[entry.clone()];
                    let address = match (e.rfind('<'), e.rfind('>')) {
                        (Some(a), Some(b)) if a < b => &e[a + 1..b],
                        _ => e,
                    };
                    let address = address.trim().to_lowercase();
                    if !address.is_empty() {
                        out.push(address);
                    }
                }
                start = entry.end + 1;
            }
        }
        out
    }
}

/// Whether an entry is worth looking up: something typed, and not a whole `Name <address>`.
fn looks_up(typed: &str) -> bool {
    !typed.contains('>') && !contacts::query_words(typed).is_empty()
}

impl App {
    /// A recipients field was edited: its draft is saved shortly, and the entry at its caret
    /// looked up.
    pub fn recipients_edited(&mut self, f: Recipients) {
        self.compose_edited();
        let req = self.next_stamp() as u64;
        let demo = self.demo;
        let Some(c) = &mut self.compose else { return };
        let typed = c.typed(f);
        let s = &mut c.suggest;
        if s.field != Some(f) || s.typed != typed {
            s.dismissed = false;
        }
        s.field = Some(f);
        s.typed = typed.clone();
        if !looks_up(&typed) {
            s.items.clear();
            s.active = 0;
            return;
        }
        // Until the answer comes, the ones still matching stay, in their order.
        let words = contacts::query_words(&typed);
        let active = s.items.get(s.active).map(|a| a.email.clone());
        s.items.retain(|a| contacts::matches(&words, &contacts::terms(&a.name, &a.email)));
        s.active = active.and_then(|e| s.items.iter().position(|a| a.email == e)).unwrap_or(0);
        s.req = req;
        if demo {
            self.contacts_arrived(req, crate::demo::suggest(&typed));
        } else {
            self.send(Command::Contacts { req, typed });
        }
    }

    /// The people for lookup `req` ([`envoy_core::engine::Event::Contacts`]): shown if it's
    /// the newest, without those already in the message.
    pub(super) fn contacts_arrived(&mut self, req: u64, contacts: Vec<Address>) {
        let Some(c) = &mut self.compose else { return };
        if c.suggest.req != req {
            return;
        }
        let Some(f) = c.suggest.field else { return };
        let present = c.present(f);
        let s = &mut c.suggest;
        let active = s.items.get(s.active).map(|a| a.email.clone());
        s.items = contacts.into_iter().filter(|a| !present.contains(&a.email.to_lowercase())).take(SHOWN).collect();
        s.active = active.and_then(|e| s.items.iter().position(|a| a.email == e)).unwrap_or(0);
    }

    /// A key in recipients field `f`, before the field: taken only while suggestions show.
    pub fn suggestion_key(&mut self, f: Recipients, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let Some(c) = &mut self.compose else { return false };
        if c.recipients(f).preedit.is_some() || ev.mods.cmd || ev.mods.ctrl || ev.mods.alt || ev.mods.logo {
            return false;
        }
        let Some((items, active)) = c.suggestions(f) else { return false };
        let (n, email) = (items.len(), items[active].email.clone());
        match ev.key {
            Key::Down => c.suggest.active = (active + 1) % n,
            Key::Up => c.suggest.active = (active + n - 1) % n,
            Key::Enter if tx.key_repeat() => {}
            Key::Enter => self.accept_suggestion(f, &email),
            Key::Tab if !ev.mods.shift => self.accept_suggestion(f, &email),
            Key::Escape => c.suggest.dismissed = true,
            _ => return false,
        }
        true
    }

    /// The pointer is over suggestion `i`.
    pub fn hover_suggestion(&mut self, i: usize) {
        if let Some(c) = &mut self.compose
            && i < c.suggest.items.len()
        {
            c.suggest.active = i;
        }
    }

    /// Puts the suggestion for `email` in place of the entry being typed in `f`, followed by
    /// a comma for the next one when it's the last. One edit: undo takes it back.
    pub fn accept_suggestion(&mut self, f: Recipients, email: &str) {
        let Some(c) = &mut self.compose else { return };
        let Some(a) = c.suggest.items.iter().find(|a| a.email == email).cloned() else { return };
        let edit = c.recipients_mut(f);
        let entry = contacts::entry_at(&edit.text, edit.focus);
        let lead = if entry.start > 0 { " " } else { "" };
        let last = entry.end == edit.text.len();
        let text = format!("{lead}{}{}", a.recipient(), if last { ", " } else { "" });
        edit.break_history_group();
        edit.set_selection(entry.start, entry.end);
        edit.insert(&text);
        edit.break_history_group();
        c.suggest = Suggest { field: Some(f), ..Suggest::default() };
        self.compose_edited();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn addr(name: &str, email: &str) -> Address {
        Address { name: name.into(), email: email.into() }
    }

    /// A composer, To focused, with the engine's commands recorded (not demo mode).
    fn app() -> App {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.demo = false;
        app.compose_new();
        app
    }

    fn c(app: &App) -> &Compose {
        app.compose.as_ref().unwrap()
    }

    /// Types `text` at the end of field `f`, as the field would, then reports the edit.
    fn type_in(app: &mut App, f: Recipients, text: &str) {
        let edit = app.compose.as_mut().unwrap().recipients_mut(f);
        edit.focused = true;
        let end = edit.text.len();
        edit.set_caret(end);
        edit.insert(text);
        app.recipients_edited(f);
    }

    fn last_lookup(app: &App) -> (u64, String) {
        let sent = app.sent.borrow();
        let last = sent.iter().rev().find(|c| c.starts_with("Contacts")).expect("a lookup").clone();
        let req = last.split("req: ").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
        let typed = last.split("typed: \"").nth(1).unwrap().trim_end_matches("\" }").to_owned();
        (req, typed)
    }

    fn answer(app: &mut App, contacts: &[Address]) {
        let (req, _) = last_lookup(app);
        app.contacts_arrived(req, contacts.to_vec());
    }

    fn key(app: &mut App, f: Recipients, k: Key) -> bool {
        app.suggestion_key(f, &KeyEvent { key: k, mods: Mods::default() }, &mut Tx::default())
    }

    fn shown(app: &App, f: Recipients) -> Vec<String> {
        c(app).suggestions(f).map(|(items, _)| items.iter().map(|a| a.email.clone()).collect()).unwrap_or_default()
    }

    fn two() -> Vec<Address> {
        vec![addr("Jordan Lee", "jordan@northwind.dev"), addr("Jo March", "jo@march.example")]
    }

    #[test]
    fn typing_looks_up_the_entry_at_the_caret() {
        let mut app = app();
        type_in(&mut app, Recipients::To, "Ana <ana@x.com>, j");
        assert_eq!(last_lookup(&app).1, "j", "only the entry being typed");
        answer(&mut app, &two());
        assert_eq!(shown(&app, Recipients::To), vec!["jordan@northwind.dev", "jo@march.example"]);
        assert!(shown(&app, Recipients::Cc).is_empty(), "under the field typed in only");
        // A whole recipient typed by hand isn't looked up.
        let before = app.sent.borrow().len();
        type_in(&mut app, Recipients::To, "ordan <jl@x.com>");
        assert_eq!(app.sent.borrow().iter().skip(before).filter(|c| c.starts_with("Contacts")).count(), 0);
        assert!(shown(&app, Recipients::To).is_empty());
    }

    #[test]
    fn a_stale_answer_is_ignored_and_what_shows_narrows_meanwhile() {
        let mut app = app();
        type_in(&mut app, Recipients::To, "j");
        let (first, _) = last_lookup(&app);
        answer(&mut app, &two());
        type_in(&mut app, Recipients::To, "or");
        assert_eq!(shown(&app, Recipients::To), vec!["jordan@northwind.dev"], "narrowed before the answer, at once");
        // The answer to "j" comes after "jor" was asked: it's dropped.
        app.contacts_arrived(first, vec![addr("Jill", "jill@x.com")]);
        assert_eq!(shown(&app, Recipients::To), vec!["jordan@northwind.dev"]);
        answer(&mut app, &[addr("Jordan Lee", "jordan@northwind.dev"), addr("Jorge", "jorge@x.com")]);
        assert_eq!(shown(&app, Recipients::To), vec!["jordan@northwind.dev", "jorge@x.com"]);
        // An answer for a composer that's gone changes nothing.
        let (req, _) = last_lookup(&app);
        app.compose = None;
        app.contacts_arrived(req, two());
        assert!(app.compose.is_none());
    }

    #[test]
    fn arrows_move_return_takes_and_the_field_is_ready_for_the_next() {
        let mut app = app();
        type_in(&mut app, Recipients::To, "Ana <ana@x.com>, jo");
        answer(&mut app, &two());
        assert!(key(&mut app, Recipients::To, Key::Down));
        assert_eq!(c(&app).suggestions(Recipients::To).unwrap().1, 1);
        assert!(key(&mut app, Recipients::To, Key::Down), "wraps");
        assert!(key(&mut app, Recipients::To, Key::Up));
        assert_eq!(c(&app).suggestions(Recipients::To).unwrap().1, 1);
        let edits = c(&app).edits;
        assert!(key(&mut app, Recipients::To, Key::Enter));
        let to = &c(&app).to;
        assert_eq!(to.text, "Ana <ana@x.com>, Jo March <jo@march.example>, ");
        assert_eq!(to.focus, to.text.len(), "the caret after it");
        assert!(shown(&app, Recipients::To).is_empty(), "closed");
        assert!(c(&app).edits > edits && c(&app).dirty, "the draft is saved");
        assert!(!key(&mut app, Recipients::To, Key::Enter), "Return is the field's again");
    }

    #[test]
    fn tab_takes_one_in_the_middle_of_the_field_and_names_that_need_quotes_get_them() {
        let mut app = app();
        let to = &mut app.compose.as_mut().unwrap().to;
        to.text = "bo, Ana <ana@x.com>".into();
        to.focused = true;
        to.set_caret(2);
        app.recipients_edited(Recipients::To);
        answer(&mut app, &[addr("Lee, Bo", "bo@x.com")]);
        assert!(key(&mut app, Recipients::To, Key::Tab));
        assert_eq!(c(&app).to.text, "\"Lee, Bo\" <bo@x.com>, Ana <ana@x.com>");
        assert_eq!(c(&app).to.focus, "\"Lee, Bo\" <bo@x.com>".len());
    }

    #[test]
    fn esc_hides_them_until_the_entry_changes() {
        let mut app = app();
        type_in(&mut app, Recipients::Cc, "jo");
        answer(&mut app, &two());
        assert!(key(&mut app, Recipients::Cc, Key::Escape));
        assert!(shown(&app, Recipients::Cc).is_empty());
        assert!(!key(&mut app, Recipients::Cc, Key::Escape), "the next Esc is the composer's (it minimizes)");
        assert!(!key(&mut app, Recipients::Cc, Key::Down));
        type_in(&mut app, Recipients::Cc, "r");
        answer(&mut app, &two()[..1]);
        assert_eq!(shown(&app, Recipients::Cc), vec!["jordan@northwind.dev"], "back for the new entry");
    }

    #[test]
    fn with_nothing_to_suggest_every_key_is_the_fields() {
        let mut app = app();
        for k in [Key::Down, Key::Up, Key::Enter, Key::Tab, Key::Escape] {
            assert!(!key(&mut app, Recipients::To, k.clone()), "{k:?} before typing");
        }
        type_in(&mut app, Recipients::To, "zz");
        answer(&mut app, &[]);
        for k in [Key::Down, Key::Up, Key::Enter, Key::Tab, Key::Escape] {
            assert!(!key(&mut app, Recipients::To, k.clone()), "{k:?} with no suggestions");
        }
        type_in(&mut app, Recipients::To, "");
        answer(&mut app, &two());
        // Shift+Tab goes back a field; chords are shortcuts (mod+Enter sends).
        let mut tx = Tx::default();
        assert!(!app.suggestion_key(Recipients::To, &KeyEvent { key: Key::Tab, mods: Mods { shift: true, ..Mods::default() } }, &mut tx));
        assert!(!app.suggestion_key(Recipients::To, &KeyEvent { key: Key::Enter, mods: Mods { cmd: true, ..Mods::default() } }, &mut tx));
        // Focus elsewhere, or the caret moved to another entry: nothing shows, nothing's taken.
        app.compose.as_mut().unwrap().to.focused = false;
        assert!(!key(&mut app, Recipients::To, Key::Enter));
        let to = &mut app.compose.as_mut().unwrap().to;
        to.focused = true;
        to.text = format!("Ana <ana@x.com>, {}", to.text);
        to.set_caret(2);
        assert!(!key(&mut app, Recipients::To, Key::Down));
    }

    #[test]
    fn people_already_in_the_message_arent_suggested() {
        let mut app = app();
        app.compose.as_mut().unwrap().cc.text = "\"Lee, Jordan\" <Jordan@Northwind.dev>".into();
        type_in(&mut app, Recipients::To, "jo@march.example, jo");
        answer(&mut app, &[addr("Jordan Lee", "jordan@northwind.dev"), addr("Jo March", "jo@march.example"), addr("Joy", "joy@x.com")]);
        assert_eq!(shown(&app, Recipients::To), vec!["joy@x.com"]);
        // The entry being typed is no reason to leave its own match out.
        let mut app = super::tests::app();
        type_in(&mut app, Recipients::To, "jo@march.example");
        answer(&mut app, &two()[1..]);
        assert_eq!(shown(&app, Recipients::To), vec!["jo@march.example"]);
    }

    #[test]
    fn the_pointer_picks_too() {
        let mut app = app();
        type_in(&mut app, Recipients::Bcc, "j");
        answer(&mut app, &two());
        app.hover_suggestion(1);
        app.hover_suggestion(9);
        assert_eq!(c(&app).suggestions(Recipients::Bcc).unwrap().1, 1);
        app.accept_suggestion(Recipients::Bcc, "jordan@northwind.dev");
        assert_eq!(c(&app).bcc.text, "Jordan Lee <jordan@northwind.dev>, ");
    }

    #[test]
    fn demo_mode_suggests_from_its_sample_mail() {
        let mut app = App::new(true);
        crate::demo::populate(&mut app);
        app.compose_new();
        type_in(&mut app, Recipients::To, "j");
        assert_eq!(shown(&app, Recipients::To)[0], "jordan@northwind.dev");
        assert!(!shown(&app, Recipients::To).iter().any(|e| e == "you@gmail.com"));
    }
}
