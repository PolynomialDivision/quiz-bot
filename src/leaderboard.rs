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

    pub fn label(self) -> String {
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
        format!("{} {}", MONTHS[(self.month - 1) as usize], self.year)
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
    entry
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&entry.user_id)
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

pub fn monthly_content(
    month: YearMonth,
    entries: &[MonthlyLeaderboardEntry],
) -> RoomMessageEventContent {
    if entries.is_empty() {
        let body = format!("📊 {} leaderboard\nNo scores this month.", month.label());
        return RoomMessageEventContent::text_html(
            body,
            format!(
                "<p><strong>📊 {} leaderboard</strong><br>No scores this month.</p>",
                escape_html(&month.label())
            ),
        );
    }

    let winning_score = entries[0].total_correct;
    let winners: Vec<_> = entries
        .iter()
        .take_while(|entry| entry.total_correct == winning_score)
        .map(safe_name)
        .collect();
    let winner_word = if winners.len() == 1 {
        "Winner"
    } else {
        "Winners"
    };
    let winner_suffix = if winners.len() == 1 { "" } else { " each" };
    let title = format!("🏆 {} {winner_word}", month.label());
    let winner_line = format!(
        "{} — {winning_score} points{winner_suffix}",
        winners.join(", ")
    );

    let mut plain = vec![title.clone(), winner_line, String::new()];
    let mut html = format!(
        "<p><strong>{}</strong><br>{} — <strong>{winning_score} points{winner_suffix}</strong></p>",
        escape_html(&title),
        escape_html(&winners.join(", ")),
    );
    let mut previous_score = None;
    let mut rank = 0;

    for (index, entry) in entries.iter().enumerate() {
        if previous_score != Some(entry.total_correct) {
            rank = index + 1;
            previous_score = Some(entry.total_correct);
        }
        let marker = match rank {
            1 => "🥇",
            2 => "🥈",
            3 => "🥉",
            _ => "▪️",
        };
        let name = safe_name(entry);
        plain.push(format!("{marker} {name}"));
        plain.push(format!(
            "{} points · {} questions · {} rounds",
            entry.total_correct, entry.total_questions, entry.rounds_played
        ));
        html.push_str(&format!(
            "<p>{marker} <strong>{}</strong><br>{} points · {} questions · {} rounds</p>",
            escape_html(&name),
            entry.total_correct,
            entry.total_questions,
            entry.rounds_played,
        ));
    }

    RoomMessageEventContent::text_html(plain.join("\n"), html)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use matrix_sdk::ruma::events::room::message::MessageType;

    use super::*;

    fn entry(name: &str, score: i64) -> MonthlyLeaderboardEntry {
        MonthlyLeaderboardEntry {
            user_id: format!("@{}:example.org", name.to_lowercase()),
            display_name: Some(name.to_owned()),
            total_correct: score,
            total_questions: 20,
            rounds_played: 4,
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
    fn tied_winners_and_mobile_lines_are_rendered() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            &[entry("Alice", 12), entry("Bob", 12), entry("Cara", 9)],
        );
        let MessageType::Text(text) = content.msgtype else {
            panic!("expected text")
        };
        assert!(text.body.contains("July 2026 Winners"));
        assert!(text.body.contains("Alice, Bob — 12 points each"));
        assert!(text.body.contains("🥇 Alice\n12 points"));
        assert!(text.body.contains("🥇 Bob\n12 points"));
        assert!(!text.body.contains('|'));
    }

    #[test]
    fn display_names_are_escaped_in_html_and_sanitized_in_plain_text() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            &[entry("<Alice & Co>\nAdmin", 12)],
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
