use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use chrono_tz::Tz;
use matrix_sdk::{ruma::OwnedTransactionId, Client};
use mxbot_common::{matrix_sdk, supervise::Alive};
use tracing::{error, info, warn};

use crate::{config::ScheduleConfig, state::ScheduledOnce, BotContext};

/// How long after its fire moment a slot may still start. The scheduler
/// fires once the moment has passed rather than only in its exact minute,
/// so a late tick or a restart around that time doesn't lose the quiz.
const FIRE_GRACE_SECS: i64 = 5 * 60;

/// Background task: check every 20 seconds whether it's time to fire any
/// configured quiz slot.
pub async fn run(ctx: BotContext, client: Client, alive: Alive) {
    info!("Quiz scheduler started");
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // For the status file: a round should succeed every 20 seconds.
    alive.expect_every(std::time::Duration::from_secs(20));
    loop {
        interval.tick().await;
        alive.tick();
        match tick(&ctx, &client).await {
            Ok(true) => alive.ok(),
            Ok(false) => alive.idle(),
            Err(e) => {
                error!("Scheduler error: {e}");
                alive.failed(format!("scheduler round failed: {e}"));
            }
        }
        alive.next_due(next_fire(&ctx));
    }
}

/// When the next recurring quiz fires (its first reminder), for the status file.
fn next_fire(ctx: &BotContext) -> Option<std::time::SystemTime> {
    let tz: Tz = ctx.config.schedule.timezone;
    let now = chrono::Utc::now().with_timezone(&tz).naive_local();
    let offset = max_reminder(ctx);
    let fires = ctx
        .config
        .schedule
        .quiz_times
        .iter()
        .map(|t| fire_secs(t.hour, t.minute, offset));
    next_fire_after(now, fires)?
        .and_local_timezone(tz)
        .earliest()
        .map(|at| std::time::SystemTime::from(at.with_timezone(&chrono::Utc)))
}

/// The first fire moment strictly after `now`, today or tomorrow.
fn next_fire_after(now: NaiveDateTime, fires: impl Iterator<Item = i64>) -> Option<NaiveDateTime> {
    fires
        .map(|secs| {
            let today = fire_moment(now.date(), secs);
            if today > now {
                today
            } else {
                today + chrono::Duration::days(1)
            }
        })
        .min()
}

/// The earliest reminder before a quiz, in seconds: when a slot fires.
fn max_reminder(ctx: &BotContext) -> i64 {
    ctx.config
        .schedule
        .reminder_before_secs
        .iter()
        .copied()
        .max()
        .unwrap_or(0) as i64
}

/// Seconds after local midnight at which a quiz starting at `qh:qm` fires
/// (its earliest reminder), wrapping to the previous day if needed.
fn fire_secs(qh: u32, qm: u32, offset: i64) -> i64 {
    ((qh * 3600 + qm * 60) as i64 - offset).rem_euclid(86400)
}

/// The fire moment on `date`.
fn fire_moment(date: NaiveDate, fire_secs: i64) -> NaiveDateTime {
    date.and_hms_opt(0, 0, 0).expect("midnight exists") + chrono::Duration::seconds(fire_secs)
}

/// The date whose fire moment `now` falls within the grace window of, if any:
/// today's, or yesterday's for a window that crosses midnight.
fn due_fire_date(now: NaiveDateTime, fire_secs: i64) -> Option<NaiveDate> {
    let today = now.date();
    [today, today - chrono::Duration::days(1)]
        .into_iter()
        .find(|&date| {
            let since = (now - fire_moment(date, fire_secs)).num_seconds();
            (0..FIRE_GRACE_SECS).contains(&since)
        })
}

/// One scheduler round; `true` when it started a quiz.
async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<bool> {
    let tz: Tz = ctx.config.schedule.timezone;
    let local_now = chrono::Utc::now().with_timezone(&tz);
    let local_date = local_now.date_naive();
    let now = local_now.naive_local();
    let offset = max_reminder(ctx);
    let mut started = false;

    for quiz_time in &ctx.config.schedule.quiz_times {
        let (qh, qm) = (quiz_time.hour, quiz_time.minute);
        // The slot's key in `last_quiz_dates`, as "HH:MM".
        let time_str = &quiz_time.to_string();

        // Fire `offset` seconds before the quiz so the reminder lands on time.
        let fire_secs = fire_secs(qh, qm, offset);
        let Some(fire_date) = due_fire_date(now, fire_secs) else {
            continue;
        };

        // Already fired this slot?
        if ctx
            .state
            .lock()
            .await
            .last_quiz_dates
            .get(time_str.as_str())
            == Some(&fire_date)
        {
            continue;
        }

        // Another quiz round already running? Retried on the next tick
        // while the grace window lasts.
        if ctx.quiz_run_lock.try_lock().is_err() {
            warn!("Scheduler: slot {time_str} is due but a quiz is already in progress, waiting");
            continue;
        }

        // Marked before starting so a restart can't fire it twice.
        {
            let mut state = ctx.state.lock().await;
            state.last_quiz_dates.insert(time_str.clone(), fire_date);
            if let Err(e) = state.save(&ctx.state_path).await {
                error!("Failed to persist last_quiz_dates: {e}");
            }
        }

        info!(
            "Scheduled quiz firing for slot {time_str} (fire at {}, quiz at {qh}:{qm:02})",
            fire_moment(fire_date, fire_secs).format("%H:%M"),
        );
        let ctx2 = ctx.clone();
        let client2 = client.clone();
        let slot = time_str.clone();
        started = true;
        tokio::spawn(async move {
            if let Err(e) = crate::quiz::start_quiz(ctx2, client2, false, Some(slot), None).await {
                error!("Quiz error: {e}");
            }
        });
    }

    // ── One-time quizzes (!schedulequiz) ─────────────────────────────────────
    let once_entries: Vec<ScheduledOnce> = ctx.state.lock().await.scheduled_once.clone();

    for entry in once_entries {
        let Some((qh, qm)) = ScheduleConfig::parse_quiz_time(&entry.quiz_time) else {
            warn!(
                "Invalid scheduled_once time {:?}, removing",
                entry.quiz_time
            );
            remove_once(ctx, &entry).await;
            continue;
        };

        // `entry.date` is the day of the fire moment (see `!schedulequiz`).
        let fire_secs = fire_secs(qh, qm, offset);
        let since = (now - fire_moment(entry.date, fire_secs)).num_seconds();
        if since < 0 {
            continue;
        }
        // Removed before spawning to prevent double-fire on restart, and
        // also once its time has passed while the bot was offline, so it
        // doesn't linger in the list forever.
        remove_once(ctx, &entry).await;
        if since >= FIRE_GRACE_SECS {
            warn!(
                "One-time quiz at {} on {} was missed (bot offline?): removed",
                entry.quiz_time, entry.date
            );
            continue;
        }

        if ctx.quiz_run_lock.try_lock().is_err() {
            warn!(
                "One-time quiz at {} would fire now but a quiz is already running: dropped",
                entry.quiz_time,
            );
            continue;
        }

        info!("One-time quiz firing for {}", entry.quiz_time);
        started = true;
        let ctx2 = ctx.clone();
        let client2 = client.clone();
        tokio::spawn(async move {
            // skip_reminder = false → full reminder flow; slot_key = None → no last_quiz_dates entry.
            if let Err(e) = crate::quiz::start_quiz(ctx2, client2, false, None, None).await {
                error!("One-time quiz error: {e}");
            }
        });
    }

    // Let quizzes crossing midnight finish before freezing the previous month.
    if local_date.day() > 1 || local_now.hour() >= 1 {
        post_previous_month(ctx, client, tz, local_date).await?;
    }

    Ok(started)
}

async fn remove_once(ctx: &BotContext, entry: &ScheduledOnce) {
    let mut state = ctx.state.lock().await;
    state.scheduled_once.retain(|e| e != entry);
    if let Err(e) = state.save(&ctx.state_path).await {
        error!("Failed to persist scheduled_once: {e}");
    }
}

async fn post_previous_month(
    ctx: &BotContext,
    client: &Client,
    tz: Tz,
    local_date: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let month = crate::leaderboard::YearMonth::previous(local_date);
    let period = month.period();
    let transaction_id = format!("quiz-monthly-leaderboard-{period}");

    if !ctx
        .db
        .try_claim_monthly_post(&period, &transaction_id)
        .await?
    {
        return Ok(());
    }

    let result = async {
        let (start, end) = month.utc_bounds(tz)?;
        let entries = ctx.db.monthly_leaderboard(start, end).await?;
        let question_count = ctx.db.question_count_between(start, end).await?;
        let room = client
            .get_room(&ctx.room_id)
            .ok_or_else(|| anyhow::anyhow!("bot is not in leaderboard room"))?;
        let txn_id: OwnedTransactionId = transaction_id.clone().into();
        let event_id = mxbot_common::send::send_with_retry_txn(
            &room,
            crate::leaderboard::monthly_content(month, question_count, &entries),
            &txn_id,
        )
        .await?;
        ctx.db
            .complete_monthly_post(&period, event_id.as_str())
            .await?;
        info!(
            period,
            participants = entries.len(),
            "Posted monthly leaderboard"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(error) = result {
        if let Err(release_error) = ctx.db.release_monthly_post(&period).await {
            error!(
                "Monthly leaderboard {period} failed: {error}; \
                 additionally failed to release claim: {release_error}"
            );
        } else {
            warn!("Monthly leaderboard {period} failed; will retry: {error}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(date: &str, time: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn day(date: &str) -> NaiveDate {
        date.parse().unwrap()
    }

    #[test]
    fn fires_within_the_grace_window_not_only_in_the_exact_minute() {
        let fire = fire_secs(20, 0, 300); // quiz 20:00, reminder 19:55
        assert_eq!(due_fire_date(at("2026-09-25", "19:54:59"), fire), None);
        assert_eq!(
            due_fire_date(at("2026-09-25", "19:55:00"), fire),
            Some(day("2026-09-25"))
        );
        assert_eq!(
            due_fire_date(at("2026-09-25", "19:59:30"), fire),
            Some(day("2026-09-25"))
        );
        assert_eq!(due_fire_date(at("2026-09-25", "20:00:00"), fire), None);
    }

    #[test]
    fn the_next_fire_is_the_first_slot_after_now() {
        let fires = || [fire_secs(20, 0, 300), fire_secs(12, 0, 0)].into_iter();
        assert_eq!(
            next_fire_after(at("2026-09-25", "10:00:00"), fires()),
            Some(at("2026-09-25", "12:00:00"))
        );
        assert_eq!(
            next_fire_after(at("2026-09-25", "19:55:00"), fires()),
            Some(at("2026-09-26", "12:00:00")),
            "strictly after"
        );
        assert_eq!(
            next_fire_after(at("2026-09-25", "10:00:00"), [].into_iter()),
            None
        );
    }

    #[test]
    fn a_window_crossing_midnight_belongs_to_the_previous_day() {
        let fire = fire_secs(0, 0, 120); // quiz 00:00, reminder 23:58
        assert_eq!(
            due_fire_date(at("2026-09-26", "00:01:00"), fire),
            Some(day("2026-09-25"))
        );
        assert_eq!(
            due_fire_date(at("2026-09-25", "23:58:10"), fire),
            Some(day("2026-09-25"))
        );
    }
}
