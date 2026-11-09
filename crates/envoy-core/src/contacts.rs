//! Recipient suggestions: who you write to, learnt from your own mail on this computer, and
//! nothing more (docs/NORTH_STAR.md: "contacts only as far as autocomplete needs"). No address
//! book, no server, no network: the store counts the people in the mail it caches
//! ([`crate::store::Store::suggest`]), and this module ranks them and matches what's typed.
//!
//! **Ranking.** What the store keeps of an address ([`Stats`]) is how often, and how lately, you
//! wrote to it, it wrote to you, and it was copied on mail you got, and how much of its mail was
//! bulk (lists, newsletters). Its relevance to you ([`relevance`]):
//!
//! ```text
//! 10 × more(sent) × fresh(sent_at)  +  (2 × more(got) + more(cc)) × fresh(seen_at)
//! more(n) = n / (n + 2)              more mail counts, less and less
//! fresh(t) = 1 / (1 + days(t) / 30)  mail a month old counts half
//! ```
//!
//! So writing to someone weighs five times what hearing from them does. A bulk sender you've
//! never written to (its mail mostly lists or newsletters) comes after everyone else; a
//! no-reply address you've never written to isn't suggested at all; your own addresses never
//! are.
//!
//! **Matching.** Each word typed must start a word of the name, the address, or a part of the
//! address before the `@` (`jo` finds Jordan Lee <jl@x> and john.smith@y; `smi` finds the
//! latter too). Case and accents don't matter ([`fold`]).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::model::Address;

/// What the store knows of one address (in one account; [`rank`] adds up the accounts).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Messages you sent it (To, Cc or Bcc), and when the newest went (Unix seconds).
    pub sent: u32,
    pub sent_at: i64,
    /// Messages from it that weren't bulk.
    pub got: u32,
    /// Messages to you it was on too (To or Cc).
    pub cc: u32,
    /// Bulk messages from it: a mailing list's or a newsletter's.
    pub bulk: u32,
    /// The newest message from it or with it on.
    pub seen_at: i64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.sent += o.sent;
        self.sent_at = self.sent_at.max(o.sent_at);
        self.got += o.got;
        self.cc += o.cc;
        self.bulk += o.bulk;
        self.seen_at = self.seen_at.max(o.seen_at);
    }
}

/// Where a contact's name came from: one you wrote yourself (in your To, Cc or Bcc) beats the
/// one its own mail gives, which beats how others wrote it. Among names of the same source,
/// the newest wins.
pub mod source {
    pub const COPIED: i64 = 1;
    pub const THEIRS: i64 = 2;
    pub const YOURS: i64 = 3;
}

/// One stored contact, as [`rank`] takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub email: String,
    pub name: String,
    /// `(source, date)` of the name ([`source`]).
    pub name_rank: (i64, i64),
    pub stats: Stats,
}

/// How many days old mail is when it counts half.
pub const HALF_LIFE_DAYS: f64 = 30.0;

/// How relevant an address is to you, as `(tier, score)`: tier 0 for people you correspond
/// with, 1 for bulk senders you've never written to (after everyone else), and the score within
/// it. None for an address not to suggest (a no-reply one you've never written to).
pub fn relevance(s: &Stats, email: &str, now: i64) -> Option<(u8, f64)> {
    let wrote = s.sent > 0;
    if !wrote && is_noreply(email) {
        return None;
    }
    let fresh = |at: i64| 1.0 / (1.0 + (now - at).max(0) as f64 / 86_400.0 / HALF_LIFE_DAYS);
    let more = |n: u32| n as f64 / (n as f64 + 2.0);
    if !wrote && s.bulk > s.got + s.cc {
        return Some((1, more(s.bulk) * fresh(s.seen_at)));
    }
    Some((0, 10.0 * more(s.sent) * fresh(s.sent_at) + (2.0 * more(s.got) + more(s.cc)) * fresh(s.seen_at)))
}

/// An address that takes no replies: `noreply@`, `no-reply@`, `do_not_reply@`,
/// `calendar-noreply@`, `mailer-daemon@`, `postmaster@`, `bounces@`.
pub fn is_noreply(email: &str) -> bool {
    let local: String = fold(email.split('@').next().unwrap_or_default()).chars().filter(|c| c.is_alphanumeric()).collect();
    local.contains("noreply") || local.contains("donotreply") || local.starts_with("mailerdaemon") || local == "postmaster" || local.starts_with("bounce")
}

/// The best `limit` suggestions for what's typed (`words`, from [`query_words`]) among
/// `candidates`: every account's contact for each address one of whose contacts starts a term
/// with [`probe`]'s word, as the store's index finds them. Each address comes once, ranked (and
/// judged no-reply or bulk) on its history in all accounts added up; every word typed
/// must start a term of one of its names or of the address. Your own addresses (`me`,
/// lowercase) are left out.
pub fn rank(candidates: impl IntoIterator<Item = Candidate>, words: &[String], me: &HashSet<String>, now: i64, limit: usize) -> Vec<Address> {
    // Each address, its accounts added up, and every name it goes by.
    let mut by_email: HashMap<String, (Candidate, Vec<String>)> = HashMap::new();
    for c in candidates {
        if me.contains(&c.email) {
            continue;
        }
        match by_email.get_mut(&c.email) {
            Some((have, names)) => {
                have.stats.add(&c.stats);
                if !c.name.is_empty() && !names.contains(&c.name) {
                    names.push(c.name.clone());
                }
                if !c.name.is_empty() && c.name_rank > have.name_rank {
                    have.name = c.name;
                    have.name_rank = c.name_rank;
                }
            }
            None => {
                let names = if c.name.is_empty() { Vec::new() } else { vec![c.name.clone()] };
                by_email.insert(c.email.clone(), (c, names));
            }
        }
    }
    let found = |c: &Candidate, names: &[String]| {
        words.len() <= 1 || {
            let mut all = terms("", &c.email);
            for n in names {
                all.extend(terms(n, ""));
            }
            matches(words, &all)
        }
    };
    let mut scored: Vec<((u8, f64), Candidate)> =
        by_email.into_values().filter(|(c, names)| found(c, names)).filter_map(|(c, _)| Some((relevance(&c.stats, &c.email, now)?, c))).collect();
    scored.sort_by(|(a, x), (b, y)| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then_with(|| x.email.cmp(&y.email)));
    scored.into_iter().take(limit).map(|(_, c)| Address { name: c.name, email: c.email }).collect()
}

/// Every word typed starts one of the contact's terms.
pub fn matches(words: &[String], terms: &[String]) -> bool {
    words.iter().all(|w| terms.iter().any(|t| t.starts_with(w.as_str())))
}

/// The words a contact is found by, folded: each word of its name (and, for `O'Brien` or
/// `Jean-Luc`, the word whole and in parts, but not one-letter parts: `Rivera's` isn't found by
/// `s`), its address, and the parts of the address before
/// the `@` when there are several (`jordan.lee@x`: `jordan`, `lee`). Not the domain: typing `g`
/// shouldn't bring up everyone at gmail.com.
pub fn terms(name: &str, email: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in fold(name).split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '"' | '(' | ')' | '<' | '>' | '[' | ']' | '/' | '|')) {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        if word.is_empty() {
            continue;
        }
        out.push(word.to_owned());
        if word.contains(|c: char| !c.is_alphanumeric()) {
            out.push(word.chars().filter(|c| c.is_alphanumeric()).collect());
            out.extend(word.split(|c: char| !c.is_alphanumeric()).filter(|p| p.chars().nth(1).is_some()).map(str::to_owned));
        }
    }
    let email = fold(email.trim());
    if let Some((local, _)) = email.split_once('@') {
        let parts: Vec<&str> = local.split(['.', '_', '-', '+']).filter(|p| !p.is_empty()).collect();
        if parts.len() > 1 {
            out.extend(parts.into_iter().map(str::to_owned));
        }
    }
    if !email.is_empty() {
        out.push(email);
    }
    out.sort();
    out.dedup();
    out
}

/// The words of what's typed in a recipients field, folded, without the quotes and brackets
/// of a half-typed `"Name" <address>`.
pub fn query_words(typed: &str) -> Vec<String> {
    fold(typed)
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '<' | '>' | '(' | ')'))
        .map(|w| w.trim_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+' | '\''))))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The word of `words` the store's index looks up: the longest (the fewest contacts start
/// with it), the first of those.
pub fn probe(words: &[String]) -> Option<&str> {
    words.iter().fold(None::<&String>, |best, w| if best.is_none_or(|b| w.len() > b.len()) { Some(w) } else { best }).map(String::as_str)
}

/// A display name worth keeping: without surrounding quotes, and empty when it's only an
/// address (`'ana@x.com'`), as some mailers write one.
pub fn clean_name(name: &str) -> String {
    let n = name.trim().trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
    if n.contains('@') && !n.contains(' ') { String::new() } else { n.to_owned() }
}

/// An address worth suggesting: something before and after one `@`, no spaces.
pub fn plausible(email: &str) -> bool {
    let e = email.trim();
    matches!(e.split_once('@'), Some((l, d)) if !l.is_empty() && !d.is_empty() && !d.contains('@')) && !e.contains(char::is_whitespace)
}

/// The recipient being typed in a To, Cc or Bcc field: the byte range, around `caret`, between
/// the separators either side (commas and semicolons outside quotes and angle brackets, as
/// sending splits the field).
pub fn entry_at(field: &str, caret: usize) -> Range<usize> {
    let caret = caret.min(field.len());
    let (mut start, mut quoted, mut angle) = (0, false, false);
    let mut end = field.len();
    for (i, c) in field.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                if i < caret {
                    start = i + 1;
                } else {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    start..end.max(start)
}

/// Lowercase, without accents (Latin letters): what's typed and what's stored meet here, so
/// `zoe` finds Zoë and `muller` Müller.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        for l in c.to_lowercase() {
            match plain(l) {
                Some(p) => out.push_str(p),
                None => out.push(l),
            }
        }
    }
    out
}

/// A lowercase Latin letter's base letter(s), for the accented ones (Latin-1, Latin
/// Extended-A, and Romanian's ș and ț).
fn plain(c: char) -> Option<&'static str> {
    if c.is_ascii() {
        return None;
    }
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĳ' => "ij",
        'ĵ' => "j",
        'ķ' | 'ĸ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' | 'ſ' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// A Message-ID's 64-bit FNV-1a hash: how the store remembers which messages it has counted
/// people from, in a few bytes each. Stable across builds and platforms (it's stored).
pub fn message_hash(message_id: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in message_id.trim().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    fn words(q: &str) -> Vec<String> {
        query_words(q)
    }

    fn finds(q: &str, name: &str, email: &str) -> bool {
        matches(&words(q), &terms(name, email))
    }

    #[test]
    fn typing_finds_name_words_and_addresses() {
        assert!(finds("jo", "Jordan Lee", "jl@x.com"), "a name's first word");
        assert!(finds("lee", "Jordan Lee", "jl@x.com"), "a name's later word");
        assert!(finds("jo", "", "john@y.org"), "an address");
        assert!(finds("jl@", "Jordan Lee", "jl@x.com"), "an address, up to its @");
        assert!(finds("jl@x.c", "Jordan Lee", "jl@x.com"), "an address, into its domain");
        assert!(finds("smi", "", "john.smith@y.org"), "a part of an address");
        assert!(finds("john.sm", "", "john.smith@y.org"), "an address with its dots");
        assert!(finds("jordan l", "Jordan Lee", "jl@x.com"), "two words, both must match");
        assert!(finds("lee jor", "Jordan Lee", "jl@x.com"), "in any order");
        assert!(!finds("jordan k", "Jordan Lee", "jl@x.com"));
        assert!(!finds("x.com", "Jordan Lee", "jl@x.com"), "not by the domain alone");
        assert!(!finds("ee", "Jordan Lee", "jl@x.com"), "words start where they start");
    }

    #[test]
    fn case_accents_and_punctuation_dont_matter() {
        assert!(finds("ZOE", "Zoë Müller", "z@x.com"));
        assert!(finds("müll", "Zoë Muller", "z@x.com"));
        assert!(finds("soren", "Søren Ångström", "s@x.com"));
        assert!(finds("angs", "Søren Ångström", "s@x.com"));
        assert!(finds("obri", "Siobhán O'Brien", "s@x.com"), "O'Brien, whole");
        assert!(finds("bri", "Siobhán O'Brien", "s@x.com"), "and in parts");
        assert!(finds("o'b", "Siobhán O'Brien", "s@x.com"), "as written");
        assert!(finds("luc", "Jean-Luc Picard", "j@x.com"));
        assert!(!finds("s", "Dr. Rivera's Office", "frontdesk@x.com"), "not by a possessive's s");
        assert!(finds("rivera", "Dr. Rivera's Office", "frontdesk@x.com"));
        assert!(finds("a", "Ana L.", "a@x.com") && finds("l", "Ana L.", "a@x.com"), "a one-letter word is still a word");
        assert!(finds("lee", "\"Lee, Jordan\"", "j@x.com"), "Last, First");
        assert!(finds("\"jor", "Jordan Lee", "j@x.com"), "a half-typed quoted name");
        assert!(finds("<jl", "Jordan Lee", "jl@x.com"), "a half-typed bracketed address");
    }

    fn stats(sent: u32, sent_days_ago: i64, got: u32, cc: u32, bulk: u32, seen_days_ago: i64) -> Stats {
        Stats { sent, sent_at: NOW - sent_days_ago * DAY, got, cc, bulk, seen_at: NOW - seen_days_ago * DAY }
    }

    fn score(s: Stats, email: &str) -> (u8, f64) {
        relevance(&s, email, NOW).expect("suggested")
    }

    fn ahead(a: (u8, f64), b: (u8, f64)) -> bool {
        a.0 < b.0 || (a.0 == b.0 && a.1 > b.1)
    }

    #[test]
    fn people_you_write_to_come_before_people_who_only_write_to_you() {
        let wrote_once = score(stats(1, 10, 0, 0, 0, 0), "a@x");
        let they_write_often = score(stats(0, 0, 30, 0, 0, 10), "b@x");
        assert!(ahead(wrote_once, they_write_often), "{wrote_once:?} vs {they_write_often:?}");
        let copied_often = score(stats(0, 0, 0, 30, 0, 10), "c@x");
        assert!(ahead(they_write_often, copied_often), "writing to you counts more than being copied");
    }

    #[test]
    fn recent_mail_counts_more() {
        let lately = score(stats(3, 2, 0, 0, 0, 2), "a@x");
        let long_ago = score(stats(3, 400, 0, 0, 0, 400), "b@x");
        assert!(ahead(lately, long_ago));
        let half = score(stats(3, 30, 0, 0, 0, 30), "c@x").1 / score(stats(3, 0, 0, 0, 0, 0), "c@x").1;
        assert!((half - 0.5).abs() < 1e-9, "a month old counts half ({half})");
        // Someone you mailed once years ago is behind someone who writes to you every week.
        assert!(ahead(score(stats(0, 0, 150, 0, 0, 2), "a@x"), score(stats(1, 1500, 0, 0, 0, 1500), "b@x")));
    }

    #[test]
    fn more_mail_counts_less_and_less() {
        let (one, ten, hundred) = (score(stats(1, 1, 0, 0, 0, 1), "a@x").1, score(stats(10, 1, 0, 0, 0, 1), "a@x").1, score(stats(100, 1, 0, 0, 0, 1), "a@x").1);
        assert!(one < ten && ten < hundred);
        assert!(hundred - ten < ten - one);
    }

    #[test]
    fn bulk_senders_come_last_and_no_reply_addresses_not_at_all() {
        let newsletter = score(stats(0, 0, 0, 0, 200, 0), "news@shop.example");
        let faint = score(stats(0, 0, 0, 1, 0, 2000), "old@x");
        assert_eq!(newsletter.0, 1);
        assert!(ahead(faint, newsletter), "a newsletter this morning is behind someone copied once, years ago");
        assert!(relevance(&stats(0, 0, 3, 0, 0, 1), "noreply@github.com", NOW).is_none());
        for e in ["no-reply@x.com", "do_not_reply@x.com", "Calendar-NoReply@x.com", "mailer-daemon@x.com", "postmaster@x.com", "bounces+123@x.com"] {
            assert!(is_noreply(e), "{e}");
        }
        assert!(!is_noreply("nora@x.com") && !is_noreply("reply@x.com"));
        // Unless you've written to them.
        assert_eq!(score(stats(1, 5, 0, 0, 200, 0), "news@shop.example").0, 0);
        assert!(relevance(&stats(1, 5, 0, 0, 0, 5), "noreply@x.com", NOW).is_some());
        // Someone whose mail is mostly personal isn't bulk for the odd list post.
        assert_eq!(score(stats(0, 0, 5, 0, 2, 1), "colleague@x").0, 0);
    }

    fn candidate(name: &str, email: &str, s: Stats) -> Candidate {
        Candidate { email: email.into(), name: name.into(), name_rank: (source::THEIRS, s.seen_at), stats: s }
    }

    #[test]
    fn rank_orders_adds_up_accounts_and_leaves_out_me() {
        let me: HashSet<String> = ["me@x.com".to_owned()].into();
        let list = vec![
            candidate("Ana", "ana@x.com", stats(0, 0, 4, 0, 0, 3)),
            candidate("Bo", "bo@x.com", stats(2, 3, 0, 0, 0, 3)),
            candidate("Me", "me@x.com", stats(9, 1, 9, 0, 0, 1)),
            // Ana again, from another account, where you wrote to her.
            Candidate { name_rank: (source::YOURS, NOW), ..candidate("Ana Lima", "ana@x.com", stats(3, 1, 0, 0, 0, 1)) },
        ];
        let got = rank(list, &words("a"), &me, NOW, 8);
        assert_eq!(got.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), vec!["ana@x.com", "bo@x.com"]);
        assert_eq!(got[0].name, "Ana Lima", "the name you wrote");
        let got = rank(vec![candidate("Ana Lima", "ana@x.com", stats(1, 1, 0, 0, 0, 1)), candidate("Ana Bell", "bell@x.com", stats(1, 1, 0, 0, 0, 1))], &words("ana li"), &me, NOW, 8);
        assert_eq!(got.len(), 1, "every word must match: {got:?}");
        assert_eq!(rank(vec![candidate("Ana", "ana@x.com", stats(1, 1, 0, 0, 0, 1))], &words("a"), &me, NOW, 0), vec![]);
    }

    #[test]
    fn the_entry_being_typed_is_between_separators() {
        let f = "Ana <ana@x.com>, jo";
        assert_eq!(&f[entry_at(f, f.len())], " jo");
        assert_eq!(&f[entry_at(f, 3)], "Ana <ana@x.com>");
        let f = "\"Lee, Jordan\" <jl@x.com>; bo, cy";
        assert_eq!(&f[entry_at(f, 5)], "\"Lee, Jordan\" <jl@x.com>", "a comma in quotes doesn't split");
        assert_eq!(&f[entry_at(f, 27)], " bo");
        assert_eq!(&f[entry_at(f, f.len())], " cy");
        assert_eq!(entry_at("", 0), 0..0);
        assert_eq!(entry_at("a,", 2), 2..2);
        assert_eq!(entry_at("ab", 99), 0..2, "a caret past the end is at the end");
    }

    #[test]
    fn names_and_addresses_are_cleaned() {
        assert_eq!(clean_name("  \"Jordan Lee\" "), "Jordan Lee");
        assert_eq!(clean_name("'ana@x.com'"), "");
        assert_eq!(clean_name("Ana at ana@x.com"), "Ana at ana@x.com");
        assert!(plausible("a@b") && !plausible("a") && !plausible("@b") && !plausible("a@") && !plausible("a b@c") && !plausible("a@b@c"));
        assert_eq!(probe(&words("jo lee")), Some("lee"));
        assert_eq!(probe(&words("ab cd")), Some("ab"), "the first of the longest");
        assert_eq!(probe(&[]), None);
    }

    #[test]
    fn message_hashes_are_stable() {
        // Stored: a change here would count every cached message's people again.
        assert_eq!(message_hash("<a@b>"), message_hash(" <a@b> "));
        assert_eq!(message_hash(""), 0xcbf2_9ce4_8422_2325_u64 as i64);
        assert_eq!(message_hash("a") as u64, 0xaf63_dc4c_8601_ec8c, "FNV-1a's published value for \"a\"");
    }
}
