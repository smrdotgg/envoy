//! The split inbox, on screen: the inbox's four tabs (Important, Other, Newsletters,
//! Notifications), each with its unread count, and moving conversations between them by hand.
//!
//! Why tabs at the top of the inbox, and not sidebar entries: every category stays one key away
//! (1–4) with its count in view, so nothing is ever put out of sight without a number on it; the
//! sidebar keeps its size however many accounts there are (each account's inbox gets the same
//! tabs, and so does the unified inbox); and with `inbox.split` off they're simply gone.
//!
//! Search looks in every category (no tab is chosen while it shows) unless it says
//! `category:`. Categories are worked out by envoy-core on this computer and never reach the
//! server.

use serde_json::Value;

use envoy_core::engine::Command;
use envoy_core::model::{Category, MessageId, Role, Summary};
use rune::Tx;

use crate::app::{App, MovedAway, Toast, Undo};

/// A correction in the split inbox, to take back: the engine's key for it, what it did to the
/// rules, and the rows it took off the list.
pub struct Recategorized {
    pub key: i64,
    pub rules: RuleChange,
    pub rows: MovedAway,
}

/// What a correction did to `inbox.rules`, and only that, so taking it back leaves every other
/// change alone (another correction's, a hand edit reloaded meanwhile): the rules it added, and
/// the ones it replaced, each with where it was (the rule before it, and its index).
#[derive(Default)]
pub struct RuleChange {
    pub added: Vec<Value>,
    pub replaced: Vec<(Option<Value>, usize, Value)>,
}

impl RuleChange {
    /// `rules` with this change taken back: its rules out, and those it replaced back after the
    /// rule that was before each (or where it was, if that's gone). If its rules have gone
    /// already (edited away), what they replaced stays gone too.
    fn undo(&self, rules: &[Value]) -> Vec<Value> {
        let mut list = rules.to_vec();
        let mut found = self.added.is_empty();
        for a in &self.added {
            if let Some(i) = list.iter().position(|r| r == a) {
                list.remove(i);
                found = true;
            }
        }
        if found {
            for (before, index, rule) in &self.replaced {
                if list.contains(rule) {
                    continue;
                }
                let at = before.as_ref().and_then(|b| list.iter().position(|r| r == b)).map_or((*index).min(list.len()), |i| i + 1);
                list.insert(at, rule.clone());
            }
        }
        list
    }
}

impl App {
    /// The inbox is split into tabs here: the setting's on, and the view is an inbox (one
    /// account's, or all of them).
    pub fn split_applies(&self) -> bool {
        self.settings.bool("inbox.split") && self.view_role() == Some(Role::Inbox)
    }

    /// The tab whose list is shown: none while searching (search looks in every category).
    pub fn list_category(&self) -> Option<Category> {
        (self.split_applies() && self.applied_query.is_empty()).then_some(self.category)
    }

    /// The view's unread count in a category, as the engine last said (0 until it has).
    pub fn category_count(&self, c: Category) -> u32 {
        self.category_counts.as_ref().filter(|(view, _)| *view == self.view).map_or(0, |(_, counts)| counts[c.code() as usize])
    }

    /// Shows a tab (1–4, or a click). A search on screen gives way to it. False when the inbox
    /// isn't split here, so the key does nothing.
    pub fn show_category(&mut self, c: Category) -> bool {
        if !self.split_applies() {
            return false;
        }
        if self.category != c || !self.applied_query.is_empty() {
            self.category = c;
            self.search = rune::TextEdit::new("");
            self.applied_query.clear();
            self.list.clear();
            self.list_loaded = false;
            self.close_message();
            self.selection.clear();
        }
        self.request_list();
        true
    }

    /// Moves the targets' conversations to `c` (alt+1–4, or Move to…), and their senders' mail
    /// to come with a rule each (`inbox.rules`, newest first, replacing an older one for the same
    /// sender), so the correction sticks. Off the tab at once; one Undo takes back both.
    pub fn move_to_category(&mut self, ids: Vec<MessageId>, c: Category, tx: &mut Tx) -> bool {
        if ids.is_empty() || !self.split_applies() {
            return false;
        }
        let ids = self.settled(ids);
        if ids.is_empty() {
            return true;
        }
        let rows: Vec<Summary> = self.rows_of(&ids).into_iter().cloned().collect();
        let mine: Vec<String> = self.accounts.iter().map(|a| a.email.trim().to_lowercase()).collect();
        let mut senders: Vec<String> = Vec::new();
        for r in &rows {
            let s = r.from.email.trim().to_lowercase();
            if !s.is_empty() && !mine.contains(&s) && !senders.contains(&s) {
                senders.push(s);
            }
        }
        let mut change = RuleChange::default();
        if !senders.is_empty() {
            let mut list = self.settings.value("inbox.rules").as_array().cloned().unwrap_or_default();
            let mut i = 0;
            while i < list.len() {
                if senders.iter().any(|s| is_rule_for(&list[i], s)) {
                    let before = i.checked_sub(1).map(|b| list[b].clone());
                    change.replaced.push((before, i, list.remove(i)));
                } else {
                    i += 1;
                }
            }
            for s in senders.iter().rev() {
                let rule = serde_json::json!({ "from": s, "category": c.as_str() });
                list.insert(0, rule.clone());
                change.added.insert(0, rule);
            }
            self.set_rules(list);
        }
        let key = self.next_stamp();
        self.send(Command::MoveToCategory { ids: ids.clone(), category: c, undo: key });
        let snapshot = self.snapshot(key, &ids);
        if self.list_category().is_some_and(|tab| tab != c) {
            self.drop_rows(&ids, tx);
        } else {
            for s in self.list.iter_mut().filter(|s| ids.contains(&s.id)) {
                s.category = c;
            }
        }
        self.list_changed_here();
        let text = match (rows.len(), senders.as_slice()) {
            (1, [sender]) => format!("Moved to {}. Mail from {sender} will go there too", c.label()),
            (n, []) => if n == 1 { format!("Moved to {}", c.label()) } else { format!("Moved {n} to {}", c.label()) },
            (1, _) => format!("Moved to {}, and its senders' mail", c.label()),
            (n, [_]) => format!("Moved {n} to {}, and their sender's mail", c.label()),
            (n, s) => format!("Moved {n} to {}, and mail from their {} senders", c.label(), s.len()),
        };
        let window = self.settings.u32("behavior.undoSeconds").max(5);
        self.toast_seq += 1;
        let undo = Undo::Category(Box::new(Recategorized { key, rules: change, rows: snapshot }));
        self.toasts.push(Toast { id: self.toast_seq, text, undo: Some(undo), until: self.clock + window as f64, error: false, open_path: None });
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        tx.wake_after(std::time::Duration::from_secs_f64(window as f64 + 0.05));
        true
    }

    /// Takes back [`App::move_to_category`]: the rules as they were, the conversations where
    /// they were, and the rows back on the list if it's still the one they left.
    pub fn undo_category(&mut self, r: Recategorized, tx: &mut Tx) {
        let rules = self.settings.value("inbox.rules").as_array().cloned().unwrap_or_default();
        let back = r.rules.undo(&rules);
        if back != rules {
            self.set_rules(back);
        }
        self.send(Command::UndoCategory { undo: r.key });
        let shown = self.restore_rows(&r.rows);
        self.list_changed_here();
        if shown && let Some(id) = r.rows.open {
            self.open_message(id);
            tx.scroll_into_view(crate::app::row_key(id));
        }
    }

    /// Saves `inbox.rules`; Settings then shows them as they are now, not as last typed there.
    fn set_rules(&mut self, rules: Vec<Value>) {
        self.set_setting("inbox.rules", Value::Array(rules));
        self.settings_ui.edits.remove("inbox.rules");
    }
}

/// A rule `inbox.rules` holds for exactly this sender (no list), as a correction writes.
fn is_rule_for(rule: &Value, sender: &str) -> bool {
    let from = rule.get("from").and_then(Value::as_str).map(|f| f.trim().to_lowercase());
    let list = rule.get("list").and_then(Value::as_str).unwrap_or("");
    from.as_deref() == Some(sender) && list.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Action;
    use rune::{KeyEvent, Mods};

    fn key(app: &mut App, name: &str, mods: Mods) -> bool {
        app.on_key(&KeyEvent { key: rune::Key::Text(name.into()), mods }, &mut Tx::default())
    }

    fn alt() -> Mods {
        Mods { alt: true, ..Mods::default() }
    }

    fn categories(app: &App) -> Vec<Category> {
        app.list.iter().map(|m| m.category).collect()
    }

    #[test]
    fn the_inbox_opens_on_important_and_the_number_keys_switch_tabs() {
        // Demo mode's sample mail, sorted by the real classifier as the engine stored it.
        let mut app = crate::demo::testing::app();
        assert!(app.split_applies());
        assert_eq!(app.list_category(), Some(Category::Important));
        assert!(!app.list.is_empty() && categories(&app).iter().all(|c| *c == Category::Important));
        let subjects = |app: &App| app.list.iter().map(|m| m.subject.clone()).collect::<Vec<_>>();
        assert_eq!(subjects(&app), ["Design review moved to Thursday", "Dinner Saturday?", "Re: Q3 roadmap draft", "photos from the trip!!"]);
        // Every tab's unread count, nothing hidden without one: what each tab lists unread.
        let mut unread = [0; 4];
        for (k, c) in [("1", Category::Important), ("2", Category::Other), ("3", Category::Newsletters), ("4", Category::Notifications)] {
            assert!(key(&mut app, k, Mods::default()), "{k}");
            app.settle();
            assert_eq!(app.list_category(), Some(c));
            assert!(!app.list.is_empty() && categories(&app).iter().all(|x| *x == c), "{k}: {:?}", categories(&app));
            unread[c.code() as usize] = app.list.iter().filter(|m| m.thread_unread).count() as u32;
        }
        assert_eq!(Category::ALL.map(|c| unread[c.code() as usize]), [2, 0, 0, 1], "Maya's and Sam's, none, none, GitHub's");
        for c in Category::ALL {
            assert_eq!(app.category_count(c), unread[c.code() as usize], "{c:?}");
        }
        // An opened conversation says why it's in its tab.
        assert!(key(&mut app, "1", Mods::default()));
        app.settle();
        let first = app.list[0].id;
        app.open_message(first);
        assert!(app.settle_until(|app| app.explained.is_some()));
        assert_eq!(app.explained.as_ref().map(|e| (e.0, e.1, e.2.as_str())), Some((first, Category::Important, "From someone you've written to")));
        // A search looks in every category, and a tab key brings the tabs back.
        app.close_message();
        app.applied_query = "receipt".into();
        app.request_list();
        app.settle();
        assert_eq!(app.list_category(), None);
        assert_eq!(categories(&app), [Category::Notifications], "found outside the tab");
        app.applied_query = "category:news".into();
        app.request_list();
        app.settle();
        assert!(!app.list.is_empty() && categories(&app).iter().all(|c| *c == Category::Newsletters));
        assert!(key(&mut app, "3", Mods::default()));
        assert_eq!((app.applied_query.as_str(), app.list_category()), ("", Some(Category::Newsletters)));
        // Not split (off, or not an inbox): the keys do nothing, and every message shows.
        app.go(envoy_core::model::View::Role { account: Some("demo".into()), role: Role::Sent });
        assert!(!key(&mut app, "2", Mods::default()));
        app.set_setting("inbox.split", serde_json::json!(false));
        app.go_role(Role::Inbox);
        app.settle();
        assert_eq!(app.list_category(), None);
        assert_eq!(app.list.len(), 12, "the whole inbox: {:?}", subjects(&app));
        assert!(!key(&mut app, "2", Mods::default()));
    }

    #[test]
    fn a_correction_moves_the_conversation_adds_a_rule_and_one_undo_takes_both_back() {
        let mut app = crate::demo::testing::app();
        let mut tx = Tx::default();
        assert!(key(&mut app, "3", Mods::default()));
        app.settle();
        app.engine = None;
        let row = app.list[0].clone();
        app.open_message(row.id);
        let before = app.list.len();
        let from = app.sent.borrow().len();
        // alt+1: to Important, and future mail from its sender.
        assert!(key(&mut app, "1", alt()));
        let sent: Vec<String> = app.sent.borrow()[from..].to_vec();
        assert!(sent.iter().any(|c| c.starts_with(&format!("MoveToCategory {{ ids: [{}], category: Important", row.id))), "{sent:?}");
        assert_eq!(app.list.len(), before - 1, "off the Newsletters tab at once");
        assert_eq!(app.settings.value("inbox.rules"), serde_json::json!([{ "from": row.from.email, "category": "important" }]));
        let toast = app.toasts.last().unwrap();
        assert_eq!(toast.text, format!("Moved to Important. Mail from {} will go there too", row.from.email));
        assert!(toast.undo.is_some());
        // Another correction for the same sender replaces its rule rather than adding one.
        app.list.insert(0, row.clone());
        app.move_to_category(vec![row.id], Category::Notifications, &mut tx);
        assert_eq!(app.settings.value("inbox.rules"), serde_json::json!([{ "from": row.from.email, "category": "notifications" }]));
        // z: the last correction's rules and rows back.
        let from = app.sent.borrow().len();
        assert!(key(&mut app, "z", Mods::default()));
        assert!(app.sent.borrow()[from..].iter().any(|c| c.starts_with("UndoCategory")));
        assert_eq!(app.settings.value("inbox.rules"), serde_json::json!([{ "from": row.from.email, "category": "important" }]));
        assert!(app.list.iter().any(|m| m.id == row.id), "back on the tab it left");
        assert!(key(&mut app, "z", Mods::default()));
        assert_eq!(app.settings.value("inbox.rules"), serde_json::json!([]), "and the first one's");
    }

    #[test]
    fn corrections_wait_for_a_target_and_never_fire_while_typing() {
        let mut app = crate::demo::testing::detached();
        let from = app.sent.borrow().len();
        assert!(!key(&mut app, "2", alt()), "nothing open or selected");
        // A selection: one correction for all of them, mail of yours making no rule.
        let ids: Vec<MessageId> = app.list.iter().take(2).map(|m| m.id).collect();
        for id in &ids {
            app.toggle_selected(*id);
        }
        let mut mine = app.list[0].clone();
        mine.from.email = "you@gmail.com".into();
        app.list[0] = mine;
        assert!(key(&mut app, "2", alt()));
        assert_eq!(app.settings.value("inbox.rules").as_array().unwrap().len(), 1, "no rule for your own address");
        assert!(app.toasts.last().unwrap().text.starts_with("Moved 2 to Other"));
        assert_eq!(app.sent.borrow()[from..].iter().filter(|c| c.starts_with("MoveToCategory")).count(), 1, "one command for the selection");
        assert!(app.selection.is_empty(), "they left the tab");
        // Bound like any key: rebinding moves it.
        app.set_setting("keys.moveNewsletters", serde_json::json!("alt+n"));
        assert!(app.keys.iter().any(|(a, kb)| *a == Action::MoveNewsletters && kb.chords().len() == 1));
    }

    /// Undo takes back only what its own correction did to the rules: out of order, and around
    /// an edit made meanwhile (settings.json reloaded), nothing else is lost.
    #[test]
    fn undo_reverses_only_its_own_rule_change() {
        let mut app = crate::demo::testing::detached();
        let mut tx = Tx::default();
        let rule = |from: &str, c: &str| serde_json::json!({ "from": from, "category": c });
        let id_of = |app: &App, email: &str| app.list.iter().find(|m| m.from.email == email).unwrap().id;
        // Sam's address has a rule already; Priya's has none.
        app.set_setting("inbox.rules", serde_json::json!([rule("x@y.example", "other"), rule("sam@ortiz.family", "newsletters")]));
        let (sam, priya) = (id_of(&app, "sam@ortiz.family"), id_of(&app, "priya@acme.co"));
        app.move_to_category(vec![sam], Category::Notifications, &mut tx);
        let first = app.toasts.last().unwrap().id;
        app.move_to_category(vec![priya], Category::Other, &mut tx);
        let second = app.toasts.last().unwrap().id;
        assert_eq!(
            app.settings.value("inbox.rules"),
            serde_json::json!([rule("priya@acme.co", "other"), rule("sam@ortiz.family", "notifications"), rule("x@y.example", "other")])
        );
        // settings.json edited by hand meanwhile, and reloaded: a rule appended.
        let mut edited = app.settings.clone();
        let mut rules = edited.value("inbox.rules").as_array().unwrap().clone();
        rules.push(rule("@news.example", "newsletters"));
        edited.set("inbox.rules", serde_json::Value::Array(rules)).unwrap();
        app.apply_settings(edited);
        // The first correction undone first: its rule goes, the one it replaced comes back where
        // it was; the second correction's rule and the edit stay.
        app.undo_toast(first, &mut tx);
        assert_eq!(
            app.settings.value("inbox.rules"),
            serde_json::json!([rule("priya@acme.co", "other"), rule("x@y.example", "other"), rule("sam@ortiz.family", "newsletters"), rule("@news.example", "newsletters")])
        );
        app.undo_toast(second, &mut tx);
        assert_eq!(app.settings.value("inbox.rules"), serde_json::json!([rule("x@y.example", "other"), rule("sam@ortiz.family", "newsletters"), rule("@news.example", "newsletters")]));
    }

    /// The tabs show while a search does, so their counts are asked for with its list too.
    #[test]
    fn the_tabs_counts_are_asked_for_while_searching_too() {
        let mut app = crate::demo::testing::detached();
        let last_list = |app: &App| app.sent.borrow().iter().rev().find(|c| c.starts_with("List")).cloned().unwrap();
        app.applied_query = "dinner".into();
        app.request_list();
        assert!(last_list(&app).contains("category: None") && last_list(&app).contains("counts: true"), "{}", last_list(&app));
        app.applied_query.clear();
        app.request_list();
        assert!(last_list(&app).contains("category: Some(Important)") && last_list(&app).contains("counts: true"), "{}", last_list(&app));
        // No tabs (not an inbox): no counts.
        app.go(envoy_core::model::View::Starred { account: None });
        assert!(last_list(&app).contains("counts: false"), "{}", last_list(&app));
    }

    /// Counts answer a list request: an older request's, arriving after a newer one's, is
    /// dropped rather than put back on the tabs.
    #[test]
    fn counts_for_an_older_list_are_dropped() {
        let mut app = crate::demo::testing::detached();
        let mut tx = Tx::default();
        let view = app.view.clone();
        app.request_list();
        let older = app.list_req;
        app.request_list(); // asked again: something changed
        let newer = app.list_req;
        app.apply(envoy_core::engine::Event::CategoryCounts { req: newer, view: view.clone(), counts: [1, 2, 3, 4] }, &mut tx);
        app.apply(envoy_core::engine::Event::CategoryCounts { req: older, view: view.clone(), counts: [9, 9, 9, 9] }, &mut tx);
        assert_eq!(Category::ALL.map(|c| app.category_count(c)), [4, 1, 2, 3], "the newer list's counts");
    }

    #[test]
    fn move_to_offers_the_categories_in_a_split_inbox() {
        let mut app = crate::demo::testing::detached();
        let first = app.list[0].id;
        let ids = |app: &App| app.picker_options(crate::app::Picker::Move, &[first]).0.into_iter().map(|o| o.id).collect::<Vec<_>>();
        let with = ids(&app);
        assert!(["cat:important", "cat:other", "cat:newsletters", "cat:notifications"].iter().all(|c| with.contains(&c.to_string())), "{with:?}");
        app.open_message(first);
        app.pick(crate::app::Picker::Move, "cat:newsletters", &mut Tx::default());
        assert!(app.toasts.last().unwrap().text.starts_with("Moved to Newsletters"));
        app.set_setting("inbox.split", serde_json::json!(false));
        assert!(!ids(&app).iter().any(|o| o.starts_with("cat:")), "not split: no categories");
    }
}
