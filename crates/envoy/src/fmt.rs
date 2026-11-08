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

pub fn full_date(ts: i64) -> String {
    let Some(d) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let now = Local::now();
    let ago = now.signed_duration_since(d);
    let rel = if ago.num_minutes() < 1 {
        "just now".to_owned()
    } else if ago.num_minutes() < 60 {
        format!("{} min ago", ago.num_minutes())
    } else if ago.num_hours() < 24 {
        format!("{} h ago", ago.num_hours())
    } else if ago.num_days() < 7 {
        format!("{} days ago", ago.num_days())
    } else {
        String::new()
    };
    let base = if d.year() == now.year() { d.format("%a, %b %-d, %-H:%M").to_string() } else { d.format("%a, %b %-d, %Y, %-H:%M").to_string() };
    if rel.is_empty() { base } else { format!("{base} ({rel})") }
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
    fn subjects() {
        assert_eq!(base_subject("Re: Fwd: RE: lunch"), "lunch");
        assert_eq!(base_subject("Regarding"), "Regarding");
    }

    #[test]
    fn quoted_text_strips() {
        let (t, cut) = strip_quoted("Sure!\n\nOn Tue, Ana wrote:\n> hi");
        assert_eq!(t, "Sure!");
        assert!(cut);
    }
}
