//! Snooze: putting messages away until a time, kept on the mail server, not on any Envoy server
//! (docs/NORTH_STAR.md, "Triage").
//!
//! **Where a snooze lives is where its message is.** A snoozed message sits in a mailbox named
//! for its wake time, a child of `Snoozed` (in the account's personal namespace, with the
//! server's delimiter): `Snoozed/20261110T0800Z`, the time in UTC to the minute (ISO 8601 basic
//! form). Nothing else records it: no keyword, no METADATA, no index message. Every Envoy that
//! sees the account (and any other client, which shows the buckets as folders or labels) agrees
//! on what's snoozed and until when, from the mailbox list alone.
//!
//! - **Snoozing** creates the bucket (one that's there already is fine) and moves the message in,
//!   held for the undo window like an archive. A snooze that reaches the server within
//!   [`TOO_CLOSE`] of its time (or past it) wakes the message instead: a bucket is never moved
//!   into once its time has come, so no message lands in one that's already been woken.
//! - **Waking**, by every Envoy that's running: each bucket whose time has come is emptied into
//!   the inbox, its messages marked unread (and `$new`, where the server keeps keywords), with
//!   UID MOVE; the same moves by two Envoys at once just find the messages gone. A server without
//!   MOVE gets COPY and \Deleted (expunged only with UIDPLUS), each Envoy waiting up to
//!   [`MAX_JITTER`] at random first, so two rarely copy the same messages; when they do, the
//!   inbox keeps both copies (nothing there is deleted on a guess), and the list shows one.
//! - **Cleanup**: only on Gmail, where DELETE removes a label and never mail: a bucket a week
//!   past its time ([`KEEP_EMPTY`]), once woken, when the server says it's empty right before.
//!   Elsewhere DELETE takes a mailbox's messages, and one moved in between the STATUS and the
//!   DELETE would go: buckets stay there, empty, and once woken and past the time a late message
//!   could come, they're left alone (not looked into, synced or counted).
//! - If the server already has a `Snoozed` mailbox of its own (`\Snoozed`, RFC 9979's
//!   server-managed snoozes), or won't create one, the parent is `Envoy Snoozed` instead; buckets
//!   under either are snoozes. The server's own is shown, never written into.
//!
//! This module is the part that needs no server: names, the parent's choice, the rules' times,
//! and the times a user picks (presets, and typed ones like "fri 9am"), in their own time zone.

use chrono::{DateTime, Datelike, MappedLocalTime, NaiveDate, NaiveTime, TimeDelta, TimeZone, Timelike, Utc, Weekday};

/// The parent of Envoy's buckets, literally (never translated: every client must agree on it).
pub const PARENT: &str = "Snoozed";
/// The parent when `Snoozed` is the server's own or can't be created.
pub const FALLBACK_PARENT: &str = "Envoy Snoozed";
/// A snooze reaching the server this many seconds or fewer before its time wakes instead.
pub const TOO_CLOSE: i64 = 60;
/// An empty bucket is deleted (on Gmail only) no sooner than this many seconds after its time.
pub const KEEP_EMPTY: i64 = 7 * 24 * 3600;
/// Without MOVE, each Envoy waits up to this many seconds (at random) before waking a bucket,
/// so two of them rarely copy the same messages at once.
pub const MAX_JITTER: i64 = 30;
/// The keyword a woken message gets, where the server keeps keywords ([`crate::model::flags::WOKEN`]).
pub const NEW_KEYWORD: &str = "$new";

/// A wake time as a bucket's name: `20261110T0800Z` (UTC, to the minute; seconds dropped).
pub fn stamp(wake: i64) -> String {
    let t = DateTime::<Utc>::from_timestamp(wake.div_euclid(60) * 60, 0).unwrap_or_default();
    format!("{:04}{:02}{:02}T{:02}{:02}Z", t.year(), t.month(), t.day(), t.hour(), t.minute())
}

/// The time a bucket's last name part stands for, if it's exactly a [`stamp`].
pub fn parse_stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let shaped = b.len() == 14 && b[8] == b'T' && b[13] == b'Z' && b[..8].iter().chain(&b[9..13]).all(u8::is_ascii_digit);
    if !shaped {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let date = NaiveDate::from_ymd_opt(n(0..4)? as i32, n(4..6)?, n(6..8)?)?;
    let time = NaiveTime::from_hms_opt(n(9..11)?, n(11..13)?, 0)?;
    Some(date.and_time(time).and_utc().timestamp())
}

/// The bucket for `wake` under `parent`.
pub fn bucket(parent: &str, delimiter: &str, wake: i64) -> String {
    format!("{parent}{delimiter}{}", stamp(wake))
}

/// The two parents buckets may be under, in the personal namespace `prefix`: `Snoozed` and
/// `Envoy Snoozed`.
pub fn parents(prefix: &str) -> [String; 2] {
    [format!("{prefix}{PARENT}"), format!("{prefix}{FALLBACK_PARENT}")]
}

/// The personal namespace's prefix in a parent's name (`INBOX.` of `INBOX.Snoozed`).
pub fn prefix_of(parent: &str) -> &str {
    parent.strip_suffix(FALLBACK_PARENT).or_else(|| parent.strip_suffix(PARENT)).unwrap_or("")
}

/// If mailbox `name` is a bucket, the parent it's under and its wake time: exactly one of
/// `parents`, then the server's `delimiter`, then a [`stamp`]. Nothing else is one, however it
/// looks (`Snoozed-20261110T0800Z`, or `Work/Snoozed/…`).
pub fn bucket_wake<'a>(name: &str, parents: &'a [String; 2], delimiter: &str) -> Option<(&'a str, i64)> {
    if delimiter.is_empty() {
        return None;
    }
    parents.iter().find_map(|p| Some((p.as_str(), parse_stamp(name.strip_prefix(p.as_str())?.strip_prefix(delimiter)?)?)))
}

/// A mailbox as LIST named it, for [`choose_parent`].
#[derive(Clone, Debug)]
pub struct Listed<'a> {
    pub name: &'a str,
    pub delimiter: &'a str,
    /// It has the `\Snoozed` attribute: the server's own snoozes.
    pub native: bool,
}

/// Where an account's buckets go.
#[derive(Clone, Debug, PartialEq)]
pub struct Parent {
    pub name: String,
    pub delimiter: String,
    /// The server's own snoozed mailbox (`\Snoozed`), if it has one.
    pub native: Option<String>,
}

/// The personal namespace's prefix, from the mailbox list: `INBOX.` (or `INBOX/`) when every
/// mailbox but INBOX is under it, as on servers that keep folders inside INBOX; otherwise none.
/// (NAMESPACE would say, but isn't something the IMAP parser Envoy uses reads.)
pub fn personal_prefix(listed: &[Listed]) -> String {
    let Some(inbox) = listed.iter().find(|m| m.name.eq_ignore_ascii_case("INBOX")) else { return String::new() };
    if inbox.delimiter.is_empty() {
        return String::new();
    }
    let prefix = format!("INBOX{}", inbox.delimiter);
    let others: Vec<&Listed> = listed.iter().filter(|m| !m.name.eq_ignore_ascii_case("INBOX")).collect();
    let under = |m: &&Listed| m.name.len() > prefix.len() && m.name.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(&prefix));
    match others.first() {
        Some(first) if others.iter().all(under) => first.name[..prefix.len()].to_owned(),
        _ => String::new(),
    }
}

/// The account's bucket parent, from what LIST said: the same answer for every Envoy that sees
/// the same list. `Snoozed` in the personal namespace, unless the server's own snoozed mailbox
/// has that name, or only `Envoy Snoozed` is there (`Snoozed` couldn't be created).
pub fn choose_parent(listed: &[Listed]) -> Parent {
    let prefix = personal_prefix(listed);
    let delimiter = listed.iter().find(|m| m.name.eq_ignore_ascii_case("INBOX")).map(|m| m.delimiter).filter(|d| !d.is_empty()).unwrap_or("/").to_owned();
    let (main, fallback) = (format!("{prefix}{PARENT}"), format!("{prefix}{FALLBACK_PARENT}"));
    let native = listed.iter().find(|m| m.native).map(|m| m.name.to_owned());
    let found = |name: &str| listed.iter().find(|m| m.name == name);
    let name = match (found(&main), found(&fallback)) {
        (Some(m), _) if m.native => fallback,
        (Some(_), _) => main,
        (None, Some(_)) => fallback,
        (None, None) => main,
    };
    Parent { name, delimiter, native }
}

/// The parent to use instead when `parent` can't be created: `Envoy Snoozed` beside it. None
/// when it's that already.
pub fn fallback_of(parent: &str) -> Option<String> {
    if parent.ends_with(FALLBACK_PARENT) {
        return None;
    }
    parent.strip_suffix(PARENT).map(|prefix| format!("{prefix}{FALLBACK_PARENT}"))
}

/// A snooze reaching the server at `now`: true when its time is too close (or past) to move the
/// message into a bucket, and it wakes where it is instead.
pub fn wakes_instead(wake: i64, now: i64) -> bool {
    wake <= now + TOO_CLOSE
}

/// A bucket is old enough to delete (on Gmail, if it's empty): a week past its time.
pub fn old_enough(wake: i64, now: i64) -> bool {
    now >= wake + KEEP_EMPTY
}

/// This Envoy's wait before waking a bucket on a server without MOVE: 0 to [`MAX_JITTER`]
/// seconds, at random.
pub fn jitter() -> i64 {
    rand::random_range(0..=MAX_JITTER)
}

// ── Times a user picks ──

/// One of the snooze picker's ready-made times.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    /// Unix seconds.
    pub at: i64,
}

/// What mornings and evenings mean here.
const MORNING: (u32, u32) = (8, 0);
const EVENING: (u32, u32) = (18, 0);

/// The local time `day` at `time` in `tz`. A time a clock change skips (spring forward) is the
/// first minute after the gap; one it repeats (fall back) is the first of the two.
pub fn local<Tz: TimeZone>(tz: &Tz, day: NaiveDate, time: NaiveTime) -> Option<DateTime<Tz>> {
    let mut naive = day.and_time(time);
    // Gaps are an hour, rarely two; three is plenty.
    for _ in 0..=180 {
        match tz.from_local_datetime(&naive) {
            MappedLocalTime::Single(t) => return Some(t),
            MappedLocalTime::Ambiguous(earlier, _) => return Some(earlier),
            MappedLocalTime::None => naive += TimeDelta::minutes(1),
        }
    }
    None
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap_or(NaiveTime::MIN)
}

/// The first `weekday` after `day` (a week on, if `day` is one).
fn next_weekday(day: NaiveDate, weekday: Weekday) -> NaiveDate {
    let ahead = (weekday.num_days_from_monday() as i64 - day.weekday().num_days_from_monday() as i64).rem_euclid(7);
    day + TimeDelta::days(if ahead == 0 { 7 } else { ahead })
}

/// The picker's times as of `now`, in `now`'s time zone: Later today (three hours on, to the
/// nearest hour, while that's still today), Tonight (18:00, while it's before), Tomorrow (08:00),
/// This weekend (Saturday 08:00, on weekdays) and Next week (Monday 08:00). A time already
/// offered isn't offered twice (on a Friday, This weekend is Tomorrow).
pub fn presets<Tz: TimeZone>(now: &DateTime<Tz>) -> Vec<Preset> {
    let tz = now.timezone();
    let today = now.date_naive();
    let mut out: Vec<Preset> = Vec::new();
    let mut offer = |id, label, at: Option<DateTime<Tz>>| {
        if let Some(at) = at.map(|t| t.timestamp())
            && at > now.timestamp() + TOO_CLOSE
            && !out.iter().any(|p| p.at == at)
        {
            out.push(Preset { id, label, at });
        }
    };
    let later = (now.clone() + TimeDelta::hours(3)).naive_local();
    let rounded = later.date().and_time(hm(later.hour(), 0)) + TimeDelta::hours(i64::from(later.minute() >= 30));
    offer("later", "Later today", local(&tz, rounded.date(), rounded.time()).filter(|t| t.date_naive() == today));
    offer("tonight", "Tonight", local(&tz, today, hm(EVENING.0, EVENING.1)));
    offer("tomorrow", "Tomorrow", local(&tz, today + TimeDelta::days(1), hm(MORNING.0, MORNING.1)));
    if !matches!(today.weekday(), Weekday::Sat | Weekday::Sun) {
        offer("weekend", "This weekend", local(&tz, next_weekday(today, Weekday::Sat), hm(MORNING.0, MORNING.1)));
    }
    offer("nextweek", "Next week", local(&tz, next_weekday(today, Weekday::Mon), hm(MORNING.0, MORNING.1)));
    out
}

/// A time typed into the picker, read as of `now` in `now`'s time zone: a day and a time in
/// either order ("fri 9am", "9am fri", "tomorrow at 17:30", "dec 3", "3 december 2027",
/// "2026-12-03 09:00"), a time alone (today, or tomorrow once it's passed: "noon", "9:30pm"), a
/// day alone (at 08:00: "monday", "next week", "weekend"), or a while ("in 2 hours", "30m",
/// "in 3 days"; days and weeks land at 08:00). Words it doesn't know make it nothing, rather
/// than a guess. Only a time more than [`TOO_CLOSE`] ahead counts. Unix seconds, to the minute.
pub fn parse_when<Tz: TimeZone>(input: &str, now: &DateTime<Tz>) -> Option<i64> {
    let lower = input.trim().to_lowercase().replace(',', " ");
    let mut tokens: Vec<String> = Vec::new();
    for raw in lower.split_whitespace() {
        // ISO date and time in one word.
        match raw.split_once('t') {
            Some((d, t)) if parse_iso_date(d).is_some() && !t.is_empty() => {
                tokens.push(d.to_owned());
                tokens.push(t.to_owned());
            }
            _ => tokens.push(raw.to_owned()),
        }
    }
    if tokens.is_empty() {
        return None;
    }
    let tz = now.timezone();
    let today = now.date_naive();
    let mut day: Option<Day> = None;
    let mut time: Option<NaiveTime> = None;
    let mut default_time: Option<NaiveTime> = None;
    let mut offset: Option<TimeDelta> = None;
    let mut month_day: Option<(u32, Option<u32>)> = None;
    let mut year: Option<i32> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        match t {
            "at" | "on" | "until" | "till" | "this" | "the" | "for" => {}
            "next" => {
                if next == Some("week") {
                    put(&mut day, Day::Date(next_weekday(today, Weekday::Mon)))?;
                    i += 1;
                }
                // "next friday" is the coming Friday, as "friday" is.
            }
            "today" => put(&mut day, Day::Date(today))?,
            "tonight" => {
                put(&mut day, Day::Date(today))?;
                default_time = Some(hm(EVENING.0, EVENING.1));
            }
            "tomorrow" | "tmrw" | "tmr" | "tomorow" => put(&mut day, Day::Date(today + TimeDelta::days(1)))?,
            "weekend" => put(&mut day, Day::Date(next_weekday(today, Weekday::Sat)))?,
            "noon" | "midday" => time = Some(hm(12, 0)),
            "midnight" => time = Some(hm(23, 59)),
            "morning" => default_time = Some(hm(MORNING.0, MORNING.1)),
            "afternoon" => default_time = Some(hm(14, 0)),
            "evening" => default_time = Some(hm(EVENING.0, EVENING.1)),
            "night" => default_time = Some(hm(20, 0)),
            "in" => {
                let (amount, used) = amount(&tokens[i + 1..])?;
                if offset.is_some() || day.is_some() {
                    return None;
                }
                match amount {
                    Amount::Exact(d) => offset = Some(d),
                    Amount::Days(n) => day = Some(Day::Date(today + TimeDelta::days(n))),
                }
                i += used;
            }
            _ => {
                if let Some(w) = weekday(t) {
                    put(&mut day, Day::Weekday(w))?;
                } else if let Some(m) = month(t) {
                    if month_day.is_some() {
                        return None;
                    }
                    // "dec 3", or "3 dec" with the day already read.
                    match next.and_then(day_number) {
                        Some(d) => {
                            month_day = Some((m, Some(d)));
                            i += 1;
                        }
                        None => month_day = Some((m, None)),
                    }
                } else if let Some(d) = parse_iso_date(t) {
                    put(&mut day, Day::Date(d))?;
                } else if let Some(at) = clock(t, next) {
                    if time.is_some() {
                        return None;
                    }
                    time = Some(at.0);
                    i += at.1 - 1;
                } else if let Some(y) = t.parse::<i32>().ok().filter(|y| (2000..=2100).contains(y)) {
                    year = Some(y);
                } else if let Some((amount, used)) = amount(&tokens[i..]) {
                    // A while without "in": "2 hours", "30m".
                    if offset.is_some() || day.is_some() {
                        return None;
                    }
                    match amount {
                        Amount::Exact(d) => offset = Some(d),
                        Amount::Days(n) => day = Some(Day::Date(today + TimeDelta::days(n))),
                    }
                    i += used - 1;
                } else {
                    // A day of the month before its month ("3 dec"), or a word it doesn't know.
                    let d = day_number(t)?;
                    let m = next.and_then(month).filter(|_| month_day.is_none())?;
                    month_day = Some((m, Some(d)));
                    i += 1;
                }
            }
        }
        i += 1;
    }
    // A month and day without a year: this year's, or next year's once it's passed.
    let mut next_year: Option<NaiveDate> = None;
    if let Some((m, d)) = month_day {
        let d = d?;
        if day.is_some() {
            return None;
        }
        let date = NaiveDate::from_ymd_opt(year.unwrap_or(today.year()), m, d)?;
        if year.is_none() {
            next_year = NaiveDate::from_ymd_opt(today.year() + 1, m, d);
        }
        day = Some(Day::Date(date));
    } else if year.is_some() {
        return None;
    }
    let floor = |t: i64| t.div_euclid(60) * 60;
    let ahead = |t: i64| (t > now.timestamp() + TOO_CLOSE).then_some(t);
    if let Some(d) = offset {
        if time.is_some() || default_time.is_some() || day.is_some() {
            return None;
        }
        return ahead(floor((now.clone() + d).timestamp()));
    }
    let at = |date: NaiveDate, t: NaiveTime| local(&tz, date, t).map(|t| floor(t.timestamp()));
    match (day, time.or(default_time)) {
        (Some(Day::Date(date)), t) => {
            let t = t.unwrap_or(hm(MORNING.0, MORNING.1));
            match (ahead(at(date, t)?), next_year) {
                (Some(found), _) => Some(found),
                (None, Some(later)) => ahead(at(later, t)?),
                (None, None) => None,
            }
        }
        (Some(Day::Weekday(w)), t) => {
            // This week's, today included, unless it's passed: then a week on.
            let ahead_days = (w.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64).rem_euclid(7);
            let t = t.unwrap_or(hm(MORNING.0, MORNING.1));
            let first = at(today + TimeDelta::days(ahead_days), t)?;
            match ahead(first) {
                Some(found) => Some(found),
                None => ahead(at(today + TimeDelta::days(ahead_days + 7), t)?),
            }
        }
        (None, Some(t)) => match ahead(at(today, t)?) {
            Some(found) => Some(found),
            None => ahead(at(today + TimeDelta::days(1), t)?),
        },
        (None, None) => None,
    }
}

/// Fills `slot` with `d`, unless it's filled already ("fri tue"): then nothing.
fn put(slot: &mut Option<Day>, d: Day) -> Option<()> {
    if slot.is_some() {
        return None;
    }
    *slot = Some(d);
    Some(())
}

#[derive(Clone, Copy)]
enum Day {
    Date(NaiveDate),
    Weekday(Weekday),
}

enum Amount {
    /// Minutes or hours from now.
    Exact(TimeDelta),
    /// Days (or weeks) from today, at a time of day.
    Days(i64),
}

/// "2 hours", "an hour", "30m", "3days": how long, and how many tokens it took.
fn amount(tokens: &[String]) -> Option<(Amount, usize)> {
    let first = tokens.first()?;
    let (n, unit, used) = match first.as_str() {
        "a" | "an" | "one" => (1, tokens.get(1)?.as_str(), 2),
        w => {
            let digits: String = w.chars().take_while(char::is_ascii_digit).collect();
            let n: i64 = digits.parse().ok()?;
            match &w[digits.len()..] {
                "" => (n, tokens.get(1)?.as_str(), 2),
                rest => (n, rest, 1),
            }
        }
    };
    if !(1..=10_000).contains(&n) {
        return None;
    }
    let amount = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => Amount::Exact(TimeDelta::minutes(n)),
        "h" | "hr" | "hrs" | "hour" | "hours" => Amount::Exact(TimeDelta::hours(n)),
        "d" | "day" | "days" => Amount::Days(n),
        "w" | "wk" | "wks" | "week" | "weeks" => Amount::Days(n * 7),
        _ => return None,
    };
    Some((amount, used))
}

fn weekday(t: &str) -> Option<Weekday> {
    Some(match t {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month(t: &str) -> Option<u32> {
    const NAMES: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    let t = t.trim_end_matches('.');
    if t.len() < 3 {
        return None;
    }
    NAMES.iter().position(|n| n.starts_with(t) && (t.len() >= 3) && (t.len() == n.len() || t.len() <= 4)).map(|i| i as u32 + 1)
}

/// "3", "3rd", "21st": a day of the month.
fn day_number(t: &str) -> Option<u32> {
    let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &t[digits.len()..];
    if !matches!(suffix, "" | "st" | "nd" | "rd" | "th") || digits.is_empty() || digits.len() > 2 {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

/// `2026-12-03`.
fn parse_iso_date(t: &str) -> Option<NaiveDate> {
    let b = t.as_bytes();
    let shaped = b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !shaped {
        return None;
    }
    NaiveDate::from_ymd_opt(t[..4].parse().ok()?, t[5..7].parse().ok()?, t[8..].parse().ok()?)
}

/// A time of day ("9am", "9:30pm", "17:30", "9 pm"), and how many tokens it took (`next` is
/// the token after, for a separate "am" or "pm").
fn clock(t: &str, next: Option<&str>) -> Option<(NaiveTime, usize)> {
    let (body, half, used) = if let Some(b) = t.strip_suffix("am").or_else(|| t.strip_suffix("a")) {
        (b, Some(false), 1)
    } else if let Some(b) = t.strip_suffix("pm").or_else(|| t.strip_suffix("p")) {
        (b, Some(true), 1)
    } else {
        match next {
            Some("am" | "a.m.") => (t, Some(false), 2),
            Some("pm" | "p.m.") => (t, Some(true), 2),
            _ => (t, None, 1),
        }
    };
    let (h, m) = match body.split_once([':', '.']) {
        Some((h, m)) if m.len() == 2 => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        Some(_) => return None,
        // A bare number is a time only with am or pm ("9" alone could be anything).
        None if half.is_some() && !body.is_empty() && body.len() <= 2 => (body.parse::<u32>().ok()?, 0),
        None => return None,
    };
    if !body.chars().all(|c| c.is_ascii_digit() || c == ':' || c == '.') || m > 59 {
        return None;
    }
    let h = match half {
        Some(pm) if (1..=12).contains(&h) => h % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if h <= 23 => h,
        None => return None,
    };
    Some((hm(h, m), used))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};

    /// A zone with a clock change each way, like Central Europe in 2026: UTC+1, and UTC+2 from
    /// 29 March 02:00 (clocks jump to 03:00) to 25 October 03:00 (clocks go back to 02:00).
    #[derive(Clone, Copy, Debug)]
    pub struct Zurich;

    const SPRING: i64 = 1_774_746_000; // 2026-03-29T01:00:00Z
    const FALL: i64 = 1_792_890_000; // 2026-10-25T01:00:00Z

    impl Zurich {
        fn offset_at(ts: i64) -> FixedOffset {
            FixedOffset::east_opt(if (SPRING..FALL).contains(&ts) { 7200 } else { 3600 }).unwrap()
        }
    }

    impl TimeZone for Zurich {
        type Offset = FixedOffset;
        fn from_offset(_: &FixedOffset) -> Zurich {
            Zurich
        }
        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(hm(12, 0)))
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> MappedLocalTime<FixedOffset> {
            // Each offset this zone has, kept if the instant it gives has that offset.
            let mut fits: Vec<(i64, FixedOffset)> = [7200, 3600]
                .into_iter()
                .map(|s| FixedOffset::east_opt(s).unwrap())
                .map(|o| ((*local - TimeDelta::seconds(i64::from(o.local_minus_utc()))).and_utc().timestamp(), o))
                .filter(|(ts, o)| Zurich::offset_at(*ts) == *o)
                .collect();
            fits.sort_by_key(|(ts, _)| *ts);
            match fits.as_slice() {
                [] => MappedLocalTime::None,
                [(_, o)] => MappedLocalTime::Single(*o),
                [(_, a), (_, b), ..] => MappedLocalTime::Ambiguous(*a, *b),
            }
        }
        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            Zurich::offset_at(utc.and_time(NaiveTime::MIN).and_utc().timestamp())
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Zurich::offset_at(utc.and_utc().timestamp())
        }
    }

    /// Zurich time, as a moment.
    pub fn zurich(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Zurich> {
        local(&Zurich, NaiveDate::from_ymd_opt(y, mo, d).unwrap(), hm(h, mi)).unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_time(hm(h, mi)).and_utc().timestamp()
    }

    #[test]
    fn a_bucket_is_named_for_its_wake_time_in_utc_to_the_minute() {
        assert_eq!(stamp(utc(2026, 11, 10, 8, 0)), "20261110T0800Z");
        assert_eq!(stamp(utc(2026, 11, 10, 8, 0) + 59), "20261110T0800Z", "seconds dropped");
        assert_eq!(parse_stamp("20261110T0800Z"), Some(utc(2026, 11, 10, 8, 0)));
        assert_eq!(bucket("Snoozed", "/", utc(2026, 11, 10, 8, 0)), "Snoozed/20261110T0800Z");
        assert_eq!(bucket("INBOX.Snoozed", ".", utc(2026, 1, 2, 23, 5)), "INBOX.Snoozed.20260102T2305Z");
        // Exactly the shape, nothing looser.
        for bad in ["20261110T0800", "20261110T080Z", "2026111T0800Zz", "20261310T0800Z", "20261110T2460Z", "2026-11-10T08:00Z", "20261110t0800z", "２0261110T0800Z"] {
            assert_eq!(parse_stamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_bucket_is_a_stamp_under_a_parent_by_the_servers_delimiter() {
        let at = utc(2026, 11, 10, 8, 0);
        let root = parents("");
        assert_eq!(bucket_wake("Snoozed/20261110T0800Z", &root, "/"), Some(("Snoozed", at)));
        assert_eq!(bucket_wake("Envoy Snoozed/20261110T0800Z", &root, "/"), Some(("Envoy Snoozed", at)));
        assert_eq!(bucket_wake("Snoozed.20261110T0800Z", &root, "."), Some(("Snoozed", at)));
        let inside = parents(prefix_of("INBOX.Snoozed"));
        assert_eq!(bucket_wake("INBOX.Envoy Snoozed.20261110T0800Z", &inside, "."), Some(("INBOX.Envoy Snoozed", at)));
        for (name, delimiter) in [
            ("Snoozed-20261110T0800Z", "/"),
            ("Snoozed.20261110T0800Z", "/"),
            ("Snoozed/20261110T0800Z", "."),
            ("Work/Snoozed/20261110T0800Z", "/"),
            ("MySnoozed/20261110T0800Z", "/"),
            ("Snoozed/20261110T0800Z/x", "/"),
            ("Snoozed/", "/"),
            ("Snoozed", "/"),
            ("Snoozed20261110T0800Z", ""),
        ] {
            assert_eq!(bucket_wake(name, &root, delimiter), None, "{name:?} on a {delimiter:?} server");
        }
        assert_eq!(bucket_wake("INBOX.Snoozed.20261110T0800Z", &root, "."), None, "not in this namespace");
        assert_eq!(bucket_wake("Snoozed.20261110T0800Z", &inside, "."), None);
        assert_eq!((prefix_of("INBOX.Snoozed"), prefix_of("INBOX/Envoy Snoozed"), prefix_of("Snoozed")), ("INBOX.", "INBOX/", ""));
    }

    fn listed<'a>(names: &[(&'a str, bool)], delimiter: &'a str) -> Vec<Listed<'a>> {
        names.iter().map(|(name, native)| Listed { name, delimiter, native: *native }).collect()
    }

    #[test]
    fn the_parent_is_snoozed_in_the_personal_namespace() {
        let plain = listed(&[("INBOX", false), ("Sent", false), ("Trash", false)], "/");
        assert_eq!(choose_parent(&plain), Parent { name: "Snoozed".into(), delimiter: "/".into(), native: None });
        // Folders kept inside INBOX (Courier, some Dovecots): the parent is too.
        let inside = listed(&[("INBOX", false), ("INBOX.Sent", false), ("INBOX.Drafts", false)], ".");
        assert_eq!(personal_prefix(&inside), "INBOX.");
        assert_eq!(choose_parent(&inside), Parent { name: "INBOX.Snoozed".into(), delimiter: ".".into(), native: None });
        // Some folders inside INBOX and some not: no prefix.
        let mixed = listed(&[("INBOX", false), ("INBOX/Work", false), ("Sent", false)], "/");
        assert_eq!(personal_prefix(&mixed), "");
        // Gmail.
        let gmail = listed(&[("INBOX", false), ("[Gmail]", false), ("[Gmail]/Sent Mail", false), ("Snoozed", false), ("Snoozed/20261110T0800Z", false)], "/");
        assert_eq!(choose_parent(&gmail).name, "Snoozed");
    }

    #[test]
    fn the_servers_own_snoozed_mailbox_or_one_it_wont_make_moves_the_parent_aside() {
        // RFC 9979's server-managed Snoozed: never written into.
        let native = listed(&[("INBOX", false), ("Snoozed", true)], "/");
        assert_eq!(choose_parent(&native), Parent { name: "Envoy Snoozed".into(), delimiter: "/".into(), native: Some("Snoozed".into()) });
        // Only Envoy Snoozed there: Snoozed was refused before, by this Envoy or another.
        let refused = listed(&[("INBOX", false), ("Envoy Snoozed", false), ("Envoy Snoozed/20261110T0800Z", false)], "/");
        assert_eq!(choose_parent(&refused).name, "Envoy Snoozed");
        // A Snoozed of its own elsewhere doesn't take the name.
        let elsewhere = listed(&[("INBOX", false), ("Snoozed Items", true)], "/");
        assert_eq!(choose_parent(&elsewhere), Parent { name: "Snoozed".into(), delimiter: "/".into(), native: Some("Snoozed Items".into()) });
        assert_eq!(fallback_of("Snoozed").as_deref(), Some("Envoy Snoozed"));
        assert_eq!(fallback_of("INBOX.Snoozed").as_deref(), Some("INBOX.Envoy Snoozed"));
        assert_eq!(fallback_of("Envoy Snoozed"), None);
        assert_eq!(fallback_of("INBOX.Envoy Snoozed"), None);
    }

    #[test]
    fn a_snooze_never_lands_in_a_bucket_whose_time_has_come() {
        let now = 1_000_000;
        assert!(!wakes_instead(now + TOO_CLOSE + 1, now));
        assert!(wakes_instead(now + TOO_CLOSE, now));
        assert!(wakes_instead(now, now));
        assert!(wakes_instead(now - 3600, now));
    }

    #[test]
    fn buckets_are_old_enough_to_delete_a_week_on() {
        let wake = 1_000_000;
        assert!(!old_enough(wake, wake + KEEP_EMPTY - 1));
        assert!(old_enough(wake, wake + KEEP_EMPTY));
    }

    #[test]
    fn jitter_stays_within_its_bounds() {
        let draws: Vec<i64> = (0..2000).map(|_| jitter()).collect();
        assert!(draws.iter().all(|j| (0..=MAX_JITTER).contains(j)));
        assert!(draws.iter().any(|j| *j < 5) && draws.iter().any(|j| *j > MAX_JITTER - 5), "spread over the range");
    }

    fn at(p: &[Preset], id: &str) -> Option<i64> {
        p.iter().find(|p| p.id == id).map(|p| p.at)
    }

    #[test]
    fn presets_on_a_monday_morning() {
        // Monday 9 November 2026, 10:17 in Zurich (UTC+1).
        let p = presets(&zurich(2026, 11, 9, 10, 17));
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), ["later", "tonight", "tomorrow", "weekend", "nextweek"]);
        assert_eq!(at(&p, "later"), Some(utc(2026, 11, 9, 12, 0)), "13:17 → 13:00 local");
        assert_eq!(at(&p, "tonight"), Some(utc(2026, 11, 9, 17, 0)));
        assert_eq!(at(&p, "tomorrow"), Some(utc(2026, 11, 10, 7, 0)));
        assert_eq!(at(&p, "weekend"), Some(utc(2026, 11, 14, 7, 0)));
        assert_eq!(at(&p, "nextweek"), Some(utc(2026, 11, 16, 7, 0)));
        // 10:45: three hours on is 13:45, to the nearest hour 14:00.
        assert_eq!(at(&presets(&zurich(2026, 11, 9, 10, 45)), "later"), Some(utc(2026, 11, 9, 13, 0)));
    }

    #[test]
    fn presets_late_in_the_day_and_week() {
        // 20:40: later today would be tomorrow, and tonight has passed.
        let p = presets(&zurich(2026, 11, 9, 20, 40));
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), ["tomorrow", "weekend", "nextweek"]);
        // 15:10: later today is 18:00, tonight too: offered once.
        let p = presets(&zurich(2026, 11, 9, 15, 10));
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), ["later", "tomorrow", "weekend", "nextweek"]);
        // Friday night: this weekend is tomorrow.
        let p = presets(&zurich(2026, 11, 13, 21, 30));
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), ["tomorrow", "nextweek"]);
        // Saturday: no weekend to snooze to; next week is Monday.
        let p = presets(&zurich(2026, 11, 14, 9, 0));
        assert!(at(&p, "weekend").is_none());
        assert_eq!(at(&p, "nextweek"), Some(utc(2026, 11, 16, 7, 0)));
        // Monday: next week is a week on.
        assert_eq!(at(&presets(&zurich(2026, 11, 16, 9, 0)), "nextweek"), Some(utc(2026, 11, 23, 7, 0)));
    }

    #[test]
    fn presets_across_a_clock_change_keep_their_local_time() {
        // Saturday 28 March 2026, 10:00 (UTC+1); the clocks go forward that night.
        let p = presets(&zurich(2026, 3, 28, 10, 0));
        assert_eq!(at(&p, "tomorrow"), Some(utc(2026, 3, 29, 6, 0)), "08:00 local on the 29th is UTC+2");
        assert_eq!(at(&p, "nextweek"), Some(utc(2026, 3, 30, 6, 0)));
        // Saturday 24 October, 23:30 (UTC+2); they go back that night.
        let p = presets(&zurich(2026, 10, 24, 23, 30));
        assert_eq!(at(&p, "tomorrow"), Some(utc(2026, 10, 25, 7, 0)), "08:00 local on the 25th is UTC+1");
    }

    fn when(s: &str, now: &DateTime<Zurich>) -> Option<i64> {
        parse_when(s, now)
    }

    #[test]
    fn typed_times_read_as_a_person_means_them() {
        let now = zurich(2026, 11, 9, 10, 17); // Monday
        let cases: &[(&str, i64)] = &[
            ("fri 9am", utc(2026, 11, 13, 8, 0)),
            ("9am fri", utc(2026, 11, 13, 8, 0)),
            ("Friday at 9:30 AM", utc(2026, 11, 13, 8, 30)),
            ("next fri", utc(2026, 11, 13, 7, 0)),
            ("tomorrow 5pm", utc(2026, 11, 10, 16, 0)),
            ("tomorrow at 17:30", utc(2026, 11, 10, 16, 30)),
            ("tmrw", utc(2026, 11, 10, 7, 0)),
            ("tomorrow evening", utc(2026, 11, 10, 17, 0)),
            ("tonight", utc(2026, 11, 9, 17, 0)),
            ("tonight 9pm", utc(2026, 11, 9, 20, 0)),
            ("noon", utc(2026, 11, 9, 11, 0)),
            ("9:30pm", utc(2026, 11, 9, 20, 30)),
            ("17:30", utc(2026, 11, 9, 16, 30)),
            ("7am", utc(2026, 11, 10, 6, 0)),
            ("mon", utc(2026, 11, 16, 7, 0)),
            ("monday 5pm", utc(2026, 11, 9, 16, 0)),
            ("in 2 days", utc(2026, 11, 11, 7, 0)),
            ("in a week", utc(2026, 11, 16, 7, 0)),
            ("in 3 hours", utc(2026, 11, 9, 12, 17)),
            ("in an hour", utc(2026, 11, 9, 10, 17)),
            ("in 30 min", utc(2026, 11, 9, 9, 47)),
            ("2h", utc(2026, 11, 9, 11, 17)),
            ("30m", utc(2026, 11, 9, 9, 47)),
            ("3 days", utc(2026, 11, 12, 7, 0)),
            ("next week", utc(2026, 11, 16, 7, 0)),
            ("weekend", utc(2026, 11, 14, 7, 0)),
            ("this weekend", utc(2026, 11, 14, 7, 0)),
            ("dec 3", utc(2026, 12, 3, 7, 0)),
            ("December 3rd 2pm", utc(2026, 12, 3, 13, 0)),
            ("3 dec 2027", utc(2027, 12, 3, 7, 0)),
            ("jan 5", utc(2027, 1, 5, 7, 0)),
            ("nov 9", utc(2027, 11, 9, 7, 0)),
            ("2026-12-03 09:00", utc(2026, 12, 3, 8, 0)),
            ("2026-12-03T09:00", utc(2026, 12, 3, 8, 0)),
            ("2026-12-03", utc(2026, 12, 3, 7, 0)),
            ("  FRI  9AM ", utc(2026, 11, 13, 8, 0)),
        ];
        for (input, want) in cases {
            assert_eq!(when(input, &now), Some(*want), "{input:?}");
        }
    }

    #[test]
    fn what_isnt_a_future_time_is_nothing() {
        let now = zurich(2026, 11, 9, 10, 17);
        for input in ["", "hello", "fri lunch", "yesterday", "in", "in 0 min", "in 1 min", "feb 30", "13pm", "25:00", "9", "at 9", "dec", "2026", "fri tue", "in 2 hours tomorrow", "2026-11-09 08:00", "today", "9:5"] {
            assert_eq!(when(input, &now), None, "{input:?}");
        }
    }

    #[test]
    fn typed_times_across_clock_changes() {
        // A time the spring change skips is the first minute after it.
        let before = zurich(2026, 3, 28, 12, 0);
        assert_eq!(when("2026-03-29 02:30", &before), Some(utc(2026, 3, 29, 1, 0)), "03:00 UTC+2");
        assert_eq!(when("sun 2:30am", &before), Some(utc(2026, 3, 29, 1, 0)));
        // A time the autumn change repeats is the first of the two.
        let autumn = zurich(2026, 10, 24, 12, 0);
        assert_eq!(when("2026-10-25 02:30", &autumn), Some(utc(2026, 10, 25, 0, 30)), "02:30 UTC+2, the first");
        // Days ahead keep their local time across a change.
        assert_eq!(when("in 2 days", &zurich(2026, 3, 27, 10, 0)), Some(utc(2026, 3, 29, 6, 0)));
        assert_eq!(when("in 3 hours", &zurich(2026, 3, 29, 0, 30)), Some(utc(2026, 3, 29, 2, 30)), "a while is a while, whatever the clocks do");
    }
}
