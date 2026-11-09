//! Formatting helpers: dates, sizes, names.

use chrono::{Datelike, Local, TimeZone};

pub fn list_date(ts: i64, format: &str) -> String {
    let Some(d) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let now = Local::now();
    match format {
        "short" => d.format("%Y-%m-%d").to_string(),
        "long" => d.format("%b %-d, %Y, %-H:%M").to_string(),
        _ => {
            if d.date_naive() == now.date_naive() {
                d.format("%-H:%M").to_string()
            } else if (now.date_naive() - d.date_naive()).num_days() < 6 && d < now {
                d.format("%a").to_string()
            } else if d.year() == now.year() {
                d.format("%b %-d").to_string()
            } else {
                d.format("%b %-d, %Y").to_string()
            }
        }
    }
}

/// `1 message`, `2 messages`: a count and its noun, plural unless the count is one. For nouns
/// whose plural adds an s (all of Envoy's).
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

pub fn full_date(ts: i64) -> String {
    full_date_at(ts, Local::now())
}

/// [`full_date`] as of `now`.
fn full_date_at(ts: i64, now: chrono::DateTime<Local>) -> String {
    let Some(d) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let rel = ago(now.signed_duration_since(d));
    let base = if d.year() == now.year() { d.format("%a, %b %-d, %-H:%M").to_string() } else { d.format("%a, %b %-d, %Y, %-H:%M").to_string() };
    if rel.is_empty() { base } else { format!("{base} ({rel})") }
}

/// When a snooze ends, as its picker, toast and the Snoozed list say it: `18:00 today`,
/// `Tue 08:00` (in the week ahead), `Sat 21 Nov, 08:00`, `Mon 4 Jan 2027, 08:00`.
pub fn snooze_until(ts: i64) -> String {
    snooze_until_at(ts, &Local::now())
}

/// [`snooze_until`] as of `now`, in `now`'s zone.
fn snooze_until_at<Tz: TimeZone>(ts: i64, now: &chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(d) = now.timezone().timestamp_opt(ts, 0).single() else { return String::new() };
    let days = (d.date_naive() - now.date_naive()).num_days();
    if days == 0 {
        d.format("%H:%M today").to_string()
    } else if (1..7).contains(&days) {
        d.format("%a %H:%M").to_string()
    } else if d.year() == now.year() {
        d.format("%a %-d %b, %H:%M").to_string()
    } else {
        d.format("%a %-d %b %Y, %H:%M").to_string()
    }
}

/// How long ago, for the last week: `just now`, `5 min ago`, `1 hour ago`, `3 days ago`.
/// Empty for longer ago, or for a date in the future (a sender's clock ahead of ours).
fn ago(d: chrono::TimeDelta) -> String {
    if d.num_seconds() < 0 {
        String::new()
    } else if d.num_minutes() < 1 {
        "just now".to_owned()
    } else if d.num_minutes() < 60 {
        format!("{} min ago", d.num_minutes())
    } else if d.num_hours() < 24 {
        format!("{} ago", count(d.num_hours() as usize, "hour"))
    } else if d.num_days() < 7 {
        format!("{} ago", count(d.num_days() as usize, "day"))
    } else {
        String::new()
    }
}

pub fn size(bytes: usize) -> String {
    envoy_core::attach::size_label(bytes)
}

pub fn initial(name: &str) -> String {
    name.trim()
        .trim_matches(|c: char| c == '"' || c == '\'')
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

/// `Re: Re: Fwd: x` → `x`, for reply subjects.
pub fn base_subject(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let lower = s.to_lowercase();
        let cut = ["re:", "fwd:", "fw:", "aw:", "wg:", "sv:", "tr:"].iter().find(|p| lower.starts_with(**p)).map(|p| p.len());
        match cut {
            Some(n) => s = s[n..].trim_start(),
            None => return s,
        }
    }
}

/// Removes quoted history from a plain-text body: `>` lines and everything after an
/// "On … wrote:" line. Returns the text and whether anything was removed.
pub fn strip_quoted(text: &str) -> (String, bool) {
    let mut out = Vec::new();
    let mut cut = false;
    for line in text.lines() {
        let t = line.trim();
        if (t.starts_with("On ") && t.ends_with("wrote:")) || t == "-----Original Message-----" || t.starts_with("From: ") && cut {
            cut = true;
            break;
        }
        if t.starts_with('>') {
            cut = true;
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    (out.join("\n"), cut)
}

/// Markdown (from HTML mail) to readable plain text for quoting: emphasis markers dropped,
/// links written as `text <url>`.
pub fn markdown_to_plain(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let chars: Vec<char> = md.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' {
            // [text](url)
            if let Some(close) = chars[i..].iter().position(|&x| x == ']').map(|p| p + i)
                && chars.get(close + 1) == Some(&'(')
                && let Some(end) = chars[close..].iter().position(|&x| x == ')').map(|p| p + close)
            {
                let label: String = chars[i + 1..close].iter().collect();
                let url: String = chars[close + 2..end].iter().collect();
                if label.trim() == url.trim() || label.trim().is_empty() {
                    out.push_str(&url);
                } else {
                    out.push_str(&format!("{label} <{url}>"));
                }
                i = end + 1;
                continue;
            }
        }
        if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            i += 2;
            continue;
        }
        if c == '\\' && chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Quotes a body for a reply.
pub fn quote(text: &str) -> String {
    text.lines().map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_times_say_the_day_as_near_as_it_is() {
        let tz = chrono::FixedOffset::east_opt(3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 11, 9, 10, 17, 0).unwrap(); // a Monday
        let at = |d: u32, h: u32| tz.with_ymd_and_hms(2026, 11, d, h, 0, 0).unwrap().timestamp();
        assert_eq!(snooze_until_at(at(9, 18), &now), "18:00 today");
        assert_eq!(snooze_until_at(at(10, 8), &now), "Tue 08:00");
        assert_eq!(snooze_until_at(at(15, 8), &now), "Sun 08:00");
        assert_eq!(snooze_until_at(at(16, 8), &now), "Mon 16 Nov, 08:00", "a week on says its date");
        assert_eq!(snooze_until_at(tz.with_ymd_and_hms(2027, 1, 4, 8, 0, 0).unwrap().timestamp(), &now), "Mon 4 Jan 2027, 08:00");
    }

    #[test]
    fn subjects() {
        assert_eq!(base_subject("Re: Fwd: RE: lunch"), "lunch");
        assert_eq!(base_subject("Regarding"), "Regarding");
    }

    #[test]
    fn counts_are_singular_only_for_one() {
        assert_eq!(count(0, "message"), "0 messages");
        assert_eq!(count(1, "message"), "1 message");
        assert_eq!(count(2, "conversation"), "2 conversations");
    }

    #[test]
    fn relative_times_say_one_day_not_one_days() {
        let mins = |m: i64| chrono::TimeDelta::minutes(m);
        assert_eq!(ago(chrono::TimeDelta::seconds(20)), "just now");
        assert_eq!(ago(mins(1)), "1 min ago");
        assert_eq!(ago(mins(59)), "59 min ago");
        assert_eq!(ago(mins(60)), "1 hour ago");
        assert_eq!(ago(mins(150)), "2 hours ago");
        assert_eq!(ago(mins(24 * 60)), "1 day ago");
        assert_eq!(ago(mins(36 * 60)), "1 day ago");
        assert_eq!(ago(mins(6 * 24 * 60)), "6 days ago");
        assert_eq!(ago(mins(7 * 24 * 60)), "", "a week or more: the date says it");
        assert_eq!(ago(mins(-5)), "", "a date ahead of our clock isn't 'just now'");
    }

    #[test]
    fn full_dates_end_with_how_long_ago() {
        let now = Local.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap();
        let yesterday = Local.with_ymd_and_hms(2026, 6, 9, 11, 0, 0).unwrap().timestamp();
        assert_eq!(full_date_at(yesterday, now), "Tue, Jun 9, 11:00 (1 day ago)");
        let last_year = Local.with_ymd_and_hms(2025, 6, 10, 12, 0, 0).unwrap().timestamp();
        assert_eq!(full_date_at(last_year, now), "Tue, Jun 10, 2025, 12:00");
    }

    #[test]
    fn quoted_text_strips() {
        let (t, cut) = strip_quoted("Sure!\n\nOn Tue, Ana wrote:\n> hi");
        assert_eq!(t, "Sure!");
        assert!(cut);
    }
}
