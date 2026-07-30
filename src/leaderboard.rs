use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

use crate::db::MonthlyLeaderboardEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YearMonth {
    pub year: i32,
    pub month: u32,
}

impl YearMonth {
    pub fn containing(date: NaiveDate) -> Self {
        Self {
            year: date.year(),
            month: date.month(),
        }
    }

    pub fn previous(date: NaiveDate) -> Self {
        if date.month() == 1 {
            Self {
                year: date.year() - 1,
                month: 12,
            }
        } else {
            Self {
                year: date.year(),
                month: date.month() - 1,
            }
        }
    }

    pub fn period(self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }

    pub fn month_name(self) -> &'static str {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        MONTHS[(self.month - 1) as usize]
    }

    pub fn utc_bounds(self, tz: Tz) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let start = NaiveDate::from_ymd_opt(self.year, self.month, 1)
            .ok_or_else(|| anyhow!("invalid leaderboard month {}", self.period()))?;
        let (next_year, next_month) = if self.month == 12 {
            (self.year + 1, 1)
        } else {
            (self.year, self.month + 1)
        };
        let end = NaiveDate::from_ymd_opt(next_year, next_month, 1)
            .ok_or_else(|| anyhow!("invalid next leaderboard month"))?;
        Ok((local_day_start(tz, start)?, local_day_start(tz, end)?))
    }
}

fn local_day_start(tz: Tz, date: NaiveDate) -> Result<DateTime<Utc>> {
    for minute in 0..=180 {
        let local = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("invalid local month boundary"))?
            + chrono::Duration::minutes(minute);
        match tz.from_local_datetime(&local) {
            LocalResult::Single(value) => return Ok(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) => return Ok(a.min(b).with_timezone(&Utc)),
            LocalResult::None => {}
        }
    }
    Err(anyhow!(
        "could not resolve local start of {date} in timezone {tz}"
    ))
}

fn safe_name(entry: &MonthlyLeaderboardEntry) -> String {
    let fallback = entry
        .user_id
        .split(':')
        .next()
        .unwrap_or(&entry.user_id)
        .trim_start_matches('@');
    entry
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(fallback)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub fn leaderboard_text(
    title: &str,
    question_count: i64,
    entries: &[MonthlyLeaderboardEntry],
) -> String {
    let mut lines = vec![format!("🏆 {title} · {question_count} Qs:")];
    for (index, entry) in entries.iter().enumerate() {
        let marker = match index {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "▪️",
        };
        let accuracy = if entry.total_questions > 0 {
            entry.total_correct * 100 / entry.total_questions
        } else {
            0
        };
        lines.push(format!(
            "{marker} {} · {}/{} ({}%) · ⭐{:.0}%",
            safe_name(entry),
            entry.total_correct,
            entry.total_questions,
            accuracy,
            entry.wilson_score * 100.0,
        ));
    }
    lines.join("\n")
}

pub fn monthly_content(
    month: YearMonth,
    question_count: i64,
    entries: &[MonthlyLeaderboardEntry],
) -> RoomMessageEventContent {
    let plain = leaderboard_text(month.month_name(), question_count, entries);
    let mut html_lines = plain.lines();
    let header = html_lines.next().unwrap_or_default();
    let mut html = format!("<strong>{}</strong>", escape_html(header));
    for line in html_lines {
        html.push_str("<br>");
        html.push_str(&escape_html(line));
    }
    RoomMessageEventContent::text_html(plain, html)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use matrix_sdk::ruma::events::room::message::MessageType;

    use super::*;

    fn entry(name: &str, correct: i64, total: i64) -> MonthlyLeaderboardEntry {
        MonthlyLeaderboardEntry {
            user_id: format!("@{}:example.org", name.to_lowercase()),
            display_name: Some(name.to_owned()),
            total_correct: correct,
            total_questions: total,
            rounds_played: 4,
            wilson_score: crate::db::wilson_lower_bound(correct, total),
        }
    }

    #[test]
    fn previous_month_crosses_year_boundary() {
        assert_eq!(
            YearMonth::previous(NaiveDate::from_ymd_opt(2027, 1, 2).unwrap()),
            YearMonth {
                year: 2026,
                month: 12
            }
        );
        assert_eq!(
            YearMonth::previous(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()),
            YearMonth {
                year: 2026,
                month: 2
            }
        );
    }

    #[test]
    fn local_month_bounds_handle_dst_and_month_lengths() {
        let month = YearMonth {
            year: 2026,
            month: 3,
        };
        let (start, end) = month.utc_bounds(chrono_tz::Europe::Berlin).unwrap();
        assert_eq!(start.to_rfc3339(), "2026-02-28T23:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2026-03-31T22:00:00+00:00");
    }

    #[test]
    fn monthly_leaderboard_uses_compact_weighted_format() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            42,
            &[entry("Alice", 30, 40), entry("Bob", 12, 20)],
        );
        let MessageType::Text(text) = content.msgtype else {
            panic!("expected text")
        };
        assert_eq!(
            text.body,
            "🏆 July · 42 Qs:\n\
             🥇 Alice · 30/40 (75%) · ⭐60%\n\
             🥈 Bob · 12/20 (60%) · ⭐39%"
        );
    }

    #[test]
    fn all_time_leaderboard_uses_the_same_format() {
        let text = leaderboard_text("All-time", 653, &[entry("yakari", 98, 130)]);
        assert_eq!(
            text,
            "🏆 All-time · 653 Qs:\n\
             🥇 yakari · 98/130 (75%) · ⭐67%"
        );
    }

    #[test]
    fn display_names_are_escaped_in_html_and_sanitized_in_plain_text() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            12,
            &[entry("<Alice & Co>\nAdmin", 10, 12)],
        );
        let MessageType::Text(text) = content.msgtype else {
            panic!("expected text")
        };
        assert!(text.body.contains("<Alice & Co> Admin"));
        let html = &text.formatted.unwrap().body;
        assert!(html.contains("&lt;Alice &amp; Co&gt; Admin"));
        assert!(!html.contains("<Alice"));
    }
}
