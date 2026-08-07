use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

use crate::{db::MonthlyLeaderboardEntry, format};

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

/// Build the mention-name map for a set of leaderboard entries: user ID →
/// sanitized display name, ready for `format::mentionify_with_names`. Entries
/// with no (or blank) display name are omitted so the caller's mention
/// pipeline falls back to its own localpart-derived label.
pub fn mention_names(entries: &[MonthlyLeaderboardEntry]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let name = format::sanitize_display_name(entry.display_name.as_deref()?)?;
            Some((entry.user_id.clone(), name))
        })
        .collect()
}

/// Render the leaderboard as plain text with each player's raw Matrix user
/// ID embedded (e.g. `🥇 @alice:example.org · 12/15 ...`) rather than a
/// display name. Callers MUST pass the result through `format::mentionify`
/// or `format::mentionify_with_names` before sending — that is what turns
/// the embedded IDs into proper mention pills (and, given a name map from
/// `mention_names`, labels them with the player's display name). This keeps
/// leaderboard output using the exact same mention path as the in-round
/// score summary instead of a second, separate formatting implementation.
pub fn leaderboard_text(
    title: &str,
    question_count: i64,
    entries: &[MonthlyLeaderboardEntry],
) -> String {
    let mut lines = vec![format!("**🏆 {title} · {question_count} Qs:**")];
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
            entry.user_id,
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
    format::mentionify_with_names(&plain, &mention_names(entries))
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

    fn entry_without_display_name(localpart: &str, correct: i64, total: i64) -> MonthlyLeaderboardEntry {
        MonthlyLeaderboardEntry {
            user_id: format!("@{localpart}:example.org"),
            display_name: None,
            total_correct: correct,
            total_questions: total,
            rounds_played: 4,
            wilson_score: crate::db::wilson_lower_bound(correct, total),
        }
    }

    /// Extract (plain_body, Option<html_body>) from a RoomMessageEventContent.
    fn bodies(c: &RoomMessageEventContent) -> (String, Option<String>) {
        match &c.msgtype {
            MessageType::Text(t) => (t.body.clone(), t.formatted.as_ref().map(|f| f.body.clone())),
            _ => panic!("unexpected msgtype"),
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
    fn leaderboard_text_embeds_raw_mxids_for_the_mention_pipeline() {
        // `leaderboard_text` is a contract for callers that route the result
        // through `format::mentionify`/`mentionify_with_names` — it must not
        // bake in display names itself, or those callers would double up on
        // (or bypass) the shared mention formatting.
        let text = leaderboard_text("All-time", 653, &[entry("Yakari", 98, 130)]);
        assert_eq!(
            text,
            "**🏆 All-time · 653 Qs:**\n\
             🥇 @yakari:example.org · 98/130 (75%) · ⭐67%"
        );
    }

    #[test]
    fn mention_names_omits_entries_without_a_usable_display_name() {
        let entries = [
            entry("Alice", 1, 1),
            entry_without_display_name("ghost", 0, 1),
        ];
        let names = mention_names(&entries);
        assert_eq!(
            names.get("@alice:example.org").map(String::as_str),
            Some("Alice")
        );
        assert!(!names.contains_key("@ghost:example.org"));
    }

    #[test]
    fn monthly_leaderboard_uses_mention_pills_with_display_names() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            42,
            &[entry("Alice", 30, 40), entry("Bob", 12, 20)],
        );
        let (plain, html) = bodies(&content);
        let html = html.expect("should have HTML body");

        // Plain body reads exactly as before — mentionify replaces each
        // embedded mxid with its display name in the plain text too.
        assert_eq!(
            plain,
            "🏆 July · 42 Qs:\n\
             🥇 Alice · 30/40 (75%) · ⭐60%\n\
             🥈 Bob · 12/20 (60%) · ⭐39%"
        );
        // HTML body carries real mention pills, not just bold text.
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
        assert!(html.contains(">Alice<"));
        assert!(html.contains(r#"href="https://matrix.to/#/@bob:example.org""#));
        assert!(html.contains(">Bob<"));
        assert!(html.contains("<strong>"), "header should still be bold");
    }

    #[test]
    fn leaderboard_falls_back_to_localpart_when_display_name_is_missing() {
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            5,
            &[entry_without_display_name("mysterious", 3, 5)],
        );
        let (plain, html) = bodies(&content);
        let html = html.expect("should have HTML body");
        assert!(plain.contains("mysterious"));
        assert!(html.contains(r#"href="https://matrix.to/#/@mysterious:example.org""#));
        assert!(html.contains(">mysterious<"));
    }

    #[test]
    fn all_time_leaderboard_reply_uses_the_same_mention_pipeline_as_monthly() {
        // Mirrors how a command reply (e.g. !scores) assembles its content:
        // `leaderboard_text` + a names map, fed into the same
        // `mentionify_with_names` helper the round score uses.
        let entries = [entry("Yakari", 98, 130)];
        let content = format::mentionify_with_names(
            &leaderboard_text("All-time", 653, &entries),
            &mention_names(&entries),
        );
        let (plain, html) = bodies(&content);
        let html = html.expect("should have HTML body");
        assert_eq!(
            plain,
            "🏆 All-time · 653 Qs:\n\
             🥇 Yakari · 98/130 (75%) · ⭐67%"
        );
        assert!(html.contains(r#"href="https://matrix.to/#/@yakari:example.org""#));
        assert!(html.contains(">Yakari<"));
    }

    #[test]
    fn display_names_are_escaped_in_html_and_sanitized_in_plain_text() {
        // A real mxid never contains whitespace/control characters — only
        // the free-text display name does, so give it a clean user_id here
        // (unlike `entry()`, which derives both from the same input).
        let messy = MonthlyLeaderboardEntry {
            user_id: "@alice:example.org".to_owned(),
            display_name: Some("<Alice & Co>\nAdmin".to_owned()),
            total_correct: 10,
            total_questions: 12,
            rounds_played: 4,
            wilson_score: crate::db::wilson_lower_bound(10, 12),
        };
        let content = monthly_content(
            YearMonth {
                year: 2026,
                month: 7,
            },
            12,
            &[messy],
        );
        let (plain, html) = bodies(&content);
        let html = html.expect("should have HTML body");
        assert!(plain.contains("<Alice & Co> Admin"));
        assert!(html.contains("&lt;Alice &amp; Co&gt; Admin"));
        assert!(!html.contains("><Alice"));
        assert!(html.contains("https://matrix.to/#/@"), "still a mention pill");
    }
}
